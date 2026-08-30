//! The per-session agent loop: reads events, runs turns against a provider,
//! dispatches tool calls, and records the transcript in the session store.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bosun_common::config::SubagentConfig;
use bosun_common::error::ErrorExt;
use bosun_common::session::Block;
use bosun_common::session::Message;
use bosun_common::session::Role;
use bosun_common::session::SessionState;
use bosun_common::tool::ToolDelta;
use bosun_common::tool::ToolSpec;
use bosun_common::tool::canonical_tools;
use futures_util::StreamExt;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use crate::provider::ProviderCall;
use crate::provider::StreamEvent;
use crate::skills::Skill;
use crate::skills::fetch_working_skills;
use crate::skills::injected_skills;
use crate::skills::merge_skills;
use crate::skills::read_injected_skill;
use crate::skills::read_working_skill;

/// Caps the summarizer output so a compaction stays cheap.
const MAX_TOKENS: u32 = 2048;

/// Caps the subagent loop at this many turns, so a subagent that never
/// finishes cannot hold the parent's turn forever.
const MAX_SUBAGENT_TURNS: usize = 20;

const SUMMARIZATION_PROMPT: &str = "Summarize the conversation so far. Preserve: \
     decisions, file paths, commands run, tool results that still matter, and any \
     open questions. Be concise.";

pub enum LoopEvent {
    Wake,
    Interrupt,
}

pub trait DeltaSink: Send + Sync {
    fn send(&self, text: String);
}

pub trait ToolExecutor: Send + Sync {
    fn call(
        &self,
        session_id: String,
        run_id: String,
        name: String,
        args: Value,
        delta: mpsc::UnboundedSender<ToolDelta>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolError>> + Send>>;

    fn cancel(
        &self,
        session_id: String,
        run_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send>>;
}

#[derive(Clone)]
pub struct ToolOutcome {
    pub content: Value,
    pub is_error: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool call failed")]
    Internal(#[from] anyhow::Error),
}

pub struct LoopDeps {
    pub store: bosun_store::store::Store,
    pub provider: Arc<dyn crate::provider::Provider>,
    pub tools: Arc<dyn ToolExecutor>,
    pub delta_sink: Arc<dyn DeltaSink>,
    /// Non-archived messages allowed before compaction triggers.
    pub max_window_messages: usize,
    /// The control plane's injected skills directory, when one is configured.
    pub injected_skills_dir: Option<PathBuf>,
    /// Configured subagent types, keyed by type name.
    pub subagent_configs: HashMap<String, SubagentConfig>,
    /// Providers for subagent models, keyed by model name.
    pub providers: HashMap<String, Arc<dyn crate::provider::Provider>>,
    /// Price per million input tokens, used to meter model-call cost.
    pub price_input_per_mtok: f64,
    /// Price per million output tokens, used to meter model-call cost.
    pub price_output_per_mtok: f64,
}

/// One model call's cost in dollars: the per-million-token prices times the
/// token counts, rounded to six decimals. Missing token counts cost zero.
fn model_call_cost(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    price_input_per_mtok: f64,
    price_output_per_mtok: f64,
) -> f64 {
    let cost = input_tokens.unwrap_or(0) as f64 / 1e6 * price_input_per_mtok
        + output_tokens.unwrap_or(0) as f64 / 1e6 * price_output_per_mtok;
    (cost * 1e6).round() / 1e6
}

pub struct LoopHandle {
    pub sender: mpsc::UnboundedSender<LoopEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

impl LoopHandle {
    pub fn send(&self, event: LoopEvent) {
        let _ = self.sender.send(event);
    }

    pub fn stop(self) {
        drop(self.sender);
        self.task.abort();
    }
}

/// Start one loop task for the session. The task logs its own errors.
pub fn spawn_loop(session_id: String, deps: Arc<LoopDeps>) -> LoopHandle {
    let (sender, mut rx) = mpsc::unbounded_channel::<LoopEvent>();
    let task = tokio::spawn(async move {
        let result = async {
            let mut todos = Vec::new();
            // Wakes that arrive while a turn is in flight are counted here and
            // consumed by the next turn, so a user message posted mid-turn is
            // still processed once the current batch of turns ends.
            let pending_wakes = Arc::new(AtomicU64::new(0));
            loop {
                if pending_wakes.load(Ordering::Acquire) > 0 {
                    handle_wake(&deps, &session_id, &mut todos, &mut rx, &pending_wakes).await?;
                    continue;
                }
                match rx.recv().await {
                    None => break,
                    Some(LoopEvent::Wake) => {
                        pending_wakes.fetch_add(1, Ordering::AcqRel);
                        handle_wake(&deps, &session_id, &mut todos, &mut rx, &pending_wakes)
                            .await?;
                    }
                    // This arm is only reachable while no turn is in flight:
                    // handle_wake owns the channel (and cancels the in-flight
                    // turn) for the whole duration of a turn. An interrupt
                    // here is not a killed turn, so it is ignored.
                    Some(LoopEvent::Interrupt) => {
                        debug!(
                            msg = "ignoring interrupt: no turn is in flight",
                            session_id = %session_id
                        );
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(error) = result {
            error!(
                msg = "agent loop failed",
                session_id = %session_id,
                error = %error.display_chain()
            );
        }
    });
    LoopHandle { sender, task }
}

/// The interrupt channel between the event select and the in-flight turn: the
/// flag carries the interrupt across gaps between selects, the notify wakes
/// the select that is currently awaiting.
struct InterruptSignal {
    flag: AtomicBool,
    notify: Notify,
}

impl InterruptSignal {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn interrupt(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

enum TurnOutcome {
    Finished,
    ToolCalls,
    AskedUser,
    Interrupted,
    Failed,
}

#[derive(Default)]
struct AccumulatedToolCall {
    id: Option<String>,
    name: Option<String>,
    args_delta: String,
}

/// Runs turns until one ends the session: no more tool calls, an ask, an
/// interrupt, or a failure. Each turn gets a fresh [`InterruptSignal`]; the
/// channel is polled alongside the turn so an interrupt reaches it mid-flight.
async fn handle_wake(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    todos: &mut Vec<Value>,
    rx: &mut mpsc::UnboundedReceiver<LoopEvent>,
    pending_wakes: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    // Consume one pending wake; wakes that arrived mid-turn keep the loop
    // running after this batch of turns ends.
    pending_wakes.fetch_sub(1, Ordering::AcqRel);
    deps.store
        .set_state(session_id, SessionState::Running)
        .await?;

    let mut interrupted = false;
    loop {
        let signal = Arc::new(InterruptSignal::new());
        let outcome = {
            let mut turn = Box::pin(run_turn(deps, session_id, todos, &signal));
            loop {
                tokio::select! {
                    biased;
                    outcome = &mut turn => break outcome,
                    event = rx.recv() => match event {
                        Some(LoopEvent::Interrupt) => {
                            deps.store.set_state(session_id, SessionState::Interrupted).await?;
                            signal.interrupt();
                            interrupted = true;
                        }
                        Some(LoopEvent::Wake) => {
                            debug!(
                                msg = "queuing a wake that arrived mid-turn",
                                session_id = %session_id
                            );
                            pending_wakes.fetch_add(1, Ordering::AcqRel);
                        }
                        None => return Ok(()),
                    },
                }
            }
        };
        match outcome {
            TurnOutcome::ToolCalls if !interrupted => {}
            TurnOutcome::Finished | TurnOutcome::AskedUser => {
                deps.store
                    .set_state(session_id, SessionState::WaitingForInput)
                    .await?;
                return Ok(());
            }
            TurnOutcome::ToolCalls | TurnOutcome::Interrupted | TurnOutcome::Failed => {
                deps.store
                    .set_state(session_id, SessionState::Interrupted)
                    .await?;
                return Ok(());
            }
        }
    }
}

async fn run_turn(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    todos: &mut Vec<Value>,
    signal: &Arc<InterruptSignal>,
) -> TurnOutcome {
    match run_turn_inner(deps, session_id, todos, signal).await {
        Ok(outcome) => outcome,
        Err(error) => {
            error!(
                msg = "turn failed",
                session_id = %session_id,
                error = %error.display_chain()
            );
            TurnOutcome::Failed
        }
    }
}

async fn run_turn_inner(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    todos: &mut Vec<Value>,
    signal: &Arc<InterruptSignal>,
) -> anyhow::Result<TurnOutcome> {
    let messages: Vec<Message> = maybe_compact(deps, session_id, signal)
        .await?
        .into_iter()
        .map(|(_, message)| message)
        .filter(|message| {
            // An ask tool call has no matching tool result in the transcript:
            // the Ask block replaces it, so the provider would reject the
            // dangling tool_use on the next turn.
            !matches!(&message.block, Block::ToolCall { name, .. } if name == "ask")
        })
        .collect();

    let session = deps
        .store
        .get_session(session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
    let permission = session.permission;
    // Skills are discovered fresh each turn, so a working copy that gains or
    // drops a skill advertises the change on the next request.
    let working_skills = fetch_working_skills(&*deps.tools, session_id)
        .await
        .unwrap_or_else(|error| {
            warn!(
                msg = "failed to fetch skills from the node",
                session_id = %session_id,
                error = %error.display_chain()
            );
            Vec::new()
        });
    let injected_skills = injected_skills(deps.injected_skills_dir.as_deref());
    let skills = merge_skills(working_skills.clone(), injected_skills.clone());
    let tools: Vec<ToolSpec> = canonical_tools(permission)
        .into_iter()
        .filter(|tool| {
            // spawn_subagent is advertised only when subagent types are
            // configured, so a control plane without subagents never offers it.
            tool.name != "spawn_subagent" || !deps.subagent_configs.is_empty()
        })
        .collect();

    let system = system_prompt(todos, &skills);
    let mut stream = deps.provider.chat_stream(ProviderCall {
        model: deps.provider.model(),
        max_tokens: 4096,
        system: &system,
        messages,
        tools,
    })?;

    let mut text = String::new();
    let mut tool_calls = BTreeMap::<usize, AccumulatedToolCall>::new();
    let mut stopped = false;

    loop {
        if signal.flag.load(Ordering::Acquire) {
            return Ok(TurnOutcome::Interrupted);
        }
        tokio::select! {
            event = stream.next() => match event {
                Some(Ok(StreamEvent::TextDelta(delta))) => {
                    text.push_str(&delta);
                    deps.delta_sink.send(delta);
                }
                Some(Ok(StreamEvent::ToolCallDelta { index, id, name, args_delta })) => {
                    let call = tool_calls.entry(index).or_default();
                    if call.id.is_none() {
                        call.id = id;
                    }
                    if call.name.is_none() {
                        call.name = name;
                    }
                    call.args_delta.push_str(&args_delta);
                }
                Some(Ok(StreamEvent::Stop { input_tokens, output_tokens })) => {
                    deps.store
                        .append_model_call(
                            session_id,
                            deps.provider.model(),
                            deps.provider.name(),
                            "completion",
                            Some(input_tokens),
                            Some(output_tokens),
                            Some(model_call_cost(
                                Some(input_tokens),
                                Some(output_tokens),
                                deps.price_input_per_mtok,
                                deps.price_output_per_mtok,
                            )),
                        )
                        .await?;
                    stopped = true;
                }
                Some(Err(error)) => {
                    error!(
                        msg = "provider stream failed",
                        session_id = %session_id,
                        provider = %deps.provider.name(),
                        error = %error.display_chain()
                    );
                    return Ok(TurnOutcome::Failed);
                }
                None => break,
            },
            _ = signal.notify.notified() => {
                if signal.flag.load(Ordering::Acquire) {
                    return Ok(TurnOutcome::Interrupted);
                }
            }
        }
    }

    if !stopped {
        error!(
            msg = "provider stream ended without a stop event",
            session_id = %session_id,
            provider = %deps.provider.name()
        );
        return Ok(TurnOutcome::Failed);
    }

    if !text.is_empty() {
        deps.store
            .append_message(session_id, Role::Assistant, &Block::Text { text })
            .await?;
    }

    let calls: Vec<(String, String, Value)> = tool_calls
        .into_values()
        .map(|call| {
            let args = serde_json::from_str(&call.args_delta).unwrap_or_else(|error| {
                warn!(
                    msg = "tool call arguments are not valid JSON",
                    session_id = %session_id,
                    error = %error.display_chain()
                );
                Value::Null
            });
            (
                call.id.unwrap_or_default(),
                call.name.unwrap_or_default(),
                args,
            )
        })
        .collect();

    if calls.is_empty() {
        return Ok(TurnOutcome::Finished);
    }

    for (id, name, args) in calls {
        // Commit each tool call to the transcript just before dispatching it,
        // so calls after an ask or a mid-turn interrupt never leave a phantom
        // tool_use without a result.
        deps.store
            .append_message(
                session_id,
                Role::Assistant,
                &Block::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                },
            )
            .await?;
        deps.store
            .append_tool_call(session_id, &id, &name, &args)
            .await?;

        match name.as_str() {
            "" => {
                warn!(
                    msg = "tool call has no name",
                    session_id = %session_id,
                    call_id = %id
                );
                deps.store
                    .complete_tool_call(session_id, &id, &json!({ "error": "unknown tool" }), true)
                    .await?;
                deps.store
                    .append_message(
                        session_id,
                        Role::User,
                        &Block::ToolResult {
                            id,
                            name,
                            is_error: true,
                            content: json!({ "error": "unknown tool" }),
                        },
                    )
                    .await?;
            }
            "ask" => {
                let message = args["message"].as_str().unwrap_or_default().to_string();
                let options = args["options"]
                    .as_array()
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|value| value.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                deps.store
                    .complete_tool_call(session_id, &id, &json!({ "asked": true }), false)
                    .await?;
                deps.store
                    .append_message(
                        session_id,
                        Role::Assistant,
                        &Block::Ask {
                            message,
                            options,
                            answer: None,
                        },
                    )
                    .await?;
                return Ok(TurnOutcome::AskedUser);
            }
            "todowrite" => {
                match args["items"].as_array() {
                    Some(items) => *todos = items.clone(),
                    None => warn!(
                        msg = "todowrite items are not an array",
                        session_id = %session_id
                    ),
                }
                deps.store
                    .complete_tool_call(session_id, &id, &json!({ "ok": true }), false)
                    .await?;
                deps.store
                    .append_message(
                        session_id,
                        Role::User,
                        &Block::ToolResult {
                            id,
                            name,
                            is_error: false,
                            content: json!({ "ok": true }),
                        },
                    )
                    .await?;
            }
            "skill" => {
                let skill_name = args["name"].as_str().unwrap_or_default();
                let content = if working_skills.iter().any(|skill| skill.name == skill_name) {
                    match read_working_skill(&*deps.tools, session_id, skill_name).await {
                        Ok(Some(markdown)) => Some(json!({ "content": markdown })),
                        Ok(None) => {
                            warn!(
                                msg = "the node does not know a skill it listed",
                                session_id = %session_id,
                                skill = %skill_name
                            );
                            None
                        }
                        Err(error) => {
                            warn!(
                                msg = "failed to read the skill's instructions from the node",
                                session_id = %session_id,
                                skill = %skill_name,
                                error = %error.display_chain()
                            );
                            None
                        }
                    }
                } else if injected_skills.iter().any(|skill| skill.name == skill_name) {
                    read_injected_skill(deps.injected_skills_dir.as_deref(), skill_name)
                        .map(|markdown| json!({ "content": markdown }))
                } else {
                    None
                };
                let (content, is_error) = match content {
                    Some(content) => (content, false),
                    None => (json!({ "error": "skill not found" }), true),
                };
                deps.store
                    .complete_tool_call(session_id, &id, &content, is_error)
                    .await?;
                deps.store
                    .append_message(
                        session_id,
                        Role::User,
                        &Block::ToolResult {
                            id,
                            name,
                            is_error,
                            content,
                        },
                    )
                    .await?;
            }
            "spawn_subagent" => {
                let subagent_type = args["subagent_type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let instructions = args["instructions"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let Some(config) = deps.subagent_configs.get(&subagent_type) else {
                    let content =
                        json!({ "error": format!("unknown subagent type {subagent_type}") });
                    deps.store
                        .complete_tool_call(session_id, &id, &content, true)
                        .await?;
                    deps.store
                        .append_message(
                            session_id,
                            Role::User,
                            &Block::ToolResult {
                                id,
                                name,
                                is_error: true,
                                content,
                            },
                        )
                        .await?;
                    continue;
                };
                let Some(provider) = deps.providers.get(&config.model).cloned() else {
                    let content =
                        json!({ "error": format!("no provider for model {}", config.model) });
                    deps.store
                        .complete_tool_call(session_id, &id, &content, true)
                        .await?;
                    deps.store
                        .append_message(
                            session_id,
                            Role::User,
                            &Block::ToolResult {
                                id,
                                name,
                                is_error: true,
                                content,
                            },
                        )
                        .await?;
                    continue;
                };
                deps.store
                    .append_message(
                        session_id,
                        Role::Assistant,
                        &Block::Subagent {
                            subagent_type: subagent_type.clone(),
                            status: "started".into(),
                            text: instructions.clone(),
                        },
                    )
                    .await?;
                match run_subagent(
                    deps,
                    session_id,
                    &subagent_type,
                    config,
                    provider,
                    &instructions,
                    signal,
                )
                .await
                {
                    Ok(summary) => {
                        deps.store
                            .append_message(
                                session_id,
                                Role::Assistant,
                                &Block::Subagent {
                                    subagent_type: subagent_type.clone(),
                                    status: "done".into(),
                                    text: summary.clone(),
                                },
                            )
                            .await?;
                        let content = json!({ "summary": summary });
                        deps.store
                            .complete_tool_call(session_id, &id, &content, false)
                            .await?;
                        deps.store
                            .append_message(
                                session_id,
                                Role::User,
                                &Block::ToolResult {
                                    id,
                                    name,
                                    is_error: false,
                                    content,
                                },
                            )
                            .await?;
                    }
                    Err(_) => {
                        let content = json!({ "error": "subagent failed" });
                        deps.store
                            .complete_tool_call(session_id, &id, &content, true)
                            .await?;
                        deps.store
                            .append_message(
                                session_id,
                                Role::User,
                                &Block::ToolResult {
                                    id,
                                    name,
                                    is_error: true,
                                    content,
                                },
                            )
                            .await?;
                    }
                }
            }
            _ => {
                let run_id = Uuid::new_v4().to_string();
                debug!(
                    msg = "dispatching tool call",
                    session_id = %session_id,
                    tool = %name,
                    run_id = %run_id
                );
                let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<ToolDelta>();
                let mut call = deps.tools.call(
                    session_id.to_string(),
                    run_id.clone(),
                    name.clone(),
                    args,
                    delta_tx,
                );
                let outcome = loop {
                    if signal.flag.load(Ordering::Acquire) {
                        return Ok(TurnOutcome::Interrupted);
                    }
                    tokio::select! {
                        result = &mut call => {
                            break match result {
                                Ok(outcome) => outcome,
                                Err(error) => {
                                    error!(
                                        msg = "tool call failed",
                                        session_id = %session_id,
                                        tool = %name,
                                        run_id = %run_id,
                                        error = %error.display_chain()
                                    );
                                    ToolOutcome {
                                        content: json!({ "error": "tool call failed" }),
                                        is_error: true,
                                    }
                                }
                            };
                        }
                        delta = delta_rx.recv() => {
                            if let Some(delta) = delta {
                                deps.delta_sink.send(delta.text);
                            }
                        }
                        _ = signal.notify.notified() => {
                            if signal.flag.load(Ordering::Acquire) {
                                if let Err(error) = deps
                                    .tools
                                    .cancel(session_id.to_string(), run_id.clone())
                                    .await
                                {
                                    warn!(
                                        msg = "failed to cancel tool call",
                                        session_id = %session_id,
                                        tool = %name,
                                        run_id = %run_id,
                                        error = %error.display_chain()
                                    );
                                }
                                return Ok(TurnOutcome::Interrupted);
                            }
                        }
                    }
                };
                drop(call);
                // The outcome can overtake the tool's final deltas, so drain
                // whatever the select left in the channel before recording.
                while let Ok(delta) = delta_rx.try_recv() {
                    deps.delta_sink.send(delta.text);
                }
                deps.store
                    .complete_tool_call(session_id, &id, &outcome.content, outcome.is_error)
                    .await?;
                deps.store
                    .append_message(
                        session_id,
                        Role::User,
                        &Block::ToolResult {
                            id,
                            name,
                            is_error: outcome.is_error,
                            content: outcome.content,
                        },
                    )
                    .await?;
            }
        }
    }

    Ok(TurnOutcome::ToolCalls)
}

/// Runs the subagent's nested loop: its own window, its own tool list (node
/// tools only), its own model and permission. Every streamed message and tool
/// call is written to the session store as a Subagent block, so the parent's
/// transcript shows the subagent's work. Returns the subagent's accumulated
/// text as the summary the parent reports to its model.
async fn run_subagent(
    deps: &LoopDeps,
    session_id: &str,
    subagent_type: &str,
    config: &SubagentConfig,
    provider: Arc<dyn crate::provider::Provider>,
    instructions: &str,
    interrupt: &InterruptSignal,
) -> Result<String, anyhow::Error> {
    // The preamble rides in the first user message, the same way the parent's
    // system prompt would, so the subagent needs no separate system text.
    let mut window = vec![Message {
        role: Role::User,
        block: Block::Text {
            text: format!(
                "You are a subagent of type {subagent_type} working on the task below. \
                 You share the session's working copy. Make the requested change directly \
                 using your tools. Do not ask questions; finish on your own.\n\n{instructions}"
            ),
        },
    }];
    // Node-side tools only: the subagent never asks the user, rewrites the
    // session todo list, loads skills, or spawns further subagents.
    let tools: Vec<ToolSpec> = canonical_tools(config.permission)
        .into_iter()
        .filter(|tool| {
            !matches!(
                tool.name.as_str(),
                "ask" | "todowrite" | "skill" | "spawn_subagent"
            )
        })
        .collect();
    let mut summary = String::new();

    for _ in 0..MAX_SUBAGENT_TURNS {
        let mut stream = provider.chat_stream(ProviderCall {
            model: &config.model,
            max_tokens: MAX_TOKENS,
            system: "",
            messages: window.clone(),
            tools: tools.clone(),
        })?;

        let mut text = String::new();
        let mut tool_calls = BTreeMap::<usize, AccumulatedToolCall>::new();
        let mut stopped = false;

        loop {
            if interrupt.flag.load(Ordering::Acquire) {
                return Err(anyhow::anyhow!("subagent interrupted"));
            }
            tokio::select! {
                event = stream.next() => match event {
                    Some(Ok(StreamEvent::TextDelta(delta))) => {
                        text.push_str(&delta);
                        deps.delta_sink.send(delta);
                    }
                    Some(Ok(StreamEvent::ToolCallDelta { index, id, name, args_delta })) => {
                        let call = tool_calls.entry(index).or_default();
                        if call.id.is_none() {
                            call.id = id;
                        }
                        if call.name.is_none() {
                            call.name = name;
                        }
                        call.args_delta.push_str(&args_delta);
                    }
                    Some(Ok(StreamEvent::Stop { input_tokens, output_tokens })) => {
                        deps.store
                            .append_model_call(
                                session_id,
                                &config.model,
                                provider.name(),
                                "completion",
                                Some(input_tokens),
                                Some(output_tokens),
                                Some(model_call_cost(
                                    Some(input_tokens),
                                    Some(output_tokens),
                                    deps.price_input_per_mtok,
                                    deps.price_output_per_mtok,
                                )),
                            )
                            .await?;
                        stopped = true;
                    }
                    Some(Err(error)) => {
                        let error = anyhow::Error::new(error);
                        error!(
                            msg = "subagent provider stream failed",
                            session_id = %session_id,
                            subagent_type = %subagent_type,
                            provider = %provider.name(),
                            error = %error.display_chain()
                        );
                        return Err(error);
                    }
                    None => break,
                },
                _ = interrupt.notify.notified() => {
                    if interrupt.flag.load(Ordering::Acquire) {
                        return Err(anyhow::anyhow!("subagent interrupted"));
                    }
                }
            }
        }

        if !stopped {
            error!(
                msg = "subagent provider stream ended without a stop event",
                session_id = %session_id,
                subagent_type = %subagent_type,
                provider = %provider.name()
            );
            return Err(anyhow::anyhow!(
                "subagent provider stream ended without a stop event"
            ));
        }

        if !text.is_empty() {
            summary.push_str(&text);
            window.push(Message {
                role: Role::Assistant,
                block: Block::Text { text: text.clone() },
            });
            deps.store
                .append_message(
                    session_id,
                    Role::Assistant,
                    &Block::Subagent {
                        subagent_type: subagent_type.to_string(),
                        status: "message".into(),
                        text,
                    },
                )
                .await?;
        }

        let calls: Vec<(String, String, Value)> = tool_calls
            .into_values()
            .map(|call| {
                let args = serde_json::from_str(&call.args_delta).unwrap_or_else(|error| {
                    warn!(
                        msg = "subagent tool call arguments are not valid JSON",
                        session_id = %session_id,
                        subagent_type = %subagent_type,
                        error = %error.display_chain()
                    );
                    Value::Null
                });
                (
                    call.id.unwrap_or_default(),
                    call.name.unwrap_or_default(),
                    args,
                )
            })
            .collect();

        if calls.is_empty() {
            return Ok(summary.trim().to_string());
        }

        for (id, name, args) in calls {
            window.push(Message {
                role: Role::Assistant,
                block: Block::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                },
            });
            deps.store
                .append_message(
                    session_id,
                    Role::Assistant,
                    &Block::Subagent {
                        subagent_type: subagent_type.to_string(),
                        status: "tool".into(),
                        text: format!("{name} {args}"),
                    },
                )
                .await?;
            deps.store
                .append_tool_call(session_id, &id, &name, &args)
                .await?;

            let run_id = Uuid::new_v4().to_string();
            let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<ToolDelta>();
            let mut call = deps.tools.call(
                session_id.to_string(),
                run_id.clone(),
                name.clone(),
                args,
                delta_tx,
            );
            let outcome = loop {
                if interrupt.flag.load(Ordering::Acquire) {
                    if let Err(error) = deps
                        .tools
                        .cancel(session_id.to_string(), run_id.clone())
                        .await
                    {
                        warn!(
                            msg = "failed to cancel subagent tool call",
                            session_id = %session_id,
                            subagent_type = %subagent_type,
                            tool = %name,
                            run_id = %run_id,
                            error = %error.display_chain()
                        );
                    }
                    return Err(anyhow::anyhow!("subagent interrupted"));
                }
                tokio::select! {
                    result = &mut call => {
                        break match result {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                error!(
                                    msg = "subagent tool call failed",
                                    session_id = %session_id,
                                    subagent_type = %subagent_type,
                                    tool = %name,
                                    run_id = %run_id,
                                    error = %error.display_chain()
                                );
                                ToolOutcome {
                                    content: json!({ "error": "tool call failed" }),
                                    is_error: true,
                                }
                            }
                        };
                    }
                    delta = delta_rx.recv() => {
                        if let Some(delta) = delta {
                            deps.delta_sink.send(delta.text);
                        }
                    }
                    _ = interrupt.notify.notified() => {
                        if interrupt.flag.load(Ordering::Acquire) {
                            if let Err(error) = deps
                                .tools
                                .cancel(session_id.to_string(), run_id.clone())
                                .await
                            {
                                warn!(
                                    msg = "failed to cancel subagent tool call",
                                    session_id = %session_id,
                                    subagent_type = %subagent_type,
                                    tool = %name,
                                    run_id = %run_id,
                                    error = %error.display_chain()
                                );
                            }
                            return Err(anyhow::anyhow!("subagent interrupted"));
                        }
                    }
                }
            };
            drop(call);
            // The outcome can overtake the tool's final deltas, so drain
            // whatever the select left in the channel before recording.
            while let Ok(delta) = delta_rx.try_recv() {
                deps.delta_sink.send(delta.text);
            }
            deps.store
                .complete_tool_call(session_id, &id, &outcome.content, outcome.is_error)
                .await?;
            window.push(Message {
                role: Role::User,
                block: Block::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    is_error: outcome.is_error,
                    content: outcome.content.clone(),
                },
            });
            deps.store
                .append_message(
                    session_id,
                    Role::User,
                    &Block::Subagent {
                        subagent_type: subagent_type.to_string(),
                        status: "tool_result".into(),
                        text: outcome.content.to_string(),
                    },
                )
                .await?;
        }
    }

    warn!(
        msg = "subagent exceeded its turn limit",
        session_id = %session_id,
        subagent_type = %subagent_type
    );
    Err(anyhow::anyhow!("subagent exceeded its turn limit"))
}

/// Compacts the non-archived transcript when it exceeds
/// `max_window_messages`: the retired tail is summarized by the provider,
/// archived in the store, and replaced by a Summary message. Returns the
/// window for the turn. On a summarizer failure or interrupt the store is
/// left untouched and the full window is returned.
async fn maybe_compact(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    signal: &Arc<InterruptSignal>,
) -> anyhow::Result<Vec<(i64, Message)>> {
    let window = deps.store.messages(session_id, false).await?;
    if window.len() <= deps.max_window_messages {
        return Ok(window);
    }
    let keep = deps.max_window_messages / 2;
    let split = window.len() - keep;
    let tail = &window[..split];
    let tail_last_id = window[split - 1].0;

    let Some((text, input_tokens, output_tokens)) =
        summarize_tail(deps, session_id, tail, signal).await
    else {
        return Ok(window);
    };

    deps.store
        .append_message(session_id, Role::Assistant, &Block::Summary { text })
        .await?;
    deps.store.mark_archived(session_id, tail_last_id).await?;
    deps.store
        .append_model_call(
            session_id,
            deps.provider.model(),
            deps.provider.name(),
            "compaction",
            input_tokens,
            output_tokens,
            Some(model_call_cost(
                input_tokens,
                output_tokens,
                deps.price_input_per_mtok,
                deps.price_output_per_mtok,
            )),
        )
        .await?;
    info!(
        session_id = %session_id,
        retired = split,
        "compacted transcript"
    );
    Ok(deps.store.messages(session_id, false).await?)
}

/// Asks the provider to summarize the retired tail: the instruction plus the
/// rendered messages as one user message. Returns the summary text and the
/// token counts when the stream ended with a Stop; returns None on a stream
/// error, a missing Stop, or an interrupt, logging the reason.
async fn summarize_tail(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    tail: &[(i64, Message)],
    signal: &Arc<InterruptSignal>,
) -> Option<(String, Option<u64>, Option<u64>)> {
    let mut prompt = String::from(SUMMARIZATION_PROMPT);
    for (_, message) in tail {
        prompt.push_str(&format!(
            "\n\n{}: {}",
            role_label(message.role),
            render_block(&message.block)
        ));
    }
    let messages = vec![Message {
        role: Role::User,
        block: Block::Text { text: prompt },
    }];
    let mut stream = match deps.provider.chat_stream(ProviderCall {
        model: deps.provider.model(),
        max_tokens: MAX_TOKENS,
        system: "",
        messages,
        tools: vec![],
    }) {
        Ok(stream) => stream,
        Err(error) => {
            warn!(
                msg = "summarizer request failed",
                session_id = %session_id,
                provider = %deps.provider.name(),
                error = %error.display_chain()
            );
            return None;
        }
    };

    let mut text = String::new();
    let mut input_tokens = None;
    let mut output_tokens = None;
    let mut stopped = false;

    loop {
        if signal.flag.load(Ordering::Acquire) {
            warn!(
                msg = "interrupt during compaction",
                session_id = %session_id
            );
            return None;
        }
        tokio::select! {
            event = stream.next() => match event {
                Some(Ok(StreamEvent::TextDelta(delta))) => text.push_str(&delta),
                // A summarizer that calls tools contributes no text.
                Some(Ok(StreamEvent::ToolCallDelta { .. })) => {}
                Some(Ok(StreamEvent::Stop { input_tokens: input, output_tokens: output })) => {
                    input_tokens = Some(input);
                    output_tokens = Some(output);
                    stopped = true;
                }
                Some(Err(error)) => {
                    warn!(
                        msg = "summarizer stream failed",
                        session_id = %session_id,
                        provider = %deps.provider.name(),
                        error = %error.display_chain()
                    );
                    return None;
                }
                None => break,
            },
            _ = signal.notify.notified() => {
                if signal.flag.load(Ordering::Acquire) {
                    warn!(
                        msg = "interrupt during compaction",
                        session_id = %session_id
                    );
                    return None;
                }
            }
        }
    }

    if !stopped {
        warn!(
            msg = "summarizer stream ended without a stop event",
            session_id = %session_id,
            provider = %deps.provider.name()
        );
        return None;
    }
    Some((text, input_tokens, output_tokens))
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// One message as plain text for the summarizer.
fn render_block(block: &Block) -> String {
    match block {
        Block::Text { text } => text.clone(),
        Block::ToolCall { id, name, args } => format!("tool call {name} (id {id}): {args}"),
        Block::ToolResult {
            id,
            name,
            is_error,
            content,
        } => format!("tool result {name} (id {id}, is_error {is_error}): {content}"),
        Block::Ask {
            message, options, ..
        } => format!(
            "question to user: {message} (options: {})",
            options.join(", ")
        ),
        Block::Summary { text } => format!("summary: {text}"),
        Block::Subagent {
            subagent_type,
            status,
            text,
        } => format!("subagent {subagent_type} ({status}): {text}"),
    }
}

fn system_prompt(todos: &[Value], skills: &[Skill]) -> String {
    let mut prompt = String::from(
        "You are Bosun, an autonomous software engineering agent. \
         You work in a session working copy. Use the provided tools to inspect \
         and modify code. Prefer the simplest change that works. Keep replies \
         concise and literal. Ask the user only when a decision requires them.",
    );
    if !skills.is_empty() {
        prompt.push_str("\n\nSkills available in this session:");
        for skill in skills {
            prompt.push_str(&format!("\n- {}: {}", skill.name, skill.description));
        }
    }
    if !todos.is_empty() {
        prompt.push_str("\n\nCurrent todo list:");
        for (index, todo) in todos.iter().enumerate() {
            let content = todo["content"].as_str().unwrap_or_default();
            let status = todo["status"].as_str().unwrap_or_default();
            prompt.push_str(&format!("\n{index}. [{status}] {content}"));
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use bosun_common::session::Permission;
    use bosun_common::session::Session;
    use bosun_common::session::SessionState;
    use bosun_common::tool::ToolDelta;
    use bosun_store::store::Store;
    use futures_util::StreamExt;
    use futures_util::stream;
    use futures_util::stream::BoxStream;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    use super::*;
    use crate::provider::Provider;
    use crate::provider::ProviderCall;
    use crate::provider::ProviderError;
    use crate::provider::StreamEvent;

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            node: "node-1".to_string(),
            repo_url: None,
            git_ref: None,
            dir: "/work".to_string(),
            model: "mock-model".to_string(),
            permission: Permission::ReadWrite,
            state: SessionState::Creating,
            created_at_secs: 1_700_000_000,
            prompt: None,
        }
    }

    fn read_only_session(id: &str) -> Session {
        Session {
            permission: Permission::ReadOnly,
            ..session(id)
        }
    }

    /// One request the loop sent the provider, captured for assertions.
    #[derive(Debug, Clone)]
    struct CapturedCall {
        model: String,
        system: String,
        messages: Vec<Message>,
        tools: Vec<ToolSpec>,
    }

    /// One scripted `chat_stream` answer, item by item.
    type Script = Vec<Result<StreamEvent, ProviderError>>;

    /// Answers each `chat_stream` call with the next script and records the
    /// request, so a test can drive several turns and inspect what the loop
    /// sent (system prompt and transcript window).
    struct ScriptedProvider {
        scripts: Arc<Mutex<VecDeque<Script>>>,
        calls: Arc<Mutex<Vec<CapturedCall>>>,
        delay: Duration,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
            Self::with_delay(scripts, Duration::ZERO)
        }

        /// Spaces every streamed event by `delay`, so a turn stays in flight
        /// long enough for a test to interleave other loop events.
        fn with_delay(scripts: Vec<Vec<StreamEvent>>, delay: Duration) -> Self {
            let scripts: VecDeque<Script> = scripts
                .into_iter()
                .map(|script| script.into_iter().map(Ok).collect())
                .collect();
            Self {
                scripts: Arc::new(Mutex::new(scripts)),
                calls: Arc::new(Mutex::new(Vec::new())),
                delay,
            }
        }

        /// Scripts the raw stream items, so a test can make a call fail.
        fn with_results(scripts: Vec<Script>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
                calls: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::ZERO,
            }
        }

        fn captured_calls(&self) -> Vec<CapturedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn model(&self) -> &str {
            "mock-model"
        }

        fn chat_stream<'a>(
            &'a self,
            call: ProviderCall<'a>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .expect("no script left for the provider call");
            self.calls.lock().unwrap().push(CapturedCall {
                model: call.model.to_string(),
                system: call.system.to_string(),
                messages: call.messages.clone(),
                tools: call.tools.clone(),
            });
            let delay = self.delay;
            let stream = stream::iter(script).then(move |item| {
                let delay = delay;
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    item
                }
            });
            Ok(stream.boxed())
        }
    }

    struct BlockingProvider;

    impl Provider for BlockingProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn model(&self) -> &str {
            "mock-model"
        }

        fn chat_stream<'a>(
            &'a self,
            _call: ProviderCall<'a>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            let pending: BoxStream<'static, Result<StreamEvent, ProviderError>> =
                stream::pending().boxed();
            Ok(pending)
        }
    }

    #[derive(Debug, Clone)]
    struct CapturedToolCall {
        session_id: String,
        run_id: String,
        name: String,
        args: Value,
    }

    /// Records every call and cancel, and either streams one delta and
    /// completes, or never completes so a test can interrupt it.
    struct MockTools {
        outcome: ToolOutcome,
        /// Canned outcomes for specific tool names; other tools get `outcome`.
        outcomes: HashMap<String, ToolOutcome>,
        delta_text: Option<String>,
        block: bool,
        calls: Arc<Mutex<Vec<CapturedToolCall>>>,
        cancels: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockTools {
        fn new(outcome: ToolOutcome) -> Self {
            Self {
                outcome,
                outcomes: HashMap::new(),
                delta_text: None,
                block: false,
                calls: Arc::new(Mutex::new(Vec::new())),
                cancels: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Serves a canned outcome for one tool name.
        fn serving(mut self, name: &str, outcome: ToolOutcome) -> Self {
            self.outcomes.insert(name.to_string(), outcome);
            self
        }

        /// Streams one delta before the call completes.
        fn streaming(mut self, text: &str) -> Self {
            self.delta_text = Some(text.to_string());
            self
        }

        /// Never completes, so an interrupt reaches the in-flight call.
        /// Served per-name outcomes still complete: the skills plumbing must
        /// not hang the turn it runs in.
        fn blocking(mut self) -> Self {
            self.block = true;
            self
        }
    }

    impl ToolExecutor for MockTools {
        fn call(
            &self,
            session_id: String,
            run_id: String,
            name: String,
            args: Value,
            delta: mpsc::UnboundedSender<ToolDelta>,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolError>> + Send>> {
            let served = self.outcomes.contains_key(&name);
            let outcome = self
                .outcomes
                .get(&name)
                .cloned()
                .unwrap_or_else(|| self.outcome.clone());
            self.calls.lock().unwrap().push(CapturedToolCall {
                session_id,
                run_id,
                name,
                args,
            });
            let delta_text = self.delta_text.clone();
            let block = self.block && !served;
            Box::pin(async move {
                if let Some(text) = delta_text {
                    let _ = delta.send(ToolDelta { text });
                }
                if block {
                    // Hold the sender so the loop parks on the delta channel
                    // and the interrupt wakes its select deterministically.
                    let _hold = delta;
                    std::future::pending::<Result<ToolOutcome, ToolError>>().await
                } else {
                    drop(delta);
                    Ok(outcome)
                }
            })
        }

        fn cancel(
            &self,
            session_id: String,
            run_id: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send>> {
            self.cancels.lock().unwrap().push((session_id, run_id));
            Box::pin(async move { Ok(()) })
        }
    }

    struct CollectSink(Arc<Mutex<Vec<String>>>);

    impl DeltaSink for CollectSink {
        fn send(&self, text: String) {
            self.0.lock().unwrap().push(text);
        }
    }

    async fn wait_for<F, Fut>(what: &str, mut condition: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if condition().await {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn default_outcome() -> ToolOutcome {
        ToolOutcome {
            content: json!({ "ok": true }),
            is_error: false,
        }
    }

    fn test_deps(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
    ) -> LoopDeps {
        // Existing tests never fill a window, so compaction stays off.
        test_deps_with_max_window(store, provider, tools, sink, usize::MAX)
    }

    fn test_deps_with_max_window(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
        max_window_messages: usize,
    ) -> LoopDeps {
        test_deps_with_prices(store, provider, tools, sink, max_window_messages, 0.0, 0.0)
    }

    fn test_deps_with_prices(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
        max_window_messages: usize,
        price_input_per_mtok: f64,
        price_output_per_mtok: f64,
    ) -> LoopDeps {
        LoopDeps {
            store: store.clone(),
            provider,
            tools,
            delta_sink: sink,
            max_window_messages,
            injected_skills_dir: None,
            subagent_configs: HashMap::new(),
            providers: HashMap::new(),
            price_input_per_mtok,
            price_output_per_mtok,
        }
    }

    /// A deps with configured subagent types and their providers; existing
    /// tests never fill the window, so compaction stays off.
    fn test_deps_with_subagents(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
        subagent_configs: HashMap<String, SubagentConfig>,
        providers: HashMap<String, Arc<dyn Provider>>,
    ) -> LoopDeps {
        LoopDeps {
            store: store.clone(),
            provider,
            tools,
            delta_sink: sink,
            max_window_messages: usize::MAX,
            injected_skills_dir: None,
            subagent_configs,
            providers,
            price_input_per_mtok: 0.0,
            price_output_per_mtok: 0.0,
        }
    }

    #[tokio::test]
    async fn wake_streams_text_commits_a_message_and_waits_for_input() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s1")).await.unwrap();

        let sink = Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new()))));
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("hello".into()),
            StreamEvent::Stop {
                input_tokens: 5,
                output_tokens: 2,
            },
        ]]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            sink.clone(),
        ));
        let handle = spawn_loop("s1".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the streamed text to reach the sink", || async {
            sink.0.lock().unwrap().iter().any(|text| text == "hello")
        })
        .await;

        wait_for("the assistant text message to be stored", || {
            let store = store.clone();
            async move {
                let messages = store.messages("s1", false).await.unwrap();
                messages.iter().any(|(_, message)| {
                    message.role == Role::Assistant
                        && matches!(&message.block, Block::Text { text } if text == "hello")
                })
            }
        })
        .await;

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s1").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let calls = store.model_calls("s1").await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input_tokens, Some(5));
        assert_eq!(calls[0].output_tokens, Some(2));
        assert_eq!(calls[0].model, "mock-model");
        assert_eq!(calls[0].provider, "mock");

        let captured = provider.captured_calls();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].model, "mock-model");

        handle.stop();
    }

    #[tokio::test]
    async fn model_calls_record_cost_from_tokens_and_prices() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-cost")).await.unwrap();

        // One million tokens at $3/$15 per million: $3.00 + $15.00 = $18.00.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::Stop {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
            },
        ]]));
        let deps = Arc::new(test_deps_with_prices(
            &store,
            provider,
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            usize::MAX,
            3.0,
            15.0,
        ));
        let handle = spawn_loop("s-cost".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-cost").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let calls = store.model_calls("s-cost").await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input_tokens, Some(1_000_000));
        assert_eq!(calls[0].output_tokens, Some(1_000_000));
        assert_eq!(calls[0].cost, Some(18.0));

        handle.stop();
    }

    #[tokio::test]
    async fn interrupt_during_a_turn_marks_the_session_interrupted() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s2")).await.unwrap();

        let deps = Arc::new(test_deps(
            &store,
            Arc::new(BlockingProvider),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s2".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the turn to start running", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s2").await.unwrap().unwrap();
                stored.state == SessionState::Running
            }
        })
        .await;

        handle.send(LoopEvent::Interrupt);

        wait_for("the session to be interrupted", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s2").await.unwrap().unwrap();
                stored.state == SessionState::Interrupted
            }
        })
        .await;

        handle.stop();
    }

    #[tokio::test]
    async fn ask_ends_the_turn_and_waits() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-ask")).await.unwrap();

        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"continue?","options":["yes","no"]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 3,
                    output_tokens: 2,
                },
            ]])),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-ask".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-ask").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-ask", false).await.unwrap();
        assert!(
            messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::ToolCall { id, name, .. } if id == "call-1" && name == "ask"
            )),
            "the ask tool call is recorded"
        );
        let (message, options, answer) = messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    options,
                    answer,
                } => Some((message, options, answer)),
                _ => None,
            })
            .expect("an ask block is recorded");
        assert_eq!(message.as_str(), "continue?");
        assert_eq!(options.as_slice(), ["yes", "no"]);
        assert!(answer.is_none());

        let calls = store.model_calls("s-ask").await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input_tokens, Some(3));
        assert_eq!(calls[0].output_tokens, Some(2));

        let tool_calls = store.tool_calls("s-ask").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "ask");
        assert_eq!(tool_calls[0].result, Some(json!({ "asked": true })));
        assert!(!tool_calls[0].is_error);

        handle.stop();
    }

    #[tokio::test]
    async fn todowrite_updates_the_todo_state() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-todo")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("todowrite".into()),
                    args_delta: r#"{"items":[{"id":"1","content":"write tests","status":"todo"},{"id":"2","content":"fix the bug","status":"in_progress"}]}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 5,
                    output_tokens: 3,
                },
            ],
            vec![
                StreamEvent::TextDelta("working on it".into()),
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-todo".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the second turn's system prompt to carry the todo list", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move {
                    let calls = provider.captured_calls();
                    calls.len() == 2 && calls[1].system.contains("Current todo list")
                }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert!(
            !calls[0].system.contains("Current todo list"),
            "the first turn has no todos yet"
        );
        assert!(calls[1].system.contains("0. [todo] write tests"));
        assert!(calls[1].system.contains("1. [in_progress] fix the bug"));

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-todo").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-todo", false).await.unwrap();
        let result = messages
            .iter()
            .find(|(_, message)| matches!(&message.block, Block::ToolResult { name, .. } if name == "todowrite"))
            .expect("a todowrite result is recorded");
        assert_eq!(result.1.role, Role::User);
        let (id, name, is_error, content) = match &result.1.block {
            Block::ToolResult {
                id,
                name,
                is_error,
                content,
            } => (id, name, is_error, content),
            _ => unreachable!(),
        };
        assert_eq!(id.as_str(), "call-1");
        assert_eq!(name.as_str(), "todowrite");
        assert!(!is_error);
        assert_eq!(content, &json!({ "ok": true }));

        let tool_calls = store.tool_calls("s-todo").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "todowrite");
        assert_eq!(tool_calls[0].result, Some(json!({ "ok": true })));
        assert!(!tool_calls[0].is_error);

        assert_eq!(store.model_calls("s-todo").await.unwrap().len(), 2);

        handle.stop();
    }

    #[tokio::test]
    async fn skills_are_advertised_and_loadable() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-skill")).await.unwrap();

        let markdown = "---\nname: my-skill\ndescription: Does things\n---\n\nBody text";
        // The node's executor answers the internal skills plumbing: listing
        // the working copy's skills and reading one skill's instructions.
        let tools = Arc::new(
            MockTools::new(default_outcome())
                .serving(
                    "skills",
                    ToolOutcome {
                        content: json!({ "skills": [
                            { "name": "my-skill", "description": "Does things" }
                        ] }),
                        is_error: false,
                    },
                )
                .serving(
                    "skill/read",
                    ToolOutcome {
                        content: json!({ "content": markdown }),
                        is_error: false,
                    },
                ),
        );

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("skill".into()),
                    args_delta: r#"{"name":"my-skill"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("loaded".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-skill".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-skill").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 2, "the skill result starts a second turn");
        assert!(
            calls[0].tools.iter().any(|tool| tool.name == "skill"),
            "the skill tool is advertised"
        );
        assert!(
            calls[0].system.contains("my-skill"),
            "the system prompt advertises the skill name"
        );
        assert!(
            calls[0].system.contains("Does things"),
            "the system prompt carries the skill description"
        );

        let messages = store.messages("s-skill", false).await.unwrap();
        let result = messages
            .iter()
            .find(|(_, message)| {
                matches!(&message.block, Block::ToolResult { name, .. } if name == "skill")
            })
            .expect("a skill result is recorded");
        assert_eq!(result.1.role, Role::User);
        let (id, name, is_error, content) = match &result.1.block {
            Block::ToolResult {
                id,
                name,
                is_error,
                content,
            } => (id, name, is_error, content),
            _ => unreachable!(),
        };
        assert_eq!(id.as_str(), "call-1");
        assert_eq!(name.as_str(), "skill");
        assert!(!is_error);
        assert_eq!(content, &json!({ "content": markdown }));

        let tool_calls = store.tool_calls("s-skill").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "skill");
        assert_eq!(tool_calls[0].result, Some(json!({ "content": markdown })));
        assert!(!tool_calls[0].is_error);

        // The loop listed the working copy's skills through the executor on
        // each turn, and read the skill the model asked for.
        let calls = tools.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|call| call.name == "skills").count(),
            2,
            "the skills list is fetched fresh each turn"
        );
        let read_calls: Vec<&CapturedToolCall> = calls
            .iter()
            .filter(|call| call.name == "skill/read")
            .collect();
        assert_eq!(read_calls.len(), 1);
        assert_eq!(read_calls[0].args, json!({ "name": "my-skill" }));

        handle.stop();
    }

    #[tokio::test]
    async fn an_unknown_skill_reports_an_error_result() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("s-skill-miss"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("skill".into()),
                    args_delta: r#"{"name":"nope"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let tools = Arc::new(MockTools::new(default_outcome()).serving(
            "skills",
            ToolOutcome {
                content: json!({ "skills": [] }),
                is_error: false,
            },
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-skill-miss".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-skill-miss").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-skill-miss", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "skill"
                            && *is_error
                            && content == &json!({ "error": "skill not found" })
                    )),
            "the unknown skill call records an error result"
        );

        let tool_calls = store.tool_calls("s-skill-miss").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert!(tool_calls[0].is_error);
        assert_eq!(
            tool_calls[0].result,
            Some(json!({ "error": "skill not found" }))
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_tool_call_routes_to_the_executor_and_the_result_feeds_back() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-tool")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"cargo build"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 6,
                    output_tokens: 4,
                },
            ],
            vec![
                StreamEvent::TextDelta("build passed".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let sink = Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new()))));
        let tools = Arc::new(
            MockTools::new(ToolOutcome {
                content: json!({ "exit": 0 }),
                is_error: false,
            })
            .streaming("compiling..."),
        );
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            sink.clone(),
        ));
        let handle = spawn_loop("s-tool".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the tool delta to reach the sink", || async {
            sink.0
                .lock()
                .unwrap()
                .iter()
                .any(|text| text == "compiling...")
        })
        .await;

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-tool").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        {
            let calls = tools.calls.lock().unwrap();
            let shell_calls: Vec<&CapturedToolCall> =
                calls.iter().filter(|call| call.name == "shell").collect();
            assert_eq!(shell_calls.len(), 1);
            assert_eq!(shell_calls[0].session_id, "s-tool");
            assert_eq!(shell_calls[0].name, "shell");
            assert_eq!(shell_calls[0].args, json!({ "command": "cargo build" }));
        }

        let messages = store.messages("s-tool", false).await.unwrap();
        assert!(
            messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::ToolCall { id, name, .. } if id == "call-1" && name == "shell"
            )),
            "the shell tool call is recorded"
        );
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "shell"
                            && !is_error
                            && content == &json!({ "exit": 0 })
                    )),
            "the shell result is recorded as a user message"
        );

        let tool_calls = store.tool_calls("s-tool").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "shell");
        assert_eq!(tool_calls[0].args, json!({ "command": "cargo build" }));
        assert_eq!(tool_calls[0].result, Some(json!({ "exit": 0 })));
        assert!(!tool_calls[0].is_error);

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 2, "the tool result starts a second turn");
        assert!(
            calls[1].messages.iter().any(
                |message| matches!(&message.block, Block::ToolCall { name, .. } if name == "shell")
            ),
            "the second turn sees the tool call"
        );
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { name, content, is_error, .. } if name == "shell"
                            && content == &json!({ "exit": 0 })
                            && !is_error
                    )),
            "the second turn sees the tool result"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_user_message_after_waiting_starts_a_new_turn() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-resume")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-resume".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the first turn to end waiting for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-resume").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        store
            .append_message(
                "s-resume",
                Role::User,
                &Block::Text {
                    text: "build it".into(),
                },
            )
            .await
            .unwrap();

        handle.send(LoopEvent::Wake);

        wait_for("the second turn to start", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == "build it")),
            "the second turn sends the user message to the provider"
        );

        wait_for("the session to wait for input again", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-resume").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        handle.stop();
    }

    #[tokio::test]
    async fn a_wake_during_a_turn_runs_another_turn_afterwards() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-midwake")).await.unwrap();

        // The delay keeps the first turn in flight while the test sends the
        // second wake, so the wake has to be queued and consumed afterwards.
        let provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::TextDelta("first".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("second".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(200),
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-midwake".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the first turn to start", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        // The first turn is still in flight: the wake is queued, not dropped.
        handle.send(LoopEvent::Wake);

        wait_for("both turns to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-midwake").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-midwake", false).await.unwrap();
        let texts: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&"first") && texts.contains(&"second"),
            "both turns committed their text: {texts:?}"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn interrupt_during_a_tool_call_cancels_the_tool() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-cancel")).await.unwrap();

        let tools = Arc::new(
            MockTools::new(default_outcome())
                .serving(
                    "skills",
                    ToolOutcome {
                        content: json!({ "skills": [] }),
                        is_error: false,
                    },
                )
                .blocking(),
        );
        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"sleep 100"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 2,
                },
            ]])),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-cancel".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the shell call to be dispatched", {
            let tools = tools.clone();
            move || {
                let tools = tools.clone();
                async move {
                    tools
                        .calls
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|call| call.name == "shell")
                }
            }
        })
        .await;

        // Let the loop park on the in-flight call before interrupting, so the
        // interrupt reaches the parked select instead of a flag check between
        // iterations.
        tokio::time::sleep(Duration::from_millis(20)).await;

        handle.send(LoopEvent::Interrupt);

        wait_for("the tool to be cancelled", {
            let tools = tools.clone();
            move || {
                let tools = tools.clone();
                async move { tools.cancels.lock().unwrap().len() == 1 }
            }
        })
        .await;

        {
            let calls = tools.calls.lock().unwrap();
            let cancels = tools.cancels.lock().unwrap();
            assert_eq!(cancels.len(), 1);
            let shell_call = calls
                .iter()
                .find(|call| call.name == "shell")
                .expect("the shell call was dispatched");
            assert_eq!(cancels[0].0, "s-cancel");
            assert_eq!(cancels[0].1, shell_call.run_id);
        }

        wait_for("the session to be interrupted", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-cancel").await.unwrap().unwrap();
                stored.state == SessionState::Interrupted
            }
        })
        .await;

        handle.stop();
    }

    #[tokio::test]
    async fn a_failed_turn_marks_the_session_interrupted() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-fail")).await.unwrap();

        // The stream ends without a Stop event, so the turn fails.
        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![vec![StreamEvent::TextDelta(
                "partial".into(),
            )]])),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-fail".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to be interrupted", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-fail").await.unwrap().unwrap();
                stored.state == SessionState::Interrupted
            }
        })
        .await;

        // The failed turn commits nothing to the transcript.
        assert!(store.messages("s-fail", false).await.unwrap().is_empty());
        assert!(store.model_calls("s-fail").await.unwrap().is_empty());

        handle.stop();
    }

    #[tokio::test]
    async fn ask_then_other_tool_call_leaves_no_phantom_tool_call() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-ask-shell")).await.unwrap();

        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("ask-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"continue?"}"#.into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("shell-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"ls"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 3,
                    output_tokens: 2,
                },
            ]])),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-ask-shell".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-ask-shell").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-ask-shell", false).await.unwrap();
        let tool_call_names: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_call_names,
            ["ask"],
            "only the ask call is committed; the shell call is never dispatched"
        );
        assert!(
            !messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::ToolResult { name, .. } if name == "shell")),
            "the shell call has no result"
        );
        assert!(
            messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Ask { .. })),
            "the ask block ends the turn"
        );

        let tool_calls = store.tool_calls("s-ask-shell").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "ask-1");
        assert_eq!(tool_calls[0].name, "ask");

        handle.stop();
    }

    #[tokio::test]
    async fn a_tool_call_without_a_name_records_an_error_result_and_continues() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-unknown")).await.unwrap();

        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-x".into()),
                        name: None,
                        args_delta: "{}".into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ])),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-unknown".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-unknown").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-unknown", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { name, is_error, content, .. } if name.is_empty()
                            && *is_error
                            && content == &json!({ "error": "unknown tool" })
                    )),
            "the nameless call records an unknown-tool error result"
        );

        let tool_calls = store.tool_calls("s-unknown").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-x");
        assert!(tool_calls[0].is_error);
        assert_eq!(
            tool_calls[0].result,
            Some(json!({ "error": "unknown tool" }))
        );

        handle.stop();
    }

    #[tokio::test]
    async fn the_ask_tool_call_is_filtered_from_the_next_turns_window() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-filter")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("ask-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"continue?"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-filter".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the ask turn to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-filter").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        store
            .append_message("s-filter", Role::User, &Block::Text { text: "yes".into() })
            .await
            .unwrap();
        handle.send(LoopEvent::Wake);

        wait_for("the second turn to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert!(
            !calls[1].messages.iter().any(
                |message| matches!(&message.block, Block::ToolCall { name, .. } if name == "ask")
            ),
            "the ask tool call must not be sent back to the provider"
        );
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| matches!(&message.block, Block::Ask { .. })),
            "the ask block stays in the window"
        );
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == "yes")),
            "the user's answer is sent to the provider"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_read_only_session_advertises_only_read_tools() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&read_only_session("s-ro"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-ro".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the turn to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        let names: Vec<&str> = calls[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(!names.contains(&"shell"));
        assert!(!names.contains(&"file/write"));
        assert!(!names.contains(&"edit"));
        assert!(names.contains(&"file/read"));
        assert!(names.contains(&"ask"));

        handle.stop();
    }

    #[tokio::test]
    async fn two_tool_calls_in_one_turn_dispatch_in_order_with_distinct_run_ids() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-two")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"first"}"#.into(),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("call-2".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"second"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 4,
                    output_tokens: 2,
                },
            ],
            vec![
                StreamEvent::TextDelta("both ran".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let tools = Arc::new(MockTools::new(default_outcome()));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-two".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-two").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        {
            let calls = tools.calls.lock().unwrap();
            let shell_calls: Vec<&CapturedToolCall> =
                calls.iter().filter(|call| call.name == "shell").collect();
            assert_eq!(shell_calls.len(), 2, "both calls are dispatched");
            assert_eq!(shell_calls[0].args, json!({ "command": "first" }));
            assert_eq!(shell_calls[1].args, json!({ "command": "second" }));
            assert_ne!(
                shell_calls[0].run_id, shell_calls[1].run_id,
                "each call gets its own run id"
            );
        }

        let tool_calls = store.tool_calls("s-two").await.unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[1].call_id, "call-2");
        assert!(tool_calls.iter().all(|call| !call.is_error));

        handle.stop();
    }

    #[tokio::test]
    async fn a_tool_error_result_is_recorded_as_is_error() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-terr")).await.unwrap();

        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("shell".into()),
                        args_delta: r#"{"command":"failing-cmd"}"#.into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 2,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("ok".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ])),
            Arc::new(MockTools::new(ToolOutcome {
                content: json!({ "error": "boom" }),
                is_error: true,
            })),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-terr".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-terr").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-terr", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, is_error, content, .. } if id == "call-1"
                            && *is_error
                            && content == &json!({ "error": "boom" })
                    )),
            "the failing tool result is recorded with is_error true"
        );

        let tool_calls = store.tool_calls("s-terr").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert!(tool_calls[0].is_error);
        assert_eq!(tool_calls[0].result, Some(json!({ "error": "boom" })));

        handle.stop();
    }

    #[tokio::test]
    async fn interrupt_while_idle_leaves_the_session_waiting_for_input() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-parked")).await.unwrap();

        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ]])),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-parked".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the turn to end waiting for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-parked").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        handle.send(LoopEvent::Interrupt);

        // No turn is in flight, so the interrupt kills nothing: the session
        // stays waiting for input.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stored = store.get_session("s-parked").await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::WaitingForInput);

        handle.stop();
    }

    #[tokio::test]
    async fn multi_fragment_args_delta_accumulates_into_one_call() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-frag")).await.unwrap();

        let tools = Arc::new(MockTools::new(default_outcome()));
        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("shell".into()),
                        args_delta: r#"{"command":"cargo "#.into(),
                    },
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: None,
                        name: None,
                        args_delta: r#"build"}"#.into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 3,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ])),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-frag".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-frag").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        {
            let calls = tools.calls.lock().unwrap();
            let shell_calls: Vec<&CapturedToolCall> =
                calls.iter().filter(|call| call.name == "shell").collect();
            assert_eq!(shell_calls.len(), 1);
            assert_eq!(shell_calls[0].name, "shell");
            assert_eq!(shell_calls[0].args, json!({ "command": "cargo build" }));
        }

        let messages = store.messages("s-frag", false).await.unwrap();
        assert!(
            messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::ToolCall { id, name, args } if id == "call-1"
                    && name == "shell"
                    && args == &json!({ "command": "cargo build" })
            )),
            "the committed tool call joins the fragments and keeps the first id and name"
        );

        handle.stop();
    }

    /// Fills the transcript with 100 user+assistant text pairs.
    async fn fill_transcript(store: &Store, session_id: &str) {
        for index in 0..100 {
            store
                .append_message(
                    session_id,
                    Role::User,
                    &Block::Text {
                        text: format!("user {index}"),
                    },
                )
                .await
                .unwrap();
            store
                .append_message(
                    session_id,
                    Role::Assistant,
                    &Block::Text {
                        text: format!("assistant {index}"),
                    },
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn compaction_summarizes_and_archives_the_tail() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-compact")).await.unwrap();
        fill_transcript(&store, "s-compact").await;

        // The first call summarizes the retired tail, the second runs the turn.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("compacted".into()),
                StreamEvent::Stop {
                    input_tokens: 200,
                    output_tokens: 20,
                },
            ],
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Stop {
                    input_tokens: 3,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps_with_prices(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            20,
            3.0,
            15.0,
        ));
        let handle = spawn_loop("s-compact".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-compact").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let all = store.messages("s-compact", true).await.unwrap();
        assert_eq!(
            all.len(),
            202,
            "200 transcript messages plus the summary and the turn's reply"
        );
        assert!(
            all.iter().any(|(_, message)| matches!(
                &message.block,
                Block::Summary { text } if text == "compacted"
            )),
            "the summarizer's text is recorded as a Summary message"
        );

        // The retired tail (190 of 200) is archived: the active window holds
        // only the kept messages, the summary, and the turn's reply.
        let active = store.messages("s-compact", false).await.unwrap();
        assert_eq!(active.len(), 12);
        assert!(active.len() < all.len(), "messages were archived");
        assert!(
            active
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Summary { .. })),
            "the summary stays in the active window"
        );
        assert!(
            active
                .iter()
                .any(|(_, message)| message.role == Role::Assistant
                    && matches!(&message.block, Block::Text { text } if text == "ok")),
            "the turn's reply is recorded"
        );

        // The summarizer call runs before the turn: no tools, tail as messages.
        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 2, "the summarizer call and the turn call");
        assert!(
            calls[0].tools.is_empty() && !calls[0].messages.is_empty(),
            "the summarizer call offers no tools and carries the tail"
        );
        assert!(
            !calls[1].tools.is_empty(),
            "the turn call still offers tools"
        );
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| matches!(&message.block, Block::Summary { .. })),
            "the turn sees the summary in its window"
        );

        let model_calls = store.model_calls("s-compact").await.unwrap();
        assert_eq!(model_calls.len(), 2);
        assert_eq!(model_calls[0].kind, "compaction");
        assert_eq!(model_calls[0].input_tokens, Some(200));
        assert_eq!(model_calls[0].output_tokens, Some(20));
        // 200k input tokens at $3/M and 20k output at $15/M: $0.0009.
        assert_eq!(model_calls[0].cost, Some(0.0009));
        assert_eq!(model_calls[1].kind, "completion");
        // 3k input at $3/M and 1k output at $15/M: $0.000024.
        assert_eq!(model_calls[1].cost, Some(0.000024));

        handle.stop();
    }

    #[tokio::test]
    async fn compaction_is_skipped_when_the_summarizer_fails() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("s-compact-fail"))
            .await
            .unwrap();
        fill_transcript(&store, "s-compact-fail").await;

        // The first call (the summarizer) fails; the second runs the turn.
        let provider = Arc::new(ScriptedProvider::with_results(vec![
            vec![Err(ProviderError::Parse {
                detail: "boom".into(),
            })],
            vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            ],
        ]));
        let deps = Arc::new(test_deps_with_max_window(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            20,
        ));
        let handle = spawn_loop("s-compact-fail".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-compact-fail").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let all = store.messages("s-compact-fail", true).await.unwrap();
        assert!(
            !all.iter()
                .any(|(_, message)| matches!(&message.block, Block::Summary { .. })),
            "no summary is appended when the summarizer fails"
        );
        assert_eq!(
            all.len(),
            201,
            "the 200 transcript messages plus the turn's reply"
        );
        assert!(
            all.iter()
                .any(|(_, message)| message.role == Role::Assistant
                    && matches!(&message.block, Block::Text { text } if text == "ok")),
            "the turn still completes"
        );

        // Only the turn's completion is metered; the failed summarizer is not.
        let model_calls = store.model_calls("s-compact-fail").await.unwrap();
        assert_eq!(model_calls.len(), 1);
        assert_eq!(model_calls[0].kind, "completion");

        handle.stop();
    }

    #[tokio::test]
    async fn spawn_subagent_runs_a_nested_loop_and_reports_the_summary() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-subagent")).await.unwrap();

        // One queue serves every chat_stream call: the parent's first turn
        // spawns the subagent, the subagent's turn streams its work, and the
        // parent's second turn wraps up with the summary in its window.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn_subagent".into()),
                    args_delta: r#"{"subagent_type":"coder","instructions":"add a test"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 3,
                    output_tokens: 2,
                },
            ],
            vec![
                StreamEvent::TextDelta("made the change".into()),
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps_with_subagents(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                SubagentConfig {
                    model: "mock-model".into(),
                    permission: Permission::ReadWrite,
                },
            )]),
            HashMap::from([(
                "mock-model".to_string(),
                provider.clone() as Arc<dyn Provider>,
            )]),
        ));
        let handle = spawn_loop("s-subagent".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-subagent").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-subagent", false).await.unwrap();
        let subagent_blocks: Vec<(&str, &str)> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Subagent { status, text, .. } => Some((status.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            subagent_blocks,
            [
                ("started", "add a test"),
                ("message", "made the change"),
                ("done", "made the change"),
            ],
            "the subagent's activity appears in the session transcript"
        );

        let result = messages
            .iter()
            .find(|(_, message)| {
                matches!(&message.block, Block::ToolResult { name, .. } if name == "spawn_subagent")
            })
            .expect("a spawn_subagent result is recorded");
        assert_eq!(result.1.role, Role::User);
        let (id, name, is_error, content) = match &result.1.block {
            Block::ToolResult {
                id,
                name,
                is_error,
                content,
            } => (id, name, is_error, content),
            _ => unreachable!(),
        };
        assert_eq!(id.as_str(), "call-1");
        assert_eq!(name.as_str(), "spawn_subagent");
        assert!(!is_error);
        assert_eq!(
            content["summary"].as_str(),
            Some("made the change"),
            "the summary is the subagent's text: {content}"
        );

        let tool_calls = store.tool_calls("s-subagent").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "spawn_subagent");
        assert_eq!(
            tool_calls[0].result,
            Some(json!({ "summary": "made the change" }))
        );
        assert!(!tool_calls[0].is_error);

        // The three calls are: parent, subagent, parent.
        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 3);
        let subagent_call = &calls[1];
        assert_eq!(subagent_call.model, "mock-model");
        let tool_names: Vec<&str> = subagent_call
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        for excluded in ["ask", "todowrite", "skill", "spawn_subagent"] {
            assert!(
                !tool_names.contains(&excluded),
                "the subagent sees no {excluded} tool: {tool_names:?}"
            );
        }
        assert!(tool_names.contains(&"shell"));
        let first = &subagent_call.messages[0];
        assert_eq!(first.role, Role::User);
        let first_text = match &first.block {
            Block::Text { text } => text,
            _ => unreachable!("the subagent window starts with a text message"),
        };
        assert!(first_text.contains("subagent of type coder"));
        assert!(first_text.contains("add a test"));

        let model_calls = store.model_calls("s-subagent").await.unwrap();
        assert_eq!(model_calls.len(), 3, "parent, subagent, and parent again");
        assert_eq!(model_calls[1].model, "mock-model");
        assert_eq!(model_calls[1].kind, "completion");

        handle.stop();
    }

    #[tokio::test]
    async fn an_unknown_subagent_type_reports_an_error_result() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("s-subagent-miss"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn_subagent".into()),
                    args_delta: r#"{"subagent_type":"nope","instructions":"do it"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps_with_subagents(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                SubagentConfig {
                    model: "mock-model".into(),
                    permission: Permission::ReadWrite,
                },
            )]),
            HashMap::new(),
        ));
        let handle = spawn_loop("s-subagent-miss".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-subagent-miss").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-subagent-miss", false).await.unwrap();
        assert!(
            !messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Subagent { .. })),
            "no subagent activity is recorded for an unknown type"
        );
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "spawn_subagent"
                            && *is_error
                            && content == &json!({ "error": "unknown subagent type nope" })
                    )),
            "the unknown type call records an error result"
        );

        let tool_calls = store.tool_calls("s-subagent-miss").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "spawn_subagent");
        assert!(tool_calls[0].is_error);
        assert_eq!(
            tool_calls[0].result,
            Some(json!({ "error": "unknown subagent type nope" }))
        );

        handle.stop();
    }
}
