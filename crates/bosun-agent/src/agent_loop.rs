//! The per-session agent loop: reads events, runs turns against a provider,
//! dispatches tool calls, and records the transcript in the session store.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use bosun_common::config::PersonaConfig;
use bosun_common::error::ErrorExt;
use bosun_common::session::Block;
use bosun_common::session::ChildEventKind;
use bosun_common::session::InterruptCause;
use bosun_common::session::Message;
use bosun_common::session::Role;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::tool::ToolDelta;
use bosun_common::tool::ToolSpec;
use bosun_common::tool::canonical_tools;
use bosun_common::tool::parse_allowed_tools;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use crate::provider::AskRecipient;
use crate::provider::ProviderCall;
use crate::provider::ProviderError;
use crate::provider::StreamEvent;
use crate::skills::Skill;
use crate::skills::fetch_working_skills;
use crate::skills::merge_skills;
use crate::skills::read_injected_skill;
use crate::skills::read_working_skill;
use crate::standards::fetch_repo_standards;

/// The session's skills, discovered once and reused across turns.
struct SessionSkills {
    working: Vec<Skill>,
    injected: Vec<Skill>,
    merged: Vec<Skill>,
}

/// Caps the summarizer output so a compaction stays cheap.
const MAX_TOKENS: u32 = 2048;

const SUMMARIZATION_PROMPT: &str = "Summarize the conversation so far. Preserve: \
     decisions, file paths, commands run, tool results that still matter, and any \
     open questions. Be concise.";

pub enum LoopEvent {
    /// A turn should run: a user message or a child's authored event was
    /// appended to this session's thread.
    Wake,
    /// The user sent a message to this session: the message was appended to
    /// this session's thread. Starts a turn even when the session is
    /// interrupted, which is how the owner of a tree resumes it.
    UserMessage,
    /// A parent's `message_child` directed at this session: the message was
    /// appended to this session's thread. Starts a turn even when the session
    /// is stopped or interrupted, which is how a child resumes.
    ParentMessage,
    Interrupt,
}

/// Why a wake is queued while a turn is in flight. The kind is kept so a
/// stopped or interrupted session can still be woken by a user message or a
/// parent message while plain wakes on it run nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeKind {
    Turn,
    UserMessage,
    ParentMessage,
}

/// Sends loop events to another session's loop: the control plane routes
/// through the registry, and tests route through direct handles. None leaves
/// delivery silent, for a lone loop in tests.
pub trait LoopMailbox: Send + Sync {
    fn send(&self, session_id: &str, event: LoopEvent);
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

/// One child session the `spawn` tool asks for: the parent session, the
/// persona the child runs under, and the assignment that becomes the child's
/// first user message.
#[derive(Clone)]
pub struct SpawnChild {
    pub parent: Session,
    pub persona_name: String,
    pub persona: PersonaConfig,
    pub instructions: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The child could not be started; the message says why.
    #[error("{0}")]
    Failed(String),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// Creates a child session and starts its loop, returning the child's id
/// once its executor is up. The control plane implements this; the loop calls
/// it from the `spawn` tool, so a parent delegates without running the child
/// inside its own turn.
pub trait ChildSpawner: Send + Sync {
    fn spawn(
        &self,
        store: bosun_store::store::Store,
        request: SpawnChild,
    ) -> Pin<Box<dyn Future<Output = Result<String, SpawnError>> + Send>>;
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
    /// Configured personas, keyed by persona name. The `spawn` tool resolves
    /// its target persona from here, and a session's persona prompt is read
    /// from it at every turn.
    pub personas: HashMap<String, PersonaConfig>,
    /// Providers for persona models, keyed by model name.
    pub providers: HashMap<String, Arc<dyn crate::provider::Provider>>,
    /// Per-model metering prices keyed by model name: (input, output). A
    /// model without an entry is metered at the start-time prices below.
    pub prices: HashMap<String, (f64, f64)>,
    /// Price per million input tokens, used to meter model-call cost.
    pub price_input_per_mtok: f64,
    /// Price per million output tokens, used to meter model-call cost.
    pub price_output_per_mtok: f64,
    /// The control plane's child-session spawner. None disables the `spawn`
    /// tool, which the loop advertises only when a spawner is attached.
    pub spawner: Option<Arc<dyn ChildSpawner>>,
    /// Delivery of loop events to other sessions' loops: a child's authored
    /// event wakes its parent, and `message_child` wakes the child it names.
    /// None disables `message_child` and leaves authored events unwoken, for
    /// a lone loop in tests.
    pub mailbox: Option<Arc<dyn LoopMailbox>>,
}

/// State a session's loop keeps across wakes: the todo list, the cached
/// skill and repo-standard lists, and the newest message id in the last
/// wake's snapshot. Events newer than that are unhandled and keep their
/// child in the manifest. The id advances at the start of a wake, so an
/// event is surfaced by exactly one wake even if that wake fails before
/// completing.
#[derive(Default)]
struct LoopState {
    todos: Vec<Value>,
    skills_cache: Option<SessionSkills>,
    /// The repo-standard files present at the working-copy root, fetched once
    /// per session like the skills list. None until the first turn has
    /// fetched; a fetch that fails caches an empty list.
    repo_standards_cache: Option<Vec<String>>,
    surfaced_through: i64,
}

/// One child session in the per-wake manifest: id, persona, state, and its
/// last authored message to this session, when it has authored one.
#[derive(Debug, Clone)]
struct LiveChild {
    id: String,
    persona: Option<String>,
    state: SessionState,
    last_authored: Option<String>,
}

/// The provider and prices one model call runs under: resolved from the
/// session's current model at turn start, so a persona switch changes the
/// provider on the next turn without restarting the loop.
struct TurnModel {
    provider: Arc<dyn crate::provider::Provider>,
    price_input_per_mtok: f64,
    price_output_per_mtok: f64,
}

impl LoopDeps {
    /// The configured provider for `model`, or the loop's own start-time
    /// provider when the control plane has no entry for it (legacy sessions
    /// and tests). Prices follow the same lookup.
    fn turn_model(&self, model: &str) -> TurnModel {
        let provider = self
            .providers
            .get(model)
            .cloned()
            .unwrap_or_else(|| self.provider.clone());
        let (price_input_per_mtok, price_output_per_mtok) = self
            .prices
            .get(model)
            .copied()
            .unwrap_or((self.price_input_per_mtok, self.price_output_per_mtok));
        TurnModel {
            provider,
            price_input_per_mtok,
            price_output_per_mtok,
        }
    }
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
            // Wakes that arrive while a turn is in flight are queued here and
            // consumed by the next wake, so an event that lands mid-turn is
            // still processed once the current batch of turns ends. The kind
            // is kept so a stopped session can still be woken by a parent
            // message.
            let mut pending: VecDeque<WakeKind> = VecDeque::new();
            let mut state = LoopState::default();
            loop {
                let wake = match pending.pop_front() {
                    Some(wake) => wake,
                    None => match rx.recv().await {
                        None => break,
                        Some(LoopEvent::Wake) => WakeKind::Turn,
                        Some(LoopEvent::UserMessage) => WakeKind::UserMessage,
                        Some(LoopEvent::ParentMessage) => WakeKind::ParentMessage,
                        // This arm is only reachable while no turn is in
                        // flight: handle_wake owns the channel (and cancels
                        // the in-flight turn) for the whole duration of a
                        // turn. An interrupt here is not a killed turn, so it
                        // is ignored.
                        Some(LoopEvent::Interrupt) => {
                            debug!(
                                msg = "ignoring interrupt: no turn is in flight",
                                session_id = %session_id
                            );
                            continue;
                        }
                    },
                };
                handle_wake(&deps, &session_id, &mut state, &mut rx, &mut pending, wake).await?;
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
    /// The turn ended without tool calls. `text` is the turn's own assistant
    /// text, so a child's completion report carries the final turn's words —
    /// a silent final turn reports empty instead of reusing earlier text.
    Finished {
        text: String,
    },
    ToolCalls,
    /// The turn ended in an `ask`. `question` is the ask's message: a child
    /// ends its wake by authoring it as an Ask event to its parent, a root by
    /// waiting for the user's answer. `origin` names the origin leaf when the
    /// ask re-raised a child's question — the session whose own question the
    /// raise carries; None when the session asked its own question, in which
    /// case the session itself is the origin leaf.
    AskedUser {
        question: String,
        origin: Option<String>,
    },
    Interrupted,
    Failed,
}

#[derive(Default)]
struct AccumulatedToolCall {
    id: Option<String>,
    name: Option<String>,
    args_delta: String,
}

/// How a provider stream ended, before the caller decides what it means.
enum StreamEnd {
    Collected {
        text: String,
        tool_calls: BTreeMap<usize, AccumulatedToolCall>,
        stopped: bool,
    },
    Interrupted,
    Failed(anyhow::Error),
}

/// Runs turns until one ends the session: no more tool calls, an ask, an
/// interrupt, or a failure. Each turn gets a fresh [`InterruptSignal`]; the
/// channel is polled alongside the turn so an interrupt reaches it mid-flight.
async fn handle_wake(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    state: &mut LoopState,
    rx: &mut mpsc::UnboundedReceiver<LoopEvent>,
    pending: &mut VecDeque<WakeKind>,
    wake: WakeKind,
) -> anyhow::Result<()> {
    // A plain wake on a session that has already stopped, or that a user
    // interrupt parked, must not start another turn: a stray wake would run a
    // second completion and author a second event to the parent, or resume a
    // child the user stopped. A crash-interrupted session is let through: its
    // children's failure reports wake it so it can re-decide them after a
    // crash. A user message or a parent's message_child is always let through
    // — those are the resume paths for an interrupted owner and an
    // interrupted or stopped child.
    let stored = deps.store.get_session(session_id).await?;
    let blocked = stored.as_ref().is_some_and(|session| match wake {
        WakeKind::Turn => {
            session.state == SessionState::Stopped
                || (session.state == SessionState::Interrupted
                    && session.interrupt_cause != Some(InterruptCause::Crash))
        }
        WakeKind::UserMessage | WakeKind::ParentMessage => false,
    });
    if blocked {
        return Ok(());
    }
    deps.store
        .set_state(session_id, SessionState::Running)
        .await?;

    // Whether this session is a child decides what a completed wake does: a
    // child ends by reporting to its parent and stopping, a root waits for
    // the user. The tree fields never change mid-wake, so one read covers
    // every turn in it.
    let is_child = stored
        .as_ref()
        .is_some_and(|session| session.parent_id.is_some());

    // The wake's message window is the active thread as it stands now,
    // shared by every turn of the wake: a turn reads it plus the messages
    // the wake itself appends, so a child event or user message that lands
    // mid-wake is invisible to the running turns and surfaces only in its
    // own queued wake.
    let mut window = deps.store.messages(session_id, false).await?;
    // The manifest is built once per wake over the full thread — archived
    // rows included, so a child's last authored message survives
    // compaction. It lists the children whose state or latest authored
    // event this wake's turns are reacting to, and stays fixed for the whole
    // wake, so a child spawned mid-wake appears from the next wake on.
    let messages = deps.store.messages(session_id, true).await?;
    let live = live_children(deps, session_id, state.surfaced_through, &messages).await?;
    state.surfaced_through = window.last().map(|(id, _)| *id).unwrap_or(0);

    let mut interrupted = false;
    loop {
        let signal = Arc::new(InterruptSignal::new());
        let outcome = {
            let mut turn = Box::pin(run_turn(
                deps,
                session_id,
                state,
                &live,
                &mut window,
                &signal,
            ));
            loop {
                tokio::select! {
                    biased;
                    outcome = &mut turn => break outcome,
                    event = rx.recv() => match event {
                        Some(LoopEvent::Interrupt) => {
                            deps.store
                                .mark_interrupted(session_id, InterruptCause::User)
                                .await?;
                            signal.interrupt();
                            interrupted = true;
                        }
                        Some(LoopEvent::Wake) => {
                            debug!(
                                msg = "queuing a wake that arrived mid-turn",
                                session_id = %session_id
                            );
                            pending.push_back(WakeKind::Turn);
                        }
                        Some(LoopEvent::ParentMessage) => {
                            debug!(
                                msg = "queuing a parent message that arrived mid-turn",
                                session_id = %session_id
                            );
                            pending.push_back(WakeKind::ParentMessage);
                        }
                        Some(LoopEvent::UserMessage) => {
                            debug!(
                                msg = "queuing a user message that arrived mid-turn",
                                session_id = %session_id
                            );
                            pending.push_back(WakeKind::UserMessage);
                        }
                        None => return Ok(()),
                    },
                }
            }
        };
        match outcome {
            TurnOutcome::ToolCalls if !interrupted => {}
            // A child that finished reports to its parent and stops — unless
            // it still has live children of its own. A child that spawned is
            // supervising: like a root waiting for the user, it waits for its
            // children's events, and reports to its parent only once nothing
            // it spawned can still act or holds an unhandled event.
            TurnOutcome::Finished { text } if is_child && !interrupted => {
                if live_children(
                    deps,
                    session_id,
                    state.surfaced_through,
                    &deps.store.messages(session_id, true).await?,
                )
                .await?
                .is_empty()
                {
                    author_child_event(
                        &deps.store,
                        deps.mailbox.as_deref(),
                        session_id,
                        ChildEventKind::Report,
                        text,
                        None,
                    )
                    .await?;
                    deps.store
                        .set_state(session_id, SessionState::Stopped)
                        .await?;
                    return Ok(());
                }
                deps.store
                    .set_state(session_id, SessionState::WaitingForInput)
                    .await?;
                return Ok(());
            }
            // A child that asked ends its wake by authoring the Ask event to
            // its parent and waiting for the parent's answer, denial, or
            // redirection. The event carries the origin leaf — the child
            // itself when it asked its own question, the leaf of the question
            // it re-raised when it surfaced a child's question upward — so
            // every level above binds the question to that leaf without ever
            // resolving it from transcripts. The question stays in its own
            // thread so the parent's later message resumes it with the
            // question in context.
            TurnOutcome::AskedUser { question, origin } if is_child && !interrupted => {
                author_child_event(
                    &deps.store,
                    deps.mailbox.as_deref(),
                    session_id,
                    ChildEventKind::Ask,
                    question,
                    Some(origin.clone().unwrap_or_else(|| session_id.to_string())),
                )
                .await?;
                deps.store
                    .set_state(session_id, SessionState::WaitingForInput)
                    .await?;
                return Ok(());
            }
            TurnOutcome::Finished { .. } | TurnOutcome::AskedUser { .. } => {
                deps.store
                    .set_state(session_id, SessionState::WaitingForInput)
                    .await?;
                return Ok(());
            }
            // A turn that fails on its own interrupts the session as a crash:
            // no user request stopped it. A child that crashed reports the
            // failure to its parent, which re-decides it — resume it with
            // message_child, or abandon it. A turn that ended under a user
            // interrupt already recorded its cause when the interrupt landed,
            // so a plain state write keeps it.
            TurnOutcome::Failed if !interrupted => {
                deps.store
                    .mark_interrupted(session_id, InterruptCause::Crash)
                    .await?;
                if is_child {
                    author_child_event(
                        &deps.store,
                        deps.mailbox.as_deref(),
                        session_id,
                        ChildEventKind::Failure,
                        CRASH_FAILURE_TEXT.to_string(),
                        None,
                    )
                    .await?;
                }
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

/// The text a child authors to its parent when a crash interrupts it. The
/// parent reads the failure event and re-decides the child: resume it with
/// `message_child`, or abandon it.
pub const CRASH_FAILURE_TEXT: &str =
    "a crash stopped my turn and I am stopped; resume me or abandon me";

/// Authors a child's event into its parent's thread — a completion report, a
/// question to the parent, or a failure notice — and wakes the parent's loop.
/// `origin` is the origin leaf an ask event carries (None for reports and
/// failures). The append can fail when the parent is gone; the child's own
/// state is the caller's to set. When the parent has an unresolved raised ask
/// bound to this child, the child's event proves that question is closed — a
/// child authors nothing while its question awaits an answer — so the
/// parent's pending row is cleared and the parent may raise again. The
/// control plane calls this from boot recovery to report crash-interrupted
/// children; the loop calls it from a child's own wake.
pub async fn author_child_event(
    store: &bosun_store::store::Store,
    mailbox: Option<&dyn LoopMailbox>,
    child_id: &str,
    kind: ChildEventKind,
    text: String,
    origin: Option<String>,
) -> anyhow::Result<()> {
    let Some(parent_id) = store
        .get_session(child_id)
        .await?
        .and_then(|session| session.parent_id)
    else {
        return Ok(());
    };
    // The event is authored as a user-role message: in the common case the
    // parent's last message was its own assistant turn, so the user role
    // keeps the next request alternating — after a tool result the event
    // simply follows another user-role message. The block kind still renders
    // it as the child's words.
    if let Err(error) = store
        .append_message(
            &parent_id,
            Role::User,
            &Block::ChildEvent {
                child_id: child_id.to_string(),
                kind,
                text,
                origin,
            },
        )
        .await
    {
        warn!(
            msg = "failed to append the child event to the parent's thread",
            session_id = %child_id,
            parent_id = %parent_id,
            error = %error.display_chain()
        );
        return Ok(());
    }
    if let Ok(Some(pending)) = store.get_pending_ask(&parent_id).await
        && pending.child_id == child_id
        && let Err(error) = store.clear_pending_ask(&parent_id).await
    {
        warn!(
            msg = "failed to clear the parent's resolved raised ask",
            session_id = %child_id,
            parent_id = %parent_id,
            error = %error.display_chain()
        );
    }
    if let Some(mailbox) = mailbox {
        mailbox.send(&parent_id, LoopEvent::Wake);
    }
    info!(
        session_id = %child_id,
        parent_id = %parent_id,
        event = kind.as_str(),
        "child authored an event to its parent"
    );
    Ok(())
}

/// The per-wake manifest of the session's children: id, persona, state, and
/// last authored message. A child is live while it can still act (creating,
/// running, waiting for input, interrupted) or when its latest authored
/// event has not been surfaced by a completed wake yet (stopped, newer than
/// `surfaced_through`). Once a wake has surfaced a stopped child's
/// completion and the parent did not resume it, the child leaves the
/// manifest. `messages` is the session's own thread, which the authored
/// events were appended into.
async fn live_children(
    deps: &LoopDeps,
    session_id: &str,
    surfaced_through: i64,
    messages: &[(i64, Message)],
) -> anyhow::Result<Vec<LiveChild>> {
    let children = deps.store.child_sessions(session_id).await?;
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let mut last_authored: HashMap<&str, (i64, &str)> = HashMap::new();
    for (id, message) in messages {
        if let Block::ChildEvent { child_id, text, .. } = &message.block {
            last_authored.insert(child_id.as_str(), (*id, text.as_str()));
        }
    }
    let mut live = Vec::new();
    for child in children {
        let authored = last_authored.get(child.id.as_str());
        let unhandled = authored.is_some_and(|(id, _)| *id > surfaced_through);
        let can_act = !matches!(child.state, SessionState::Stopped);
        if !(can_act || unhandled) {
            continue;
        }
        live.push(LiveChild {
            id: child.id.clone(),
            persona: child.persona.clone(),
            state: child.state,
            last_authored: authored.map(|(_, text)| (*text).to_string()),
        });
    }
    Ok(live)
}

async fn run_turn(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    state: &mut LoopState,
    live: &[LiveChild],
    window: &mut Vec<(i64, Message)>,
    signal: &Arc<InterruptSignal>,
) -> TurnOutcome {
    match run_turn_inner(deps, session_id, state, live, window, signal).await {
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

/// Appends a message to the store and to the wake's working window, so the
/// wake's later turns read it: the window mirrors the store's active thread
/// minus the messages other writers appended mid-wake. Returns the appended
/// message's row id.
async fn record_in_wake(
    deps: &LoopDeps,
    session_id: &str,
    window: &mut Vec<(i64, Message)>,
    role: Role,
    block: &Block,
) -> anyhow::Result<i64> {
    let id = deps.store.append_message(session_id, role, block).await?;
    window.push((
        id,
        Message {
            role,
            block: block.clone(),
        },
    ));
    Ok(id)
}

async fn run_turn_inner(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    state: &mut LoopState,
    live: &[LiveChild],
    window: &mut Vec<(i64, Message)>,
    signal: &Arc<InterruptSignal>,
) -> anyhow::Result<TurnOutcome> {
    let session = deps
        .store
        .get_session(session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
    let permission = session.permission;
    // Stored data can be edited behind boot validation, so an unparsable
    // allow-list fails the turn closed instead of silently widening the tool
    // surface to every tool.
    let allowed_tools = parse_allowed_tools(&session.allowed_tools)
        .with_context(|| format!("session {session_id} allowed_tools are invalid"))?;
    // The provider and prices are resolved from the session's current model
    // at the start of every turn, so a persona switch applies to the next
    // model call without restarting the loop.
    let turn = deps.turn_model(&session.model);
    // A session's own asks go to the user at the root of the tree and to its
    // parent anywhere below; the recipient never changes mid-session, and
    // ask blocks in the serialized thread render it.
    let ask_recipient = if session.parent_id.is_some() {
        AskRecipient::Parent
    } else {
        AskRecipient::User
    };
    // The wake's own appends since its snapshot are in `window`, so a turn
    // sees the previous turns' tool traffic but nothing that landed mid-wake
    // from another writer. `surfaced_through` is the snapshot boundary: it
    // was fixed when the wake began, so compaction may retire everything at
    // or below it.
    let messages: Vec<Message> = maybe_compact(
        deps,
        &turn,
        session_id,
        signal,
        window,
        state.surfaced_through,
        ask_recipient,
    )
    .await?;
    // A successful ask's tool call has no tool result in the transcript — its
    // Ask block replaced the result — so it is dropped from the window or the
    // provider would reject the dangling tool_use on the next turn. A refused
    // ask records an error tool result and its turn continues, so its tool
    // call stays: the result needs its matching use or the provider rejects
    // the dangling tool_result instead.
    let ask_result_ids: Vec<String> = messages
        .iter()
        .filter_map(|message| match &message.block {
            Block::ToolResult { id, name, .. } if name == "ask" => Some(id.clone()),
            _ => None,
        })
        .collect();
    let messages: Vec<Message> = messages
        .into_iter()
        .filter(|message| {
            !matches!(&message.block, Block::ToolCall { id, name, .. }
                if name == "ask" && !ask_result_ids.contains(id))
        })
        .collect();

    // The working-copy skill list is fetched once per session and cached, so
    // a turn does not round-trip to the node for it. The on-demand `skill`
    // read still goes to the executor when the model asks for it.
    if state.skills_cache.is_none() {
        let working = fetch_working_skills(&*deps.tools, session_id)
            .await
            .unwrap_or_else(|error| {
                warn!(
                    msg = "failed to fetch skills from the node",
                    session_id = %session_id,
                    error = %error.display_chain()
                );
                Vec::new()
            });
        let injected = crate::skills::injected_skills(deps.injected_skills_dir.as_deref());
        let merged = merge_skills(working.clone(), injected.clone());
        state.skills_cache = Some(SessionSkills {
            working,
            injected,
            merged,
        });
    }
    let cached = state.skills_cache.as_ref().expect("populated above");
    let working_skills: &[Skill] = &cached.working;
    let injected_skills: &[Skill] = &cached.injected;
    let skills: &[Skill] = &cached.merged;
    // The repo-standard presence list is fetched once per session and cached,
    // like the skills list: the working copy does not change mid-session, and
    // the files' contents are read on demand with the file tools, so only the
    // presence notice enters the system prompt.
    if state.repo_standards_cache.is_none() {
        let present = fetch_repo_standards(&*deps.tools, session_id)
            .await
            .unwrap_or_else(|error| {
                warn!(
                    msg = "failed to fetch repo standards from the node",
                    session_id = %session_id,
                    error = %error.display_chain()
                );
                Vec::new()
            });
        state.repo_standards_cache = Some(present);
    }
    let repo_standards = state
        .repo_standards_cache
        .as_ref()
        .expect("populated above");
    let tools: Vec<ToolSpec> = canonical_tools(permission)
        .into_iter()
        .filter(|tool| tool_allowed(&allowed_tools, &tool.name))
        .filter(|tool| {
            // The tree is recursive: any session may spawn children or message
            // its own children, each level supervising its own. The machinery
            // that starts or wakes child loops decides advertisement, not the
            // session's depth. `todowrite` stays root-only: the user's todo
            // list belongs to the tree owner, so children never see the tool.
            match tool.name.as_str() {
                "spawn" => !deps.personas.is_empty() && deps.spawner.is_some(),
                "message_child" => deps.mailbox.is_some(),
                "todowrite" => session.parent_id.is_none(),
                _ => true,
            }
        })
        .collect();

    let system = system_prompt(
        persona_system_prompt(deps, &session),
        repo_standards,
        &state.todos,
        skills,
        live,
        // The persona catalog is advertised only to sessions whose surface
        // includes `spawn`: it is the list of personas they may spawn.
        tools
            .iter()
            .any(|tool| tool.name == "spawn")
            .then(|| persona_catalog(deps)),
    );
    let mut stream = turn.provider.chat_stream(ProviderCall {
        model: turn.provider.model(),
        max_tokens: 4096,
        system: &system,
        messages,
        tools,
        ask_recipient,
    })?;

    let (text, tool_calls, stopped) =
        match collect_stream(&mut stream, deps, session_id, signal, &turn).await? {
            StreamEnd::Collected {
                text,
                tool_calls,
                stopped,
            } => (text, tool_calls, stopped),
            StreamEnd::Interrupted => return Ok(TurnOutcome::Interrupted),
            StreamEnd::Failed(error) => {
                error!(
                    msg = "provider stream failed",
                    session_id = %session_id,
                    provider = %turn.provider.name(),
                    error = %error.display_chain()
                );
                return Ok(TurnOutcome::Failed);
            }
        };

    if !stopped {
        error!(
            msg = "provider stream ended without a stop event",
            session_id = %session_id,
            provider = %turn.provider.name()
        );
        return Ok(TurnOutcome::Failed);
    }

    if !text.is_empty() {
        record_in_wake(
            deps,
            session_id,
            window,
            Role::Assistant,
            &Block::Text { text: text.clone() },
        )
        .await?;
    }

    let calls: Vec<(String, String, Value)> = parse_tool_calls(tool_calls, session_id);

    if calls.is_empty() {
        return Ok(TurnOutcome::Finished { text });
    }

    for (id, name, args) in calls {
        // Commit each tool call to the transcript just before dispatching it,
        // so calls after an ask or a mid-turn interrupt never leave a phantom
        // tool_use without a result.
        record_in_wake(
            deps,
            session_id,
            window,
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

        // The session's allowed-tool set is the second half of its effective
        // surface (the executor enforces the permission); a call outside it is
        // refused without reaching the executor. A nameless call falls through
        // to the unknown-tool branch below.
        if !name.is_empty() && !tool_allowed(&allowed_tools, &name) {
            warn!(
                msg = "tool call refused: not allowed for this session",
                session_id = %session_id,
                tool = %name,
                call_id = %id
            );
            let content = json!({ "error": format!("tool {name} is not allowed") });
            deps.store
                .complete_tool_call(session_id, &id, &content, true)
                .await?;
            record_in_wake(
                deps,
                session_id,
                window,
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
        }

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
                record_in_wake(
                    deps,
                    session_id,
                    window,
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
                let child_id = args["child_id"].as_str().map(String::from);
                // A child_id re-raises the question the named child last asked
                // this session: at the root the ask surfaces to the user and
                // the binding is recorded durably, anywhere below it re-raises
                // the question to this session's parent. The named child's ask
                // event in this session's thread carries the ORIGIN leaf — the
                // session whose own question started the raise, stamped
                // mechanically when the child authored the event — so the
                // bind preserves that leaf and a surfaced question is never
                // resolved by walking transcripts or reading the child's later
                // state. Each level validates the child it names is its own,
                // so a session can only raise a question that reached it.
                let outcome = async {
                    let bound_leaf = match &child_id {
                        None => None,
                        Some(child_id) => {
                            let Some(child) = deps.store.get_session(child_id).await? else {
                                anyhow::bail!("no child session {child_id}");
                            };
                            if child.parent_id.as_deref() != Some(session.id.as_str()) {
                                anyhow::bail!("session {child_id} is not a child of this session");
                            }
                            Some(resolve_ask_leaf(window, child_id)?)
                        }
                    };
                    // One question is raised at a time, at every level: while
                    // this session has an unresolved raised ask — the root's
                    // awaiting the user's answer, a child's awaiting its
                    // parent's — a further ask would split the next message
                    // between two questions, so it is refused with the raised
                    // child named. The row names the DIRECT child the session
                    // raised, uniformly at every level, so the refusal names
                    // a child the session can actually message. The session
                    // resolves the pending one first by messaging that child
                    // to cancel it, or by waiting for its answer.
                    if let Some(pending) = deps.store.get_pending_ask(&session.id).await? {
                        if session.parent_id.is_none() {
                            if child_id.as_deref() == Some(pending.child_id.as_str()) {
                                anyhow::bail!(
                                    "child {}'s question is already pending with the user",
                                    pending.child_id
                                );
                            }
                            anyhow::bail!(
                                "another question is pending with the user; send child {} a message to cancel it before asking again",
                                pending.child_id
                            );
                        }
                        anyhow::bail!(
                            "another question is pending with your parent; send child {} a message to cancel it before asking again",
                            pending.child_id
                        );
                    }
                    Ok::<Option<String>, anyhow::Error>(bound_leaf)
                }
                .await;
                let (content, is_error) = match &outcome {
                    Ok(_) => (json!({ "asked": true }), false),
                    Err(error) => (json!({ "error": error.to_string() }), true),
                };
                deps.store
                    .complete_tool_call(session_id, &id, &content, is_error)
                    .await?;
                if is_error {
                    record_in_wake(
                        deps,
                        session_id,
                        window,
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
                }
                let bound_leaf = outcome.expect("a successful ask names its bound leaf");
                let ask_id = record_in_wake(
                    deps,
                    session_id,
                    window,
                    Role::Assistant,
                    &Block::Ask {
                        message: message.clone(),
                        options,
                        // The surfaced ask names the direct child whose
                        // question it carries — the session the raiser's own
                        // model can message to answer, deny, or cancel it. The
                        // origin leaf stays internal, on the pending row.
                        child_id: child_id.clone(),
                        answer: None,
                    },
                )
                .await?;
                if let Some(raised_child) = &child_id {
                    let leaf = bound_leaf
                        .as_ref()
                        .expect("a raised ask names its origin leaf");
                    // The raised Ask block can be compacted away, so the
                    // binding is a store record, not a transcript scan. Every
                    // session that raises records one on itself: the row names
                    // the direct child it raised — so messaging that child
                    // cancels the raise, the child's next event clears the
                    // row, and the one-pending gate names a messageable child
                    // — and the origin leaf, so the user's answer at the root
                    // routes to the session whose own question it is. While
                    // the row stands the session raises nothing else.
                    deps.store
                        .set_pending_ask(&session.id, raised_child, leaf, &message, ask_id)
                        .await?;
                }
                return Ok(TurnOutcome::AskedUser {
                    question: message,
                    origin: bound_leaf,
                });
            }
            "todowrite" => {
                // Children never see the todo tool, so a call here is a
                // refused fallback, not a path a model should reach.
                if session.parent_id.is_some() {
                    let content =
                        json!({ "error": "todowrite is only available to the root session" });
                    deps.store
                        .complete_tool_call(session_id, &id, &content, true)
                        .await?;
                    record_in_wake(
                        deps,
                        session_id,
                        window,
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
                }
                match args["items"].as_array() {
                    Some(items) => state.todos = items.clone(),
                    None => warn!(
                        msg = "todowrite items are not an array",
                        session_id = %session_id
                    ),
                }
                deps.store
                    .complete_tool_call(session_id, &id, &json!({ "ok": true }), false)
                    .await?;
                record_in_wake(
                    deps,
                    session_id,
                    window,
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
                record_in_wake(
                    deps,
                    session_id,
                    window,
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
            // Creates a real child session: the child runs its own loop and
            // executor on this working copy under the target persona. The
            // call returns the child's id and the turn continues; the child's
            // completion report arrives later as an authored event in this
            // session's thread.
            "spawn" => {
                let persona_name = args["persona"].as_str().unwrap_or_default().to_string();
                let instructions = args["instructions"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let outcome = async {
                    let Some(persona) = deps.personas.get(&persona_name) else {
                        anyhow::bail!("unknown persona {persona_name}");
                    };
                    if !deps.providers.contains_key(&persona.model) {
                        anyhow::bail!("no provider for model {}", persona.model);
                    }
                    let Some(spawner) = &deps.spawner else {
                        anyhow::bail!("spawn is not available");
                    };
                    let child_id = spawner
                        .spawn(
                            deps.store.clone(),
                            SpawnChild {
                                parent: session.clone(),
                                persona_name: persona_name.clone(),
                                persona: persona.clone(),
                                instructions: instructions.clone(),
                            },
                        )
                        .await
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    Ok::<String, anyhow::Error>(child_id)
                }
                .await;
                let (content, is_error) = match outcome {
                    Ok(child_id) => (json!({ "child_id": child_id }), false),
                    Err(error) => (json!({ "error": error.to_string() }), true),
                };
                deps.store
                    .complete_tool_call(session_id, &id, &content, is_error)
                    .await?;
                record_in_wake(
                    deps,
                    session_id,
                    window,
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
            // Resumes or redirects one of this session's children: the
            // message is appended to the child's thread as its next user
            // message and the child's loop is woken, so a stopped child
            // resumes from its archived thread and reports again.
            "message_child" => {
                let child_id = args["id"].as_str().unwrap_or_default().to_string();
                let text = args["text"].as_str().unwrap_or_default().to_string();
                let outcome = async {
                    let Some(mailbox) = &deps.mailbox else {
                        anyhow::bail!("message_child is not available");
                    };
                    let Some(child) = deps.store.get_session(&child_id).await? else {
                        anyhow::bail!("no child session {child_id}");
                    };
                    if child.parent_id.as_deref() != Some(session.id.as_str()) {
                        anyhow::bail!("session {child_id} is not a child of this session");
                    }
                    // A message to the raised child resolves the question the
                    // session's model has taken over (a redirect wake's
                    // cancel or denial, at any level): drop the pending row
                    // first, so the user's next message is not routed to a
                    // child the model already answered, and a session whose
                    // raise was cancelled may raise again.
                    let pending = deps.store.get_pending_ask(&session.id).await?;
                    if pending
                        .as_ref()
                        .is_some_and(|pending| pending.child_id == child_id)
                    {
                        deps.store.clear_pending_ask(&session.id).await?;
                    }
                    deps.store
                        .append_message(&child_id, Role::User, &Block::Text { text })
                        .await?;
                    mailbox.send(&child_id, LoopEvent::ParentMessage);
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                let (content, is_error) = match outcome {
                    Ok(()) => (json!({ "ok": true }), false),
                    Err(error) => (json!({ "error": error.to_string() }), true),
                };
                deps.store
                    .complete_tool_call(session_id, &id, &content, is_error)
                    .await?;
                record_in_wake(
                    deps,
                    session_id,
                    window,
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
            _ => {
                let run_id = Uuid::new_v4().to_string();
                debug!(
                    msg = "dispatching tool call",
                    session_id = %session_id,
                    tool = %name,
                    run_id = %run_id
                );
                let Some(outcome) =
                    run_tool_call(deps, session_id, &run_id, &name, args, signal).await?
                else {
                    return Ok(TurnOutcome::Interrupted);
                };
                deps.store
                    .complete_tool_call(session_id, &id, &outcome.content, outcome.is_error)
                    .await?;
                record_in_wake(
                    deps,
                    session_id,
                    window,
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

/// Runs the provider stream to its end, accumulating text and tool-call
/// deltas and forwarding text to the delta sink. The caller decides what the
/// end state means and logs it.
async fn collect_stream(
    stream: &mut BoxStream<'static, Result<StreamEvent, ProviderError>>,
    deps: &Arc<LoopDeps>,
    session_id: &str,
    signal: &Arc<InterruptSignal>,
    turn: &TurnModel,
) -> Result<StreamEnd, anyhow::Error> {
    let mut text = String::new();
    let mut tool_calls = BTreeMap::<usize, AccumulatedToolCall>::new();
    let mut stopped = false;

    loop {
        if signal.flag.load(Ordering::Acquire) {
            return Ok(StreamEnd::Interrupted);
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
                            turn.provider.model(),
                            turn.provider.name(),
                            "completion",
                            Some(input_tokens),
                            Some(output_tokens),
                            Some(model_call_cost(
                                Some(input_tokens),
                                Some(output_tokens),
                                turn.price_input_per_mtok,
                                turn.price_output_per_mtok,
                            )),
                        )
                        .await?;
                    stopped = true;
                }
                Some(Err(error)) => return Ok(StreamEnd::Failed(anyhow::Error::new(error))),
                None => break,
            },
            _ = signal.notify.notified() => {
                if signal.flag.load(Ordering::Acquire) {
                    return Ok(StreamEnd::Interrupted);
                }
            }
        }
    }

    Ok(StreamEnd::Collected {
        text,
        tool_calls,
        stopped,
    })
}

/// Runs one tool call to its end, streaming deltas to the delta sink. An
/// interrupt cancels the in-flight call and returns `Ok(None)`; the caller
/// translates that into its own interrupted outcome.
async fn run_tool_call(
    deps: &Arc<LoopDeps>,
    session_id: &str,
    run_id: &str,
    name: &str,
    args: Value,
    signal: &Arc<InterruptSignal>,
) -> anyhow::Result<Option<ToolOutcome>> {
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<ToolDelta>();
    let mut call = deps.tools.call(
        session_id.to_string(),
        run_id.to_string(),
        name.to_string(),
        args,
        delta_tx,
    );
    let outcome = loop {
        if signal.flag.load(Ordering::Acquire) {
            if let Err(error) = deps
                .tools
                .cancel(session_id.to_string(), run_id.to_string())
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
            return Ok(None);
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
                        .cancel(session_id.to_string(), run_id.to_string())
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
                    return Ok(None);
                }
            }
        }
    };
    drop(call);
    // The outcome can overtake the tool's final deltas, so drain whatever the
    // select left in the channel before recording.
    while let Ok(delta) = delta_rx.try_recv() {
        deps.delta_sink.send(delta.text);
    }
    Ok(Some(outcome))
}

/// Maps the accumulated tool-call deltas into `(id, name, args)` triples;
/// unparseable argument JSON becomes `Value::Null` with a warning.
fn parse_tool_calls(
    tool_calls: BTreeMap<usize, AccumulatedToolCall>,
    session_id: &str,
) -> Vec<(String, String, Value)> {
    tool_calls
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
        .collect()
}

/// Whether an allowed-tools parse result (`None` = every tool) permits `name`.
fn tool_allowed(allowed_tools: &Option<Vec<String>>, name: &str) -> bool {
    match allowed_tools {
        None => true,
        Some(names) => names.iter().any(|n| n == name),
    }
}

/// The origin leaf of the question `named` last raised into this session's
/// window: the origin leaf its Ask event carries. A child that is waiting on
/// a question has authored nothing since the event, so the child's last
/// authored event in the window is the live ask; when it is an ask, its
/// origin — stamped mechanically by the child's loop at authoring time — is
/// the session the question must bind to, and nothing is resolved from the
/// child's thread or from any later state of the tree. When the child's last
/// authored event is not an ask, the child is not waiting on a question and
/// nothing may be bound. Refusals name `named`, the child the caller chose,
/// so the model knows which bind it must fix.
fn resolve_ask_leaf(window: &[(i64, Message)], named: &str) -> anyhow::Result<String> {
    for (_, message) in window.iter().rev() {
        let Block::ChildEvent {
            child_id,
            kind,
            origin,
            ..
        } = &message.block
        else {
            continue;
        };
        if child_id != named {
            continue;
        }
        match (kind, origin) {
            (ChildEventKind::Ask, Some(leaf)) => return Ok(leaf.clone()),
            _ => break,
        }
    }
    anyhow::bail!("child session {named} has no pending question to surface")
}

/// Compacts the wake's working window when it exceeds `max_window_messages`:
/// the oldest messages are summarized by the provider, archived in the
/// store, and replaced by a Summary message in the window. Only messages the
/// wake was woken to process — at or below its snapshot boundary — are
/// retired: archiving is an id range in the store, and a mid-wake message
/// from another writer can sit between ids the wake never saw. On a
/// summarizer failure or interrupt the store is left untouched and the full
/// window is returned.
async fn maybe_compact(
    deps: &Arc<LoopDeps>,
    turn: &TurnModel,
    session_id: &str,
    signal: &Arc<InterruptSignal>,
    window: &mut Vec<(i64, Message)>,
    wake_boundary: i64,
    ask_recipient: AskRecipient,
) -> anyhow::Result<Vec<Message>> {
    if window.len() > deps.max_window_messages {
        let keep = deps.max_window_messages / 2;
        let retireable = window
            .iter()
            .take_while(|(id, _)| *id <= wake_boundary)
            .count();
        let retire = (window.len() - keep).min(retireable);
        if retire > 0 {
            let tail: Vec<(i64, Message)> = window.drain(..retire).collect();
            let tail_last_id = tail.last().expect("retire is at least one").0;
            if let Some((text, input_tokens, output_tokens)) =
                summarize_tail(turn, session_id, ask_recipient, &tail, signal).await
            {
                let summary = Message {
                    role: Role::Assistant,
                    block: Block::Summary { text },
                };
                let summary_id = deps
                    .store
                    .append_message(session_id, summary.role, &summary.block)
                    .await?;
                deps.store.mark_archived(session_id, tail_last_id).await?;
                deps.store
                    .append_model_call(
                        session_id,
                        turn.provider.model(),
                        turn.provider.name(),
                        "compaction",
                        input_tokens,
                        output_tokens,
                        Some(model_call_cost(
                            input_tokens,
                            output_tokens,
                            turn.price_input_per_mtok,
                            turn.price_output_per_mtok,
                        )),
                    )
                    .await?;
                window.push((summary_id, summary));
                info!(
                    session_id = %session_id,
                    retired = retire,
                    "compacted transcript"
                );
            } else {
                // The summarizer failed or was interrupted: put the retired
                // tail back in place and leave the store untouched.
                window.splice(0..0, tail);
            }
        }
    }
    Ok(window.iter().map(|(_, message)| message.clone()).collect())
}

/// Asks the provider to summarize the retired tail: the instruction plus the
/// rendered messages as one user message. Returns the summary text and the
/// token counts when the stream ended with a Stop; returns None on a stream
/// error, a missing Stop, or an interrupt, logging the reason.
async fn summarize_tail(
    turn: &TurnModel,
    session_id: &str,
    ask_recipient: AskRecipient,
    tail: &[(i64, Message)],
    signal: &Arc<InterruptSignal>,
) -> Option<(String, Option<u64>, Option<u64>)> {
    let mut prompt = String::from(SUMMARIZATION_PROMPT);
    for (_, message) in tail {
        prompt.push_str(&format!(
            "\n\n{}: {}",
            message.role.as_str(),
            render_block(&message.block, ask_recipient)
        ));
    }
    let messages = vec![Message {
        role: Role::User,
        block: Block::Text { text: prompt },
    }];
    let mut stream = match turn.provider.chat_stream(ProviderCall {
        model: turn.provider.model(),
        max_tokens: MAX_TOKENS,
        system: "",
        messages,
        tools: vec![],
        ask_recipient,
    }) {
        Ok(stream) => stream,
        Err(error) => {
            warn!(
                msg = "summarizer request failed",
                session_id = %session_id,
                provider = %turn.provider.name(),
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
                        provider = %turn.provider.name(),
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
            provider = %turn.provider.name()
        );
        return None;
    }
    Some((text, input_tokens, output_tokens))
}

/// One message as plain text for the summarizer. `ask_recipient` is whose
/// answer the session's own asks wait on, so the rendered question does not
/// misattribute a child's ask to the user.
fn render_block(block: &Block, ask_recipient: AskRecipient) -> String {
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
            message,
            options,
            child_id,
            answer,
        } => {
            let origin = child_id
                .as_deref()
                .map(|child_id| format!(", from child {child_id}"))
                .unwrap_or_default();
            let answered = answer
                .as_deref()
                .map(|answer| format!(" (user answered: {answer})"))
                .unwrap_or_default();
            format!(
                "question to {}{origin}: {message}{answered} (options: {})",
                ask_recipient.as_str(),
                options.join(", ")
            )
        }
        Block::Summary { text } => format!("summary: {text}"),
        Block::ChildEvent {
            child_id,
            kind,
            text,
            ..
        } => format!("{} from child {child_id}: {text}", kind.as_str()),
    }
}

/// The session's persona prompt body, when the session names a configured
/// persona that has one. A session without a persona (or whose persona is no
/// longer configured) runs on the default system text below.
fn persona_system_prompt<'a>(deps: &'a LoopDeps, session: &Session) -> Option<&'a str> {
    let name = session.persona.as_deref()?;
    match deps.personas.get(name) {
        Some(persona) => persona.system_prompt.as_deref(),
        None => {
            warn!(
                msg = "session persona is not configured; using the default system prompt",
                session_id = %session.id,
                persona = %name
            );
            None
        }
    }
}

const DEFAULT_SYSTEM_PROMPT: &str = "You are Bosun, an autonomous software engineering agent. \
     You work in a session working copy. Use the provided tools to inspect \
     and modify code. Prefer the simplest change that works. Keep replies \
     concise and literal. Ask the user only when a decision requires them.";

/// The configured personas as `(name, description)` pairs in name order, the
/// catalog advertised to spawn-capable sessions. The description is context
/// for choosing a spawn target, never prompt text for the persona itself.
fn persona_catalog(deps: &LoopDeps) -> Vec<(String, String)> {
    let mut catalog: Vec<(String, String)> = deps
        .personas
        .iter()
        .map(|(name, persona)| (name.clone(), persona.description.clone()))
        .collect();
    catalog.sort_by(|a, b| a.0.cmp(&b.0));
    catalog
}

/// Builds the system prompt: the persona's role text when it has one (the
/// built-in default otherwise), then the session's live context — the
/// repo-standard files present in the working copy, the persona catalog for
/// spawn-capable sessions, skill advertisements, the todo list, and the
/// manifest of children whose state or latest authored event this wake is
/// reacting to. The system prompt is never stored.
fn system_prompt(
    persona: Option<&str>,
    repo_standards: &[String],
    todos: &[Value],
    skills: &[Skill],
    live: &[LiveChild],
    catalog: Option<Vec<(String, String)>>,
) -> String {
    let mut prompt = persona.unwrap_or(DEFAULT_SYSTEM_PROMPT).to_string();
    if !repo_standards.is_empty() {
        prompt.push_str(&format!(
            "\n\nRepo standards present: {}. The contents are not in this context; \
             read the files with the file tools when your task needs them.",
            repo_standards.join(", ")
        ));
    }
    if let Some(catalog) = catalog {
        prompt.push_str("\n\nPersonas you may spawn:");
        for (name, description) in catalog {
            if description.is_empty() {
                prompt.push_str(&format!("\n- {name}"));
            } else {
                prompt.push_str(&format!("\n- {name}: {description}"));
            }
        }
    }
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
    if !live.is_empty() {
        prompt.push_str("\n\nLive children:");
        for child in live {
            let persona = child.persona.as_deref().unwrap_or("default");
            let last = child.last_authored.as_deref().unwrap_or("none");
            prompt.push_str(&format!(
                "\n- {} (persona: {persona}, state: {}, last message: {last})",
                child.id,
                state_name(child.state)
            ));
        }
    }
    prompt
}

/// A session state as the manifest renders it: the wire-format names the
/// store uses.
fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Creating => "creating",
        SessionState::Running => "running",
        SessionState::WaitingForInput => "waiting_for_input",
        SessionState::Interrupted => "interrupted",
        SessionState::Stopped => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use bosun_common::config::PersonaConfig;
    use bosun_common::session::Permission;
    use bosun_common::session::Session;
    use bosun_common::session::SessionState;
    use bosun_common::tool::ALL_TOOLS;
    use bosun_common::tool::ToolDelta;
    use bosun_store::store::RouteAnswer;
    use bosun_store::store::Store;
    use bosun_test_support::wait_for;
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
            persona: None,
            parent_id: None,
            owner_id: id.to_string(),
            permission: Permission::ReadWrite,
            allowed_tools: ALL_TOOLS.to_string(),
            state: SessionState::Creating,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: None,
        }
    }

    /// A session whose persona allows only `names`.
    fn session_allowing(id: &str, names: &str) -> Session {
        Session {
            allowed_tools: names.to_string(),
            ..session(id)
        }
    }

    /// A session that names `persona` as the one it runs under.
    fn session_under_persona(id: &str, persona: &str) -> Session {
        Session {
            persona: Some(persona.to_string()),
            ..session(id)
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
        ask_recipient: AskRecipient,
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
                ask_recipient: call.ask_recipient,
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

    /// Asserts one provider request's tool traffic is a shape a real provider
    /// accepts: every tool result names a tool use that precedes it in the
    /// same request. The scripted provider never validates requests, so a
    /// dangling tool result — one whose tool use the loop dropped — surfaces
    /// only as a real provider's 400.
    fn assert_tool_results_have_matching_tool_uses(call: &CapturedCall) {
        for (index, message) in call.messages.iter().enumerate() {
            let Block::ToolResult { id, name, .. } = &message.block else {
                continue;
            };
            assert!(
                call.messages[..index]
                    .iter()
                    .any(|earlier| matches!(&earlier.block, Block::ToolCall { id: use_id, .. } if use_id == id)),
                "provider request has a tool result for {name} ({id}) with no preceding tool use: {:#?}",
                call.messages
            );
        }
    }

    /// Reports a distinct model name while delegating to a scripted provider,
    /// so a test can tell which model the loop resolved by the name it
    /// records on its model calls.
    struct ModelNamedProvider {
        inner: ScriptedProvider,
        model: String,
    }

    impl ModelNamedProvider {
        fn new(scripts: Vec<Vec<StreamEvent>>, model: &str) -> Self {
            Self {
                inner: ScriptedProvider::new(scripts),
                model: model.to_string(),
            }
        }
    }

    impl Provider for ModelNamedProvider {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn chat_stream<'a>(
            &'a self,
            call: ProviderCall<'a>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            self.inner.chat_stream(call)
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
            // A test's canned outcome wins; unserved internal plumbing calls
            // are answered with empty results, so a test that does not care
            // about a fetch neither waits out its retries nor hangs a
            // blocking-tools turn on it.
            let served: Option<ToolOutcome> = self
                .outcomes
                .get(&name)
                .cloned()
                .or_else(|| plumbing_outcome(&name));
            let outcome = served.clone().unwrap_or_else(|| self.outcome.clone());
            self.calls.lock().unwrap().push(CapturedToolCall {
                session_id,
                run_id,
                name,
                args,
            });
            let delta_text = self.delta_text.clone();
            let block = self.block && served.is_none();
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

    fn default_outcome() -> ToolOutcome {
        ToolOutcome {
            content: json!({ "ok": true }),
            is_error: false,
        }
    }

    /// The empty success outcome for an internal plumbing call the loop makes
    /// outside the model surface: listing the working copy's skills, and
    /// fetching the repo-standard presence.
    fn plumbing_outcome(name: &str) -> Option<ToolOutcome> {
        let content = match name {
            "skills" => json!({ "skills": [] }),
            "repo_standards" => json!({ "present": [] }),
            _ => return None,
        };
        Some(ToolOutcome {
            content,
            is_error: false,
        })
    }

    /// Tools that answer the session-start skills round trip instantly: a
    /// failing fetch retries for ~0.8s, which would delay a loop's first
    /// turn past the interleavings these tests script.
    fn instant_tools() -> Arc<MockTools> {
        Arc::new(MockTools::new(default_outcome()).serving(
            "skills",
            ToolOutcome {
                content: json!({ "skills": [] }),
                is_error: false,
            },
        ))
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
            personas: HashMap::new(),
            providers: HashMap::new(),
            prices: HashMap::new(),
            price_input_per_mtok,
            price_output_per_mtok,
            spawner: None,
            mailbox: None,
        }
    }

    /// A deps with configured personas and their providers; existing tests
    /// never fill the window, so compaction stays off.
    fn test_deps_with_personas(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
        personas: HashMap<String, PersonaConfig>,
        providers: HashMap<String, Arc<dyn Provider>>,
    ) -> LoopDeps {
        LoopDeps {
            store: store.clone(),
            provider,
            tools,
            delta_sink: sink,
            max_window_messages: usize::MAX,
            injected_skills_dir: None,
            personas,
            providers,
            prices: HashMap::new(),
            price_input_per_mtok: 0.0,
            price_output_per_mtok: 0.0,
            spawner: None,
            mailbox: None,
        }
    }

    /// A deps with a fake child spawner attached, for `spawn` tool tests.
    fn test_deps_with_spawner(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
        personas: HashMap<String, PersonaConfig>,
        providers: HashMap<String, Arc<dyn Provider>>,
        spawner: Arc<dyn ChildSpawner>,
    ) -> LoopDeps {
        LoopDeps {
            spawner: Some(spawner),
            ..test_deps_with_personas(store, provider, tools, sink, personas, providers)
        }
    }

    /// A deps with a loop mailbox attached, for child-event and
    /// `message_child` tests.
    fn test_deps_with_mailbox(
        store: &Store,
        provider: Arc<dyn Provider>,
        tools: Arc<MockTools>,
        sink: Arc<CollectSink>,
        mailbox: Arc<dyn LoopMailbox>,
    ) -> LoopDeps {
        LoopDeps {
            mailbox: Some(mailbox),
            ..test_deps(store, provider, tools, sink)
        }
    }

    /// Routes loop events to the senders registered under a session id, the
    /// way the control plane's registry routes them in production.
    struct TestMailbox {
        senders: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<LoopEvent>>>>,
    }

    impl TestMailbox {
        fn new() -> Self {
            Self {
                senders: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn register(&self, session_id: &str, sender: mpsc::UnboundedSender<LoopEvent>) {
            self.senders
                .lock()
                .unwrap()
                .insert(session_id.to_string(), sender);
        }
    }

    impl LoopMailbox for TestMailbox {
        fn send(&self, session_id: &str, event: LoopEvent) {
            if let Some(sender) = self.senders.lock().unwrap().get(session_id) {
                let _ = sender.send(event);
            }
        }
    }

    /// Appends a child's authored event to the parent's thread the way a
    /// child's loop would, without running a child loop: the event-injection
    /// seam loop tests use to deliver child events in a scripted order. An
    /// injected ask event is the child asking its own question, so its origin
    /// leaf is the child itself, exactly as a child loop would stamp it.
    async fn deliver_child_event(
        store: &Store,
        parent_id: &str,
        child_id: &str,
        kind: ChildEventKind,
        text: &str,
    ) {
        store
            .append_message(
                parent_id,
                Role::User,
                &Block::ChildEvent {
                    child_id: child_id.to_string(),
                    kind,
                    text: text.to_string(),
                    origin: (kind == ChildEventKind::Ask).then(|| child_id.to_string()),
                },
            )
            .await
            .unwrap();
    }

    /// The session that spawned `id`; children run on the parent's node and
    /// working copy under their own persona.
    fn child_session_of(id: &str, parent: &str) -> Session {
        let mut child = session(id);
        child.parent_id = Some(parent.to_string());
        child.owner_id = parent.to_string();
        child.persona = Some("coder".to_string());
        child
    }

    /// A fake spawner that records its request and answers `result` without
    /// starting anything: the loop under test must not depend on the child
    /// running inside the parent's turn.
    struct FakeSpawner {
        calls: Arc<Mutex<Vec<SpawnChild>>>,
        result: Result<String, SpawnError>,
    }

    impl FakeSpawner {
        fn ok(child_id: &str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                result: Ok(child_id.to_string()),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                result: Err(SpawnError::Failed(message.to_string())),
            }
        }

        fn requested(&self) -> Vec<SpawnChild> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ChildSpawner for FakeSpawner {
        fn spawn(
            &self,
            _store: Store,
            request: SpawnChild,
        ) -> Pin<Box<dyn Future<Output = Result<String, SpawnError>> + Send>> {
            self.calls.lock().unwrap().push(request);
            let result = match &self.result {
                Ok(child_id) => Ok(child_id.clone()),
                Err(SpawnError::Failed(message)) => Err(SpawnError::Failed(message.clone())),
                Err(SpawnError::Internal(_)) => {
                    unreachable!("the fake spawner never fails internally")
                }
            };
            Box::pin(async move { result })
        }
    }

    fn persona(model: &str, permission: Permission) -> PersonaConfig {
        PersonaConfig {
            model: model.to_string(),
            permission,
            allowed_tools: ALL_TOOLS.to_string(),
            description: String::new(),
            system_prompt: None,
        }
    }

    /// A persona whose system prompt reads `prompt`.
    fn persona_with_prompt(model: &str, permission: Permission, prompt: &str) -> PersonaConfig {
        PersonaConfig {
            system_prompt: Some(prompt.to_string()),
            ..persona(model, permission)
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
        let stored = store.get_session("s2").await.unwrap().unwrap();
        assert_eq!(
            stored.interrupt_cause,
            Some(InterruptCause::User),
            "a user interrupt is recorded as user"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn ask_ends_the_turn_and_waits() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-ask")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
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
        ]]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
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
        let (message, options, child_id, answer) = messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    options,
                    child_id,
                    answer,
                } => Some((message, options, child_id, answer)),
                _ => None,
            })
            .expect("an ask block is recorded");
        assert_eq!(message.as_str(), "continue?");
        assert_eq!(options.as_slice(), ["yes", "no"]);
        assert!(
            child_id.is_none(),
            "a root's own ask is not bound to a child"
        );
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

        let requests = provider.captured_calls();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].ask_recipient,
            AskRecipient::User,
            "a root's own ask is rendered as a question to the user"
        );

        handle.stop();
    }

    #[test]
    fn render_block_renders_a_childs_own_ask_to_its_parent() {
        let ask = Block::Ask {
            message: "may I push?".into(),
            options: vec!["yes".into(), "no".into()],
            child_id: None,
            answer: None,
        };
        assert_eq!(
            render_block(&ask, AskRecipient::Parent),
            "question to parent: may I push? (options: yes, no)"
        );
    }

    #[test]
    fn render_block_attributes_a_surfaced_ask_to_its_child_with_the_serializers_comma() {
        let ask = Block::Ask {
            message: "may I push?".into(),
            options: vec!["yes".into(), "no".into()],
            child_id: Some("child-1".into()),
            answer: None,
        };
        assert_eq!(
            render_block(&ask, AskRecipient::User),
            "question to user, from child child-1: may I push? (options: yes, no)"
        );
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

        // The loop listed the working copy's skills through the executor once,
        // cached for the second turn, and read the skill the model asked for.
        let calls = tools.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|call| call.name == "skills").count(),
            1,
            "the skills list is fetched once per session, not per turn"
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
    async fn repo_standards_notice_is_fetched_once_per_session_and_cached() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-standards")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"cargo test"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("tests pass".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let tools = Arc::new(MockTools::new(default_outcome()).serving(
            "repo_standards",
            ToolOutcome {
                content: json!({ "present": ["AGENTS.md", "CLAUDE.md"] }),
                is_error: false,
            },
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-standards".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-standards").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 2, "the shell result starts a second turn");
        for call in &calls {
            assert!(
                call.system
                    .contains("Repo standards present: AGENTS.md, CLAUDE.md"),
                "every turn's system prompt carries the presence notice"
            );
        }

        // The presence fetch went to the executor once across both turns; the
        // second turn composes its notice from the cache.
        {
            let tool_calls = tools.calls.lock().unwrap();
            assert_eq!(
                tool_calls
                    .iter()
                    .filter(|call| call.name == "repo_standards")
                    .count(),
                1,
                "the repo-standards presence is fetched once per session, not per turn"
            );
        }

        // The notice lives only in the ephemeral system prompt: no stored
        // message or tool record names the files or the notice.
        let messages = store.messages("s-standards", false).await.unwrap();
        for (_, message) in &messages {
            let text = serde_json::to_string(&message.block).unwrap();
            assert!(
                !text.contains("Repo standards"),
                "a stored message carries the presence notice: {text}"
            );
            assert!(
                !text.contains("AGENTS.md") && !text.contains("CLAUDE.md"),
                "a stored message names a repo-standard file: {text}"
            );
        }

        handle.stop();
    }

    #[tokio::test]
    async fn repo_standards_notice_reaches_child_sessions() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("s-parent")).await.unwrap();
        store
            .create_session(&child_session_of("s-child", "s-parent"))
            .await
            .unwrap();

        // A child runs on the parent's working copy, so its own first turn
        // fetches the same presence list for its own system prompt.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("child report".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let tools = Arc::new(MockTools::new(default_outcome()).serving(
            "repo_standards",
            ToolOutcome {
                content: json!({ "present": ["CLAUDE.md"] }),
                is_error: false,
            },
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-child".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child to stop", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-child").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .system
                .contains("Repo standards present: CLAUDE.md"),
            "the child's system prompt carries the presence notice"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_failed_repo_standards_fetch_degrades_to_no_notice_without_crashing() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("s-standards-down"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"true"}"#.into(),
                },
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
        // The node does not answer the presence call: the fetch retries its
        // bounded attempts, then the session runs without a notice.
        let tools = Arc::new(MockTools::new(default_outcome()).serving(
            "repo_standards",
            ToolOutcome {
                content: json!({ "error": "the node has no live tunnel" }),
                is_error: true,
            },
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-standards-down".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for(
            "the session to wait for input despite the failed fetch",
            || {
                let store = store.clone();
                async move {
                    let stored = store
                        .get_session("s-standards-down")
                        .await
                        .unwrap()
                        .unwrap();
                    stored.state == SessionState::WaitingForInput
                }
            },
        )
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert!(
                !call.system.contains("Repo standards present"),
                "a failed fetch must not leave a presence notice in the system prompt"
            );
        }

        // The failed fetch gave up after its bounded retries on the first
        // turn and cached the empty result: the second turn fetched nothing.
        let tool_calls = tools.calls.lock().unwrap();
        assert_eq!(
            tool_calls
                .iter()
                .filter(|call| call.name == "repo_standards")
                .count(),
            4,
            "the presence fetch retries its bounded attempts, once per session"
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

        let stored = store.get_session("s-cancel").await.unwrap().unwrap();
        assert_eq!(
            stored.interrupt_cause,
            Some(InterruptCause::User),
            "an interrupt that cancels a tool is recorded as user"
        );

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
        let stored = store.get_session("s-fail").await.unwrap().unwrap();
        assert_eq!(
            stored.interrupt_cause,
            Some(InterruptCause::Crash),
            "a turn that fails on its own interrupts the session as a crash"
        );

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
    async fn allowed_tools_restrict_the_advertised_schema() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session_allowing("s-allow", "file/read, grep, glob"))
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
        let handle = spawn_loop("s-allow".into(), deps);

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
        assert_eq!(names, ["file/read", "grep", "glob"]);
        assert!(
            !names.contains(&"spawn"),
            "an allow-list without spawn does not advertise it"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_call_to_a_disallowed_tool_is_refused_without_dispatch() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session_allowing("s-refuse", "file/read"))
            .await
            .unwrap();

        let tools = Arc::new(MockTools::new(default_outcome()));
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"rm -rf /"}"#.into(),
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
            provider,
            tools.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("s-refuse".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-refuse").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-refuse", false).await.unwrap();
        let refused = messages.iter().find(
            |(_, message)| matches!(&message.block, Block::ToolResult { id, .. } if id == "call-1"),
        );
        let (is_error, content) = match refused.map(|(_, message)| &message.block) {
            Some(Block::ToolResult {
                is_error, content, ..
            }) => (*is_error, content.clone()),
            _ => panic!("the refused call must record a tool result"),
        };
        assert!(is_error);
        assert_eq!(content, json!({ "error": "tool shell is not allowed" }));

        assert!(
            !tools
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.name == "shell"),
            "a disallowed tool call never reaches the executor"
        );

        let tool_calls = store.tool_calls("s-refuse").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert!(tool_calls[0].is_error);

        handle.stop();
    }

    #[tokio::test]
    async fn an_unparsable_session_allowed_tools_fails_the_turn_closed() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session_allowing("s-bad-allow", "shell, websurf"))
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
        let handle = spawn_loop("s-bad-allow".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to interrupt after the failed turn", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-bad-allow").await.unwrap().unwrap();
                stored.state == SessionState::Interrupted
            }
        })
        .await;

        assert!(
            provider.captured_calls().is_empty(),
            "the turn must not reach the provider when the allow-list is unparsable"
        );
        assert!(
            store.model_calls("s-bad-allow").await.unwrap().is_empty(),
            "no model call is recorded for a turn that never started"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_session_runs_under_its_personas_system_prompt() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session_under_persona("s-persona", "reviewer"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let deps = Arc::new(test_deps_with_personas(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "reviewer".to_string(),
                persona_with_prompt(
                    "mock-model",
                    Permission::ReadWrite,
                    "You are a meticulous reviewer. Never edit files.",
                ),
            )]),
            HashMap::new(),
        ));
        let handle = spawn_loop("s-persona".into(), deps);

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
        assert!(
            calls[0]
                .system
                .contains("You are a meticulous reviewer. Never edit files."),
            "the persona's prompt is the system prompt's role layer: {}",
            calls[0].system
        );
        assert!(
            !calls[0].system.contains("You are Bosun"),
            "the persona's prompt replaces the built-in default role text: {}",
            calls[0].system
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_persona_without_a_prompt_keeps_the_default_role_text() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session_under_persona("s-plain-persona", "coder"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let deps = Arc::new(test_deps_with_personas(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("mock-model", Permission::ReadWrite),
            )]),
            HashMap::new(),
        ));
        let handle = spawn_loop("s-plain-persona".into(), deps);

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
        assert!(
            calls[0].system.contains("You are Bosun"),
            "a persona without a prompt file leaves the default role text: {}",
            calls[0].system
        );

        handle.stop();
    }

    #[tokio::test]
    async fn the_persona_prompt_composes_with_the_dynamic_todo_list() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session_under_persona("s-todo-persona", "coder"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("todowrite".into()),
                    args_delta: r#"{"items":[{"id":"1","content":"write tests","status":"todo"}]}"#
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
        let deps = Arc::new(test_deps_with_personas(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona_with_prompt(
                    "mock-model",
                    Permission::ReadWrite,
                    "You are the coder persona.",
                ),
            )]),
            HashMap::new(),
        ));
        let handle = spawn_loop("s-todo-persona".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for(
            "the second turn's system prompt to carry the persona text and the todo list",
            {
                let provider = provider.clone();
                move || {
                    let provider = provider.clone();
                    async move {
                        let calls = provider.captured_calls();
                        calls.len() == 2
                            && calls[1].system.contains("You are the coder persona.")
                            && calls[1].system.contains("Current todo list")
                    }
                }
            },
        )
        .await;

        let calls = provider.captured_calls();
        assert!(calls[0].system.contains("You are the coder persona."));
        assert!(
            !calls[0].system.contains("Current todo list"),
            "the first turn has no todos yet"
        );
        assert!(calls[1].system.contains("0. [todo] write tests"));

        handle.stop();
    }

    #[tokio::test]
    async fn a_persona_switch_applies_to_the_next_turn() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let mut switched = session_under_persona("s-switch", "coder");
        switched.model = "model-a".into();
        store.create_session(&switched).await.unwrap();

        let coder = persona_with_prompt(
            "model-a",
            Permission::ReadWrite,
            "You are the coder persona.",
        );
        let mut reviewer = persona_with_prompt(
            "model-b",
            Permission::ReadOnly,
            "You are the reviewer persona.",
        );
        reviewer.allowed_tools = "file/read".into();
        let coder_provider = Arc::new(ModelNamedProvider::new(
            one_text_script("coder turn"),
            "model-a",
        ));
        let reviewer_provider = Arc::new(ModelNamedProvider::new(
            one_text_script("reviewer turn"),
            "model-b",
        ));
        let personas = HashMap::from([
            ("coder".to_string(), coder.clone()),
            ("reviewer".to_string(), reviewer.clone()),
        ]);
        let deps = Arc::new(test_deps_with_personas(
            &store,
            coder_provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            personas,
            HashMap::from([
                (
                    "model-a".to_string(),
                    coder_provider.clone() as Arc<dyn Provider>,
                ),
                (
                    "model-b".to_string(),
                    reviewer_provider.clone() as Arc<dyn Provider>,
                ),
            ]),
        ));
        let handle = spawn_loop("s-switch".into(), deps);

        handle.send(LoopEvent::Wake);
        wait_for("the first turn to run under the coder persona", || {
            let store = store.clone();
            async move {
                let calls = store.model_calls("s-switch").await.unwrap();
                calls.len() == 1 && calls[0].model == "model-a"
            }
        })
        .await;
        let first = store.model_calls("s-switch").await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].model, "model-a");

        let coder_calls = coder_provider.inner.captured_calls();
        assert_eq!(coder_calls.len(), 1);
        assert!(
            coder_calls[0].system.contains("You are the coder persona."),
            "{}",
            coder_calls[0].system
        );
        assert!(
            coder_calls[0].tools.iter().any(|tool| tool.name == "shell"),
            "the coder persona's read-write tool schema is advertised"
        );

        // The switch lands between turns: the stored session fields move to
        // the reviewer persona, and the next wake runs under its model.
        store
            .switch_persona(
                "s-switch",
                "reviewer",
                "model-b",
                Permission::ReadOnly,
                "file/read",
            )
            .await
            .unwrap();
        handle.send(LoopEvent::Wake);

        wait_for("the second turn to run under the reviewer persona", || {
            let store = store.clone();
            async move {
                let calls = store.model_calls("s-switch").await.unwrap();
                calls.len() == 2 && calls[1].model == "model-b"
            }
        })
        .await;

        let reviewer_calls = reviewer_provider.inner.captured_calls();
        assert_eq!(reviewer_calls.len(), 1);
        assert!(
            reviewer_calls[0]
                .system
                .contains("You are the reviewer persona."),
            "{}",
            reviewer_calls[0].system
        );
        assert_eq!(
            reviewer_calls[0]
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
            ["file/read"],
            "the reviewer's tool schema is rebuilt from its allowed_tools"
        );
        assert!(
            coder_provider.inner.captured_calls().len() == 1,
            "the switched-away provider sees no further calls"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_persona_switch_mid_turn_does_not_abort_the_running_turn() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let mut switched = session_under_persona("s-mid-switch", "coder");
        switched.model = "model-a".into();
        store.create_session(&switched).await.unwrap();

        // Each streamed item sleeps, so the first turn stays in flight long
        // enough for the switch to land mid-stream.
        let delay = Duration::from_millis(300);
        let coder_provider = Arc::new(ModelNamedProvider {
            inner: ScriptedProvider::with_delay(
                vec![vec![
                    StreamEvent::TextDelta("working".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ]],
                delay,
            ),
            model: "model-a".into(),
        });
        let reviewer_provider = Arc::new(ModelNamedProvider::new(
            one_text_script("reviewed"),
            "model-b",
        ));
        let mut coder = persona("model-a", Permission::ReadWrite);
        coder.system_prompt = Some("You are the coder persona.".into());
        let mut reviewer = persona("model-b", Permission::ReadOnly);
        reviewer.system_prompt = Some("You are the reviewer persona.".into());
        reviewer.allowed_tools = "file/read".into();
        let personas = HashMap::from([
            ("coder".to_string(), coder),
            ("reviewer".to_string(), reviewer),
        ]);
        let deps = Arc::new(test_deps_with_personas(
            &store,
            coder_provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            personas,
            HashMap::from([
                (
                    "model-a".to_string(),
                    coder_provider.clone() as Arc<dyn Provider>,
                ),
                (
                    "model-b".to_string(),
                    reviewer_provider.clone() as Arc<dyn Provider>,
                ),
            ]),
        ));
        let handle = spawn_loop("s-mid-switch".into(), deps);

        handle.send(LoopEvent::Wake);
        wait_for("the first turn's request to start streaming", {
            let coder_provider = coder_provider.clone();
            move || {
                let coder_provider = coder_provider.clone();
                async move { coder_provider.inner.captured_calls().len() == 1 }
            }
        })
        .await;

        store
            .switch_persona(
                "s-mid-switch",
                "reviewer",
                "model-b",
                Permission::ReadOnly,
                "file/read",
            )
            .await
            .unwrap();

        // The in-flight turn is not aborted: it finishes under the model it
        // started with, and the session reaches waiting_for_input.
        wait_for("the in-flight turn to finish", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("s-mid-switch").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                }
            }
        })
        .await;
        let calls = store.model_calls("s-mid-switch").await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].model, "model-a",
            "the running turn keeps the model it started under"
        );
        assert_eq!(
            store
                .get_session("s-mid-switch")
                .await
                .unwrap()
                .unwrap()
                .persona
                .as_deref(),
            Some("reviewer"),
            "the stored session already carries the switch"
        );

        handle.send(LoopEvent::Wake);
        wait_for("the next turn to run under the reviewer model", || {
            let store = store.clone();
            async move {
                let calls = store.model_calls("s-mid-switch").await.unwrap();
                calls.len() == 2 && calls[1].model == "model-b"
            }
        })
        .await;
        let reviewer_calls = reviewer_provider.inner.captured_calls();
        assert_eq!(reviewer_calls.len(), 1);
        assert!(
            reviewer_calls[0]
                .system
                .contains("You are the reviewer persona."),
            "{}",
            reviewer_calls[0].system
        );

        handle.stop();
    }

    /// One text turn, for providers whose exact script does not matter.
    fn one_text_script(text: &str) -> Vec<Vec<StreamEvent>> {
        vec![vec![
            StreamEvent::TextDelta(text.into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]
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
        // stays waiting for input and no cause is recorded.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stored = store.get_session("s-parked").await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::WaitingForInput);
        assert_eq!(
            stored.interrupt_cause, None,
            "an interrupt that kills no turn records no cause"
        );

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
    async fn a_child_that_finishes_reports_to_its_parent_and_stops() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-1")).await.unwrap();
        store
            .create_session(&child_session_of("child-1", "parent-1"))
            .await
            .unwrap();
        store
            .append_message(
                "child-1",
                Role::User,
                &Block::Text {
                    text: "make the change".into(),
                },
            )
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("made the change".into()),
            StreamEvent::Stop {
                input_tokens: 2,
                output_tokens: 1,
            },
        ]]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("child-1".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-1").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        // The child's words, transcript and model calls stay on the child.
        let child_messages = store.messages("child-1", false).await.unwrap();
        assert!(
            child_messages.iter().any(|(_, message)| {
                message.role == Role::Assistant
                    && matches!(&message.block, Block::Text { text } if text == "made the change")
            }),
            "the child stores its own final text"
        );
        let child_calls = store.model_calls("child-1").await.unwrap();
        assert_eq!(child_calls.len(), 1);
        assert_eq!(child_calls[0].model, "mock-model");

        // The parent's thread shows exactly one authored report: the child's
        // final text, attributed to the child, with no child tool traffic.
        let parent_messages = store.messages("parent-1", false).await.unwrap();
        let reports: Vec<(&str, &str)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } => Some((child_id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            [("child-1", "made the change")],
            "the parent thread shows one authored report from the child"
        );
        assert!(
            !parent_messages.iter().any(|(_, message)| {
                matches!(
                    &message.block,
                    Block::ToolCall { .. } | Block::ToolResult { .. }
                )
            }),
            "no raw child tool traffic reaches the parent's thread"
        );

        // The parent's own state was not touched by the child's wake.
        let parent = store.get_session("parent-1").await.unwrap().unwrap();
        assert_eq!(parent.state, SessionState::Creating);

        handle.stop();
    }

    #[tokio::test]
    async fn a_child_with_nothing_more_to_say_reports_and_stops_instead_of_hanging() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-2")).await.unwrap();
        store
            .create_session(&child_session_of("child-2", "parent-2"))
            .await
            .unwrap();

        // The child's turn ends with a stop and no text and no tool calls.
        let deps = Arc::new(test_deps(
            &store,
            Arc::new(ScriptedProvider::new(vec![vec![StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 0,
            }]])),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("child-2".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-2").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        let parent_messages = store.messages("parent-2", false).await.unwrap();
        assert_eq!(parent_messages.len(), 1);
        let (child_id, text) = match &parent_messages[0].1.block {
            Block::ChildEvent {
                child_id,
                kind: ChildEventKind::Report,
                text,
                ..
            } => (child_id.as_str(), text.as_str()),
            _ => panic!("the parent thread must show the child's completion report"),
        };
        assert_eq!(child_id, "child-2");
        assert_eq!(
            text, "",
            "a child with nothing to say authors an empty completion report"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_silent_final_turn_authors_an_empty_report_not_stale_mid_task_text() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-6")).await.unwrap();
        store
            .create_session(&child_session_of("child-6", "parent-6"))
            .await
            .unwrap();
        store
            .append_message(
                "child-6",
                Role::User,
                &Block::Text {
                    text: "make the change".into(),
                },
            )
            .await
            .unwrap();

        // Turn one talks mid-task and runs a tool; the final turn then ends
        // with a textless stop. The report must be empty, not the mid-task
        // words of the earlier turn.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("working on it".into()),
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("shell".into()),
                    args_delta: r#"{"command":"ls"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 0,
            }],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("child-6".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-6").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        // The mid-task words stay on the child's own transcript...
        let child_messages = store.messages("child-6", false).await.unwrap();
        assert!(
            child_messages.iter().any(|(_, message)| {
                message.role == Role::Assistant
                    && matches!(&message.block, Block::Text { text } if text == "working on it")
            }),
            "the child's transcript keeps its mid-task text"
        );
        // ... but the report to the parent carries only the silent final
        // turn, so it is empty.
        let parent_messages = store.messages("parent-6", false).await.unwrap();
        let reports: Vec<(&str, &str)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } => Some((child_id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            [("child-6", "")],
            "a silent final turn authors an empty report, not stale mid-task text"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_wake_on_an_already_stopped_child_runs_no_second_turn_or_report() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-5")).await.unwrap();
        store
            .create_session(&child_session_of("child-5", "parent-5"))
            .await
            .unwrap();
        store
            .append_message(
                "child-5",
                Role::User,
                &Block::Text {
                    text: "make the change".into(),
                },
            )
            .await
            .unwrap();

        // The second script would run a whole second turn if the wake were
        // let through; the assertions below prove it never ran.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("made the change".into()),
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("a second turn ran".into()),
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
        let handle = spawn_loop("child-5".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-5").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        // Only a parent's message_child may wake a stopped child (it carries
        // the message that resumes it); a stray plain wake must not start
        // another turn or author a second report.
        handle.send(LoopEvent::Wake);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stored = store.get_session("child-5").await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            SessionState::Stopped,
            "a wake on a stopped child starts no turn"
        );
        assert_eq!(
            provider.captured_calls().len(),
            1,
            "a wake on a stopped child runs no model call"
        );
        let parent_messages = store.messages("parent-5", false).await.unwrap();
        let reports: Vec<(&str, &str)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } => Some((child_id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            [("child-5", "made the change")],
            "a wake on a stopped child authors no second report"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_root_that_finishes_waits_for_input_instead_of_reporting() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-1")).await.unwrap();

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
        let handle = spawn_loop("root-1".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the root to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-1").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;
        assert_eq!(
            store.messages("root-1", false).await.unwrap().len(),
            1,
            "a root's completed wake authors nothing extra"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn two_children_run_concurrently_with_independent_in_flight_activity() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-3")).await.unwrap();
        store
            .create_session(&child_session_of("child-a", "parent-3"))
            .await
            .unwrap();
        store
            .create_session(&child_session_of("child-b", "parent-3"))
            .await
            .unwrap();

        // Child a parks in a blocking tool call; child b runs a full turn to
        // completion. Both wake at once: b must finish while a's tool call is
        // still in flight, which proves the two loops do not serialize on a
        // shared mutex or task structure.
        let tools_a = Arc::new(
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
        let provider_a = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-a1".into()),
                name: Some("shell".into()),
                args_delta: r#"{"command":"sleep 100"}"#.into(),
            },
            StreamEvent::Stop {
                input_tokens: 2,
                output_tokens: 1,
            },
        ]]));
        let deps_a = Arc::new(test_deps(
            &store,
            provider_a.clone(),
            tools_a.clone(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle_a = spawn_loop("child-a".into(), deps_a);

        let provider_b = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("b done".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let deps_b = Arc::new(test_deps(
            &store,
            provider_b.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle_b = spawn_loop("child-b".into(), deps_b);

        handle_a.send(LoopEvent::Wake);
        handle_b.send(LoopEvent::Wake);

        wait_for("child a's tool call to be dispatched", {
            let tools_a = tools_a.clone();
            move || {
                let tools_a = tools_a.clone();
                async move {
                    tools_a
                        .calls
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|call| call.name == "shell")
                }
            }
        })
        .await;

        wait_for(
            "child b to stop while child a's tool call is still running",
            || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("child-b").await.unwrap().unwrap();
                    stored.state == SessionState::Stopped
                }
            },
        )
        .await;

        let a = store.get_session("child-a").await.unwrap().unwrap();
        assert_eq!(
            a.state,
            SessionState::Running,
            "child a is still mid-turn while child b completed"
        );
        let a_calls = store.tool_calls("child-a").await.unwrap();
        assert_eq!(a_calls.len(), 1);
        assert!(
            a_calls[0].result.is_none(),
            "child a's in-flight tool call has no result yet"
        );
        let b_messages = store.messages("parent-3", false).await.unwrap();
        assert!(
            b_messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } if child_id == "child-b" && text == "b done"
            )),
            "child b's report is in the parent's thread"
        );
        assert!(
            !b_messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::ChildEvent { child_id, kind: ChildEventKind::Report, .. } if child_id == "child-a")),
            "child a has not reported while still running"
        );

        // Each child metered its own model call under its own loop.
        let calls_b = store.model_calls("child-b").await.unwrap();
        assert_eq!(calls_b.len(), 1);

        handle_a.stop();
        handle_b.stop();
    }

    #[tokio::test]
    async fn spawn_returns_the_child_id_and_the_parent_turn_continues() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-4")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"coder","instructions":"review the diff"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 3,
                    output_tokens: 2,
                },
            ],
            vec![
                StreamEvent::TextDelta("spawned, continuing".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let spawner = Arc::new(FakeSpawner::ok("child-4"));
        let deps = Arc::new(test_deps_with_spawner(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("mock-model", Permission::ReadWrite),
            )]),
            HashMap::from([(
                "mock-model".to_string(),
                provider.clone() as Arc<dyn Provider>,
            )]),
            spawner.clone(),
        ));
        let handle = spawn_loop("parent-4".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the parent's turn to continue past the spawn", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;

        wait_for("the parent to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("parent-4").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        // The spawn call recorded the child id and the turn kept going: the
        // parent never waited for the child to do any work.
        let messages = store.messages("parent-4", false).await.unwrap();
        let result = messages
            .iter()
            .find(|(_, message)| {
                matches!(&message.block, Block::ToolResult { name, .. } if name == "spawn")
            })
            .expect("a spawn result is recorded");
        assert_eq!(result.1.role, Role::User);
        let (is_error, content) = match &result.1.block {
            Block::ToolResult {
                is_error, content, ..
            } => (*is_error, content.clone()),
            _ => unreachable!(),
        };
        assert!(!is_error);
        assert_eq!(content, json!({ "child_id": "child-4" }));

        let requests = spawner.requested();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].parent.id, "parent-4");
        assert_eq!(requests[0].persona_name, "coder");
        assert_eq!(requests[0].instructions, "review the diff");

        let tool_calls = store.tool_calls("parent-4").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "spawn");
        assert_eq!(tool_calls[0].result, Some(json!({ "child_id": "child-4" })));
        assert!(!tool_calls[0].is_error);

        handle.stop();
    }

    #[tokio::test]
    async fn spawn_is_advertised_at_every_depth_when_the_spawner_is_attached() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-5")).await.unwrap();
        store
            .create_session(&child_session_of("child-5", "parent-5"))
            .await
            .unwrap();

        let personas = HashMap::from([(
            "coder".to_string(),
            persona("mock-model", Permission::ReadWrite),
        )]);
        let spawner = Arc::new(FakeSpawner::ok("never-spawned"));

        for (id, role) in [("parent-5", "root"), ("child-5", "child")] {
            let provider = Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ]]));
            let deps = Arc::new(test_deps_with_spawner(
                &store,
                provider.clone(),
                Arc::new(MockTools::new(default_outcome())),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                personas.clone(),
                HashMap::new(),
                spawner.clone(),
            ));
            let handle = spawn_loop(id.to_string(), deps);
            handle.send(LoopEvent::Wake);

            wait_for(&format!("the {role} turn to run"), {
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
            assert!(
                names.contains(&"spawn"),
                "spawn is advertised to the {role} when the spawner is attached: {names:?}"
            );
            handle.stop();
        }

        // With no spawner attached no session sees spawn: the tool needs the
        // control-plane machinery that actually starts child loops. The child
        // stopped at the end of its first wake, so it is set waiting again
        // before its second loop starts.
        store
            .set_state("child-5", SessionState::WaitingForInput)
            .await
            .unwrap();
        for (id, role) in [("parent-5", "root"), ("child-5", "child")] {
            let provider = Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ]]));
            let deps = Arc::new(test_deps_with_personas(
                &store,
                provider.clone(),
                Arc::new(MockTools::new(default_outcome())),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                personas.clone(),
                HashMap::new(),
            ));
            let handle = spawn_loop(id.to_string(), deps);
            handle.send(LoopEvent::Wake);

            wait_for(&format!("the {role} turn to run without a spawner"), {
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
            assert!(
                !names.contains(&"spawn"),
                "spawn is advertised to the {role} only when a spawner is attached: {names:?}"
            );
            handle.stop();
        }
    }

    #[tokio::test]
    async fn a_child_session_spawns_its_own_child() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-6")).await.unwrap();
        store
            .create_session(&child_session_of("child-6", "parent-6"))
            .await
            .unwrap();
        store
            .append_message(
                "child-6",
                Role::User,
                &Block::Text {
                    text: "delegate".into(),
                },
            )
            .await
            .unwrap();

        // The child's model calls spawn; the loop hands the request to the
        // spawner with the child as the parent, so the grandchild is born on
        // the child's node and working copy under its owner.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"coder","instructions":"do it"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("spawned".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let spawner = Arc::new(FakeSpawner::ok("grandchild-6"));
        let deps = Arc::new(test_deps_with_spawner(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("mock-model", Permission::ReadWrite),
            )]),
            HashMap::from([(
                "mock-model".to_string(),
                provider.clone() as Arc<dyn Provider>,
            )]),
            spawner.clone(),
        ));
        let handle = spawn_loop("child-6".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child's turn to wrap up", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-6").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        let requested = spawner.requested();
        assert_eq!(requested.len(), 1, "one spawn reached the spawner");
        assert_eq!(
            requested[0].parent.id, "child-6",
            "the spawning session is the parent of the new child"
        );
        assert_eq!(requested[0].parent.parent_id.as_deref(), Some("parent-6"));
        assert_eq!(requested[0].persona_name, "coder");
        assert_eq!(requested[0].instructions, "do it");

        let messages = store.messages("child-6", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "spawn"
                            && !is_error
                            && content == &json!({ "child_id": "grandchild-6" })
                    )),
            "the child's spawn call returns the grandchild's id"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_child_sessions_failed_spawn_reports_cleanly() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("parent-fail")).await.unwrap();
        store
            .create_session(&child_session_of("child-fail", "parent-fail"))
            .await
            .unwrap();
        store
            .append_message(
                "child-fail",
                Role::User,
                &Block::Text {
                    text: "delegate".into(),
                },
            )
            .await
            .unwrap();

        // A child's spawn that fails (node down, unknown persona) reports the
        // error as the tool result and the wake continues: the failure never
        // takes the child down with it.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"coder","instructions":"do it"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("will do it myself".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let spawner = Arc::new(FakeSpawner::failing("node n1 is not up"));
        let deps = Arc::new(test_deps_with_spawner(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("mock-model", Permission::ReadWrite),
            )]),
            HashMap::from([(
                "mock-model".to_string(),
                provider.clone() as Arc<dyn Provider>,
            )]),
            spawner,
        ));
        let handle = spawn_loop("child-fail".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the child to stop after its own fallback turn", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-fail").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        let messages = store.messages("child-fail", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "spawn"
                            && *is_error
                            && content == &json!({ "error": "node n1 is not up" })
                    )),
            "the child's failed spawn records the spawner's error as the tool result"
        );
        let parent_messages = store.messages("parent-fail", false).await.unwrap();
        let reports: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } if child_id == "child-fail" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            ["will do it myself"],
            "the child's own fallback turn reports normally after the failed spawn"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_failed_spawn_reports_the_spawners_error_as_a_tool_result() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("s-spawn-fail"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"coder","instructions":"do it"}"#.into(),
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
        let spawner = Arc::new(FakeSpawner::failing("node n1 is not up"));
        let deps = Arc::new(test_deps_with_spawner(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("mock-model", Permission::ReadWrite),
            )]),
            HashMap::from([(
                "mock-model".to_string(),
                provider.clone() as Arc<dyn Provider>,
            )]),
            spawner,
        ));
        let handle = spawn_loop("s-spawn-fail".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-spawn-fail").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-spawn-fail", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "spawn"
                            && *is_error
                            && content == &json!({ "error": "node n1 is not up" })
                    )),
            "a failed spawn records the spawner's error as the tool result"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn an_unknown_persona_reports_an_error_result() {
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
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"nope","instructions":"do it"}"#.into(),
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
        let deps = Arc::new(test_deps_with_personas(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("mock-model", Permission::ReadWrite),
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
                .any(|(_, message)| matches!(&message.block, Block::ChildEvent { .. })),
            "no child report is recorded for an unknown persona"
        );
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "spawn"
                            && *is_error
                            && content == &json!({ "error": "unknown persona nope" })
                    )),
            "the unknown persona call records an error result"
        );

        let tool_calls = store.tool_calls("s-subagent-miss").await.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "call-1");
        assert_eq!(tool_calls[0].name, "spawn");
        assert!(tool_calls[0].is_error);
        assert_eq!(
            tool_calls[0].result,
            Some(json!({ "error": "unknown persona nope" }))
        );

        handle.stop();
    }

    #[tokio::test]
    async fn spawn_without_a_configured_provider_reports_an_error_result() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("s-no-provider"))
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"coder","instructions":"do it"}"#.into(),
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
        let deps = Arc::new(test_deps_with_personas(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            HashMap::from([(
                "coder".to_string(),
                persona("ghost-model", Permission::ReadWrite),
            )]),
            HashMap::new(),
        ));
        let handle = spawn_loop("s-no-provider".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the session to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s-no-provider").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("s-no-provider", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, content } if id == "call-1"
                            && name == "spawn"
                            && *is_error
                            && content == &json!({ "error": "no provider for model ghost-model" })
                    )),
            "the spawn call for a model without a provider records an error result"
        );

        handle.stop();
    }

    /// One manifest entry as the system prompt renders it.
    fn manifest_line(id: &str, persona: &str, state: &str, last: &str) -> String {
        format!("- {id} (persona: {persona}, state: {state}, last message: {last})")
    }

    #[tokio::test]
    async fn a_child_completion_authors_an_event_wakes_the_parent_and_the_manifest_tracks_the_child()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t1")).await.unwrap();
        store
            .create_session(&child_session_of("child-t1", "root-t1"))
            .await
            .unwrap();
        store
            .append_message(
                "root-t1",
                Role::User,
                &Block::Text {
                    text: "delegate to the child".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("delegated".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("thanks for the report".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let child_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("made the change".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let root_handle = spawn_loop(
            "root-t1".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                Arc::new(MockTools::new(default_outcome())),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-t1".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                Arc::new(MockTools::new(default_outcome())),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-t1", root_handle.sender.clone());
        mailbox.register("child-t1", child_handle.sender.clone());

        root_handle.send(LoopEvent::Wake);

        wait_for("the parent's first turn to run", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 1 }
            }
        })
        .await;

        // While the child is still working, every parent wake carries its
        // manifest entry: id, persona, state, and no authored message yet.
        let calls = root_provider.captured_calls();
        assert!(
            calls[0]
                .system
                .contains(&manifest_line("child-t1", "coder", "creating", "none")),
            "the first parent turn lists the working child: {}",
            calls[0].system
        );

        // The child completes: its loop authors the event into the parent's
        // thread and wakes the parent's loop through the mailbox.
        child_handle.send(LoopEvent::Wake);
        wait_for("the child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-t1").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        wait_for("the child event to wake a second parent turn", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 2 }
            }
        })
        .await;

        let calls = root_provider.captured_calls();
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && matches!(&message.block, Block::ChildEvent {
                        child_id,
                        kind,
                        text,
                        ..
                    }
                        if child_id == "child-t1"
                            && *kind == ChildEventKind::Report
                            && text == "made the change")),
            "the reaction turn surfaces the child's authored event"
        );
        assert!(
            calls[1].system.contains(&manifest_line(
                "child-t1",
                "coder",
                "stopped",
                "made the change"
            )) || calls[1].system.contains(&manifest_line(
                "child-t1",
                "coder",
                "running",
                "made the change"
            )),
            "the reaction turn's manifest still lists the child with its last message: {}",
            calls[1].system
        );

        wait_for("the parent to wait for input again", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-t1").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let parent_messages = store.messages("root-t1", false).await.unwrap();
        let reports: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } if child_id == "child-t1" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            ["made the change"],
            "the parent's thread holds exactly one authored event"
        );

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn a_stopped_child_leaves_the_manifest_once_handled_and_not_resumed() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t2")).await.unwrap();
        store
            .create_session(&child_session_of("child-t2", "root-t2"))
            .await
            .unwrap();
        // The child completed before this loop starts: it is stopped and its
        // event sits in the parent's thread, not yet surfaced.
        store
            .set_state("child-t2", SessionState::Stopped)
            .await
            .unwrap();
        store
            .append_message(
                "root-t2",
                Role::User,
                &Block::Text {
                    text: "start".into(),
                },
            )
            .await
            .unwrap();
        deliver_child_event(
            &store,
            "root-t2",
            "child-t2",
            ChildEventKind::Report,
            "the work is done",
        )
        .await;

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("noted".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("all done".into()),
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
        let handle = spawn_loop("root-t2".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the wake that surfaces the event to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert!(
            calls[0].system.contains(&manifest_line(
                "child-t2",
                "coder",
                "stopped",
                "the work is done"
            )),
            "the surfacing wake lists the stopped child: {}",
            calls[0].system
        );

        // The next wake — a user message — no longer lists the child: the
        // completion was handled and the parent did not resume it.
        store
            .append_message(
                "root-t2",
                Role::User,
                &Block::Text {
                    text: "any news?".into(),
                },
            )
            .await
            .unwrap();
        handle.send(LoopEvent::Wake);

        wait_for("the user message's turn to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert!(
            !calls[1].system.contains("child-t2"),
            "a handled completion leaves the manifest: {}",
            calls[1].system
        );
        assert!(
            !calls[1].system.contains("Live children:"),
            "no live children remain: {}",
            calls[1].system
        );
        let stored = store.get_session("child-t2").await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            SessionState::Stopped,
            "the child is not resumed by being dropped from the manifest"
        );

        wait_for("the parent to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-t2").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        handle.stop();
    }

    #[tokio::test]
    async fn a_child_event_delivered_mid_turn_is_queued_until_the_turn_ends() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t3")).await.unwrap();
        store
            .create_session(&child_session_of("child-t3", "root-t3"))
            .await
            .unwrap();
        store
            .append_message(
                "root-t3",
                Role::User,
                &Block::Text {
                    text: "start".into(),
                },
            )
            .await
            .unwrap();

        let sink = Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new()))));
        // The delay keeps the first turn in flight while the child event is
        // delivered, so the event has to queue and surface afterwards.
        let provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::TextDelta("working".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("reacted".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(150),
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            sink.clone(),
        ));
        let handle = spawn_loop("root-t3".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the first turn to start", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        // The child completes mid-turn: the event is appended to the parent's
        // thread and the wake is queued, so the in-flight stream is untouched.
        deliver_child_event(
            &store,
            "root-t3",
            "child-t3",
            ChildEventKind::Report,
            "child done",
        )
        .await;
        store
            .set_state("child-t3", SessionState::Stopped)
            .await
            .unwrap();
        handle.send(LoopEvent::Wake);

        wait_for("the first turn's stream to finish", || async {
            sink.0.lock().unwrap().iter().any(|text| text == "working")
        })
        .await;

        wait_for("the queued wake to run a second turn", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert!(
            !calls[0]
                .messages
                .iter()
                .any(|message| matches!(&message.block, Block::ChildEvent { .. })),
            "the in-flight turn's window predates the mid-turn event"
        );
        assert!(
            calls[1].messages.iter().any(
                |message| matches!(&message.block, Block::ChildEvent { child_id, text, .. }
                    if child_id == "child-t3" && text == "child done")
            ),
            "the queued wake's turn surfaces the mid-turn event"
        );
        assert!(
            calls[1]
                .system
                .contains(&manifest_line("child-t3", "coder", "stopped", "child done")),
            "the queued wake's manifest lists the child that finished mid-turn: {}",
            calls[1].system
        );

        wait_for("the parent to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-t3").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("root-t3", false).await.unwrap();
        let texts: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            ["start", "working", "reacted"],
            "the first turn's stream ran to completion before the second turn"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_burst_of_mid_turn_child_events_is_handled_serially() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t4")).await.unwrap();
        store
            .create_session(&child_session_of("child-t4a", "root-t4"))
            .await
            .unwrap();
        store
            .create_session(&child_session_of("child-t4b", "root-t4"))
            .await
            .unwrap();
        store
            .append_message(
                "root-t4",
                Role::User,
                &Block::Text {
                    text: "start".into(),
                },
            )
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::TextDelta("overseeing".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("both handled".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("nothing left".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(150),
        ));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("root-t4".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the first turn to start", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        // Both children complete mid-turn: two events and two wakes queue
        // behind the in-flight turn.
        deliver_child_event(
            &store,
            "root-t4",
            "child-t4a",
            ChildEventKind::Report,
            "a done",
        )
        .await;
        store
            .set_state("child-t4a", SessionState::Stopped)
            .await
            .unwrap();
        deliver_child_event(
            &store,
            "root-t4",
            "child-t4b",
            ChildEventKind::Report,
            "b done",
        )
        .await;
        store
            .set_state("child-t4b", SessionState::Stopped)
            .await
            .unwrap();
        handle.send(LoopEvent::Wake);
        handle.send(LoopEvent::Wake);

        wait_for("both queued wakes to run turns", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 3 }
            }
        })
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 3, "the two queued wakes ran serially");
        for call in &calls[1..] {
            assert!(
                call.messages.iter().any(|message| matches!(
                    &message.block,
                    Block::ChildEvent { child_id, .. }
                        if child_id == "child-t4a" || child_id == "child-t4b"
                )),
                "every queued wake sees the burst events in the thread"
            );
        }
        let first_reaction = &calls[1];
        assert!(
            first_reaction.system.contains(&manifest_line(
                "child-t4a",
                "coder",
                "stopped",
                "a done"
            )) && first_reaction.system.contains(&manifest_line(
                "child-t4b",
                "coder",
                "stopped",
                "b done"
            )),
            "the first reaction wake lists both stopped children: {}",
            first_reaction.system
        );
        assert!(
            !calls[2].system.contains("child-t4a") && !calls[2].system.contains("child-t4b"),
            "children leave the manifest once their completions are handled: {}",
            calls[2].system
        );

        wait_for("the parent to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-t4").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        for child in ["child-t4a", "child-t4b"] {
            let stored = store.get_session(child).await.unwrap().unwrap();
            assert_eq!(
                stored.state,
                SessionState::Stopped,
                "{child} stays stopped: handling never resumes it"
            );
        }

        handle.stop();
    }

    #[tokio::test]
    async fn a_mid_wake_child_event_is_invisible_to_the_wake_that_is_running_and_handled_once_in_its_own()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-mw")).await.unwrap();
        store
            .create_session(&child_session_of("child-mw", "root-mw"))
            .await
            .unwrap();
        store
            .append_message(
                "root-mw",
                Role::User,
                &Block::Text {
                    text: "start".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        // The root's first turn runs a tool; the delay keeps the rest of the
        // same wake from reading a window until the child event has landed
        // mid-wake. The root then has one more turn in this wake, which is
        // the turn that must not see the event.
        let root_provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("shell".into()),
                        args_delta: r#"{"command":"work"}"#.into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("continuing".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("reacting".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("all set".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(200),
        ));
        let child_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("mid-wake done".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let root_handle = spawn_loop(
            "root-mw".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-mw".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-mw", root_handle.sender.clone());
        mailbox.register("child-mw", child_handle.sender.clone());

        root_handle.send(LoopEvent::Wake);

        wait_for("the root's first turn to start", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 1 }
            }
        })
        .await;

        // The child completes while the root's first turn is still in
        // flight: its event lands mid-wake and its own wake queues behind
        // the turns of the running wake.
        child_handle.send(LoopEvent::Wake);
        wait_for("the child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-mw").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        wait_for("the running wake's turns and the queued wake to run", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 3 }
            }
        })
        .await;

        let calls = root_provider.captured_calls();
        assert_eq!(
            calls.len(),
            3,
            "the wake and the queued wake ran their turns"
        );
        let sees_event = |call: &CapturedCall| {
            call.messages.iter().any(|message| {
                matches!(
                    &message.block,
                    Block::ChildEvent {
                        child_id,
                        kind: ChildEventKind::Report,
                        text,
                        ..
                    } if child_id == "child-mw" && text == "mid-wake done"
                )
            })
        };
        assert!(
            !sees_event(&calls[0]) && !sees_event(&calls[1]),
            "no turn of the wake the event landed in may see it: {:#?}",
            calls
                .iter()
                .take(2)
                .map(|call| &call.messages)
                .collect::<Vec<_>>()
        );
        assert!(
            sees_event(&calls[2]),
            "the event's own queued wake surfaces the child's report once: {:#?}",
            calls[2].messages
        );
        assert!(
            calls[2].system.contains(&manifest_line(
                "child-mw",
                "coder",
                "stopped",
                "mid-wake done"
            )),
            "the event's own wake lists the child with the event: {}",
            calls[2].system
        );

        // The queued wake handled the completion and did not resume the
        // child, so the next user wake no longer lists it.
        store
            .append_message(
                "root-mw",
                Role::User,
                &Block::Text {
                    text: "anything else?".into(),
                },
            )
            .await
            .unwrap();
        root_handle.send(LoopEvent::Wake);
        wait_for("the user wake's turn to run", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 4 }
            }
        })
        .await;

        let calls = root_provider.captured_calls();
        assert!(
            !calls[3].system.contains("child-mw"),
            "the child leaves the manifest once its event was handled: {}",
            calls[3].system
        );

        let parent_messages = store.messages("root-mw", false).await.unwrap();
        let reports: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } if child_id == "child-mw" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            ["mid-wake done"],
            "the parent's thread holds exactly one authored event"
        );

        wait_for("the root to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-mw").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn compaction_mid_wake_archives_only_the_wake_snapshot_and_never_the_event_that_landed_behind_it()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-cp")).await.unwrap();
        // 20 messages: at the window limit, so compaction triggers only once
        // this wake's own turns have appended past it.
        for index in 0..10 {
            store
                .append_message(
                    "root-cp",
                    Role::User,
                    &Block::Text {
                        text: format!("user {index}"),
                    },
                )
                .await
                .unwrap();
            store
                .append_message(
                    "root-cp",
                    Role::Assistant,
                    &Block::Text {
                        text: format!("assistant {index}"),
                    },
                )
                .await
                .unwrap();
        }

        let provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("shell".into()),
                        args_delta: r#"{"command":"work"}"#.into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("compacted the tail".into()),
                    StreamEvent::Stop {
                        input_tokens: 200,
                        output_tokens: 20,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("continuing".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("reacting".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(200),
        ));
        let deps = Arc::new(test_deps_with_max_window(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            20,
        ));
        let handle = spawn_loop("root-cp".into(), deps);

        handle.send(LoopEvent::Wake);

        wait_for("the first turn to start", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        // The child event lands mid-wake, while the first turn is in flight;
        // the event's wake is queued behind the running wake.
        deliver_child_event(
            &store,
            "root-cp",
            "child-cp",
            ChildEventKind::Report,
            "done mid-wake",
        )
        .await;
        handle.send(LoopEvent::Wake);

        // The running wake then continues: its second turn exceeds the
        // window limit and compacts the snapshot tail, and the queued wake
        // runs the event's own turn afterwards. Call 1 is the summarizer.
        wait_for(
            "the second turn, its compaction, and the queued wake to run",
            {
                let provider = provider.clone();
                move || {
                    let provider = provider.clone();
                    async move { provider.captured_calls().len() == 4 }
                }
            },
        )
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 4, "turn, summarizer, second turn, event wake");
        let sees_event = |call: &CapturedCall| {
            call.messages.iter().any(|message| {
                matches!(
                    &message.block,
                    Block::ChildEvent {
                        child_id,
                        kind: ChildEventKind::Report,
                        text,
                        ..
                    } if child_id == "child-cp" && text == "done mid-wake"
                )
            })
        };
        assert!(
            !sees_event(&calls[0]) && !sees_event(&calls[2]),
            "the compacting wake's turns never see the mid-wake event: {:#?}",
            calls[2].messages
        );
        assert!(
            calls[1]
                .messages
                .iter()
                .any(|message| message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text.contains("assistant 4"))),
            "the summarizer retires the snapshot tail: {:#?}",
            calls[1].messages
        );
        assert!(
            sees_event(&calls[3]),
            "the event surfaces in its own queued wake, unarchived: {:#?}",
            calls[3].messages
        );

        let active = store.messages("root-cp", false).await.unwrap();
        assert!(
            active.iter().any(|(_, message)| {
                matches!(&message.block, Block::ChildEvent { child_id, text, .. }
                    if child_id == "child-cp" && text == "done mid-wake")
            }),
            "compaction never archives a mid-wake event before its own wake"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn message_child_resumes_a_stopped_child_which_reports_again() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t5")).await.unwrap();
        store
            .create_session(&child_session_of("child-t5", "root-t5"))
            .await
            .unwrap();
        store
            .append_message(
                "child-t5",
                Role::User,
                &Block::Text {
                    text: "review the change".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"child-t5","text":"give me more detail"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("thanks".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("good detail".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let child_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("first findings".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("here is the detail".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let root_handle = spawn_loop(
            "root-t5".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                Arc::new(MockTools::new(default_outcome())),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-t5".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                Arc::new(MockTools::new(default_outcome())),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-t5", root_handle.sender.clone());
        mailbox.register("child-t5", child_handle.sender.clone());

        // The child completes its assignment first: it stops, and its report
        // wakes the parent. The parent's model then calls message_child on the
        // stopped child, which is what resumes it.
        child_handle.send(LoopEvent::Wake);
        wait_for("the child to stop after its first report", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-t5").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        wait_for("the resumed child to answer and stop again", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-t5").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
                    && store.messages("child-t5", false).await.unwrap().len() == 4
            }
        })
        .await;

        wait_for("the parent's three turns to finish", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 3 }
            }
        })
        .await;

        // The child resumed from its archived thread: its transcript is the
        // assignment, its first answer, the parent's message, and its second
        // answer, in that order.
        let child_messages = store.messages("child-t5", false).await.unwrap();
        let texts: Vec<&str> = child_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [
                "review the change",
                "first findings",
                "give me more detail",
                "here is the detail"
            ],
            "the parent's message lands in the child's archived thread"
        );

        // The parent's thread holds both authored reports, and the parent
        // recorded a successful message_child result.
        let parent_messages = store.messages("root-t5", false).await.unwrap();
        let reports: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } if child_id == "child-t5" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            ["first findings", "here is the detail"],
            "each completion authors one report into the parent's thread"
        );
        assert!(
            parent_messages.iter().any(|(_, message)| {
                matches!(
                    &message.block,
                    Block::ToolResult { name, is_error, content, .. }
                        if name == "message_child"
                            && !is_error
                            && content == &json!({ "ok": true })
                )
            }),
            "the parent's message_child call records a success result"
        );
        assert_eq!(
            child_provider.captured_calls().len(),
            2,
            "the resumed child ran exactly one more turn"
        );

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn message_child_reaches_a_running_child_which_reads_it_only_after_its_wake_and_stays_in_the_manifest()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-mr")).await.unwrap();
        store
            .create_session(&child_session_of("child-mr", "root-mr"))
            .await
            .unwrap();
        store
            .append_message(
                "root-mr",
                Role::User,
                &Block::Text {
                    text: "check on the child".into(),
                },
            )
            .await
            .unwrap();
        store
            .append_message(
                "child-mr",
                Role::User,
                &Block::Text {
                    text: "review the change".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        // The child's first turn runs a tool; the delay keeps the child
        // running while the parent's message lands behind it. The child then
        // finishes its own wake before reading the message, in the wake the
        // parent message started.
        let child_provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("shell".into()),
                        args_delta: r#"{"command":"review"}"#.into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("task done".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("here is the detail".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(200),
        ));
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"child-mr","text":"send a progress update"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("requested".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("done noted".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("detail noted".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("all set".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let root_handle = spawn_loop(
            "root-mr".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-mr".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-mr", root_handle.sender.clone());
        mailbox.register("child-mr", child_handle.sender.clone());

        // The child starts its assignment first, so it is running mid-wake
        // when the parent messages it.
        child_handle.send(LoopEvent::Wake);
        wait_for("the child's first turn to start", {
            let child_provider = child_provider.clone();
            move || {
                let child_provider = child_provider.clone();
                async move { child_provider.captured_calls().len() == 1 }
            }
        })
        .await;

        root_handle.send(LoopEvent::Wake);

        wait_for(
            "the child to finish its wake, read the message, and stop again",
            || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("child-mr").await.unwrap().unwrap();
                    stored.state == SessionState::Stopped
                        && store.messages("child-mr", false).await.unwrap().len() == 6
                }
            },
        )
        .await;

        wait_for("the parent to react to both reports", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 4 }
            }
        })
        .await;

        // A user message after the exchange: the answer's wake handled the
        // child and did not resume it, so the child leaves the manifest.
        store
            .append_message(
                "root-mr",
                Role::User,
                &Block::Text {
                    text: "anything else?".into(),
                },
            )
            .await
            .unwrap();
        root_handle.send(LoopEvent::Wake);
        wait_for("the final user wake's turn to run", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 5 }
            }
        })
        .await;

        // The child read the parent's message only in the wake it started:
        // its own in-flight wake finished its turns without seeing it.
        let child_calls = child_provider.captured_calls();
        assert_eq!(
            child_calls.len(),
            3,
            "task turn, finish turn, and answer turn"
        );
        assert!(
            !child_calls[0]
                .messages
                .iter()
                .chain(child_calls[1].messages.iter())
                .any(|message| matches!(&message.block, Block::Text { text } if text == "send a progress update")),
            "the child's running wake never saw the parent's message: {:#?}",
            child_calls[1].messages
        );
        assert!(
            child_calls[2]
                .messages
                .iter()
                .any(|message| matches!(&message.block, Block::Text { text } if text == "send a progress update")),
            "the message wake's turn sees the parent's message: {:#?}",
            child_calls[2].messages
        );

        // The parent's message landed in the child's archived thread between
        // the assignment and the child's own words.
        let child_messages = store.messages("child-mr", false).await.unwrap();
        let texts: Vec<&str> = child_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [
                "review the change",
                "send a progress update",
                "task done",
                "here is the detail"
            ],
            "the message and both answers sit in the child's thread"
        );

        // The parent tracked the child through the whole exchange: the wake
        // that reacted to each report listed it, and it left the manifest
        // only after the final report was handled without a resume.
        let calls = root_provider.captured_calls();
        assert_eq!(calls.len(), 5);
        // The child may already be running again (resumed by its queued
        // parent message) or still stopped when the wake snapshots.
        assert!(
            calls[2]
                .system
                .contains(&manifest_line("child-mr", "coder", "running", "task done"))
                || calls[2].system.contains(&manifest_line(
                    "child-mr",
                    "coder",
                    "stopped",
                    "task done"
                )),
            "the report after the message still lists the child: {}",
            calls[2].system
        );
        assert!(
            calls[3].system.contains(&manifest_line(
                "child-mr",
                "coder",
                "stopped",
                "here is the detail"
            )) || calls[3].system.contains(&manifest_line(
                "child-mr",
                "coder",
                "running",
                "here is the detail"
            )),
            "the answer's wake still lists the child: {}",
            calls[3].system
        );
        assert!(
            !calls[4].system.contains("child-mr"),
            "the child leaves the manifest once its answer was handled: {}",
            calls[4].system
        );

        let parent_messages = store.messages("root-mr", false).await.unwrap();
        assert!(
            parent_messages.iter().any(|(_, message)| {
                matches!(
                    &message.block,
                    Block::ToolResult { name, is_error, content, .. }
                        if name == "message_child"
                            && !is_error
                            && content == &json!({ "ok": true })
                )
            }),
            "messaging a running child succeeds"
        );

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn message_child_is_advertised_to_any_session_with_a_mailbox_attached() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t6")).await.unwrap();
        store
            .create_session(&child_session_of("child-t6", "root-t6"))
            .await
            .unwrap();
        store
            .create_session(&child_session_of("child-t6b", "root-t6"))
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());

        for (id, name) in [
            ("root-t6", "root with mailbox"),
            ("root-t6", "root without mailbox"),
            ("child-t6", "child with mailbox"),
            // A second child: the first child's wake stopped it (a child ends
            // by reporting and stopping), and a stopped session runs no
            // further wake.
            ("child-t6b", "child without mailbox"),
        ] {
            let provider = Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ]]));
            let deps = if name.contains("without mailbox") {
                test_deps(
                    &store,
                    provider.clone(),
                    Arc::new(MockTools::new(default_outcome())),
                    Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                )
            } else {
                test_deps_with_mailbox(
                    &store,
                    provider.clone(),
                    Arc::new(MockTools::new(default_outcome())),
                    Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                    mailbox.clone(),
                )
            };
            let handle = spawn_loop(id.to_string(), Arc::new(deps));
            handle.send(LoopEvent::Wake);

            wait_for(&format!("the {name} turn to run"), {
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
            assert_eq!(
                names.contains(&"message_child"),
                name.contains("with mailbox"),
                "message_child is advertised to the {name} only when a mailbox is attached: {names:?}"
            );
            handle.stop();
        }
    }

    #[tokio::test]
    async fn message_child_refuses_when_gated_or_aimed_at_a_non_child() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-t7")).await.unwrap();
        store
            .create_session(&child_session_of("my-child", "root-t7"))
            .await
            .unwrap();
        store
            .create_session(&session("other-parent"))
            .await
            .unwrap();
        store
            .create_session(&child_session_of("other-child", "other-parent"))
            .await
            .unwrap();

        // A root with a mailbox attached: messaging another parent's child or
        // an unknown id is refused; messaging its own child lands the message
        // in the child's thread and succeeds even with no child loop running.
        let mailbox = Arc::new(TestMailbox::new());
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"other-child","text":"hi"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"ghost","text":"hi"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-3".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"my-child","text":"please expand"}"#.into(),
                },
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
        let deps = Arc::new(test_deps_with_mailbox(
            &store,
            provider.clone(),
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            mailbox.clone(),
        ));
        let handle = spawn_loop("root-t7".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the root's turns to finish", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-t7").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let results = store.tool_calls("root-t7").await.unwrap();
        let error_of = |id: &str| {
            results
                .iter()
                .find(|call| call.call_id == id)
                .and_then(|call| call.result.as_ref())
                .and_then(|result| result["error"].as_str())
                .map(str::to_string)
        };
        assert_eq!(
            error_of("call-1").as_deref(),
            Some("session other-child is not a child of this session")
        );
        assert_eq!(
            error_of("call-2").as_deref(),
            Some("no child session ghost")
        );
        assert_eq!(
            error_of("call-3").as_deref(),
            None,
            "own-child call succeeds"
        );
        let other_messages = store.messages("other-child", false).await.unwrap();
        assert!(
            other_messages.is_empty(),
            "a refused message never reaches the foreign child's thread"
        );
        let my_messages = store.messages("my-child", false).await.unwrap();
        assert!(
            my_messages.iter().any(|(_, message)| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == "please expand")
            }),
            "a successful message_child lands in its child's thread"
        );
        handle.stop();

        // A child session messages its own children the same way a root
        // does: its own child's thread receives the message, and a foreign
        // child or an unknown id is refused.
        store
            .create_session(&child_session_of("child-t7", "root-t7"))
            .await
            .unwrap();
        let mut nested = child_session_of("nested-t7", "child-t7");
        nested.owner_id = "root-t7".into();
        store.create_session(&nested).await.unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-9".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"my-child","text":"hi"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-10".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"ghost","text":"hi"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-11".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"nested-t7","text":"please expand"}"#.into(),
                },
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
        let deps = Arc::new(test_deps_with_mailbox(
            &store,
            provider,
            Arc::new(MockTools::new(default_outcome())),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            mailbox.clone(),
        ));
        let handle = spawn_loop("child-t7".into(), deps);
        handle.send(LoopEvent::Wake);

        // The child's own child `nested-t7` is still live, so the child
        // waits for it instead of reporting and stopping: a session that has
        // children supervises them until they resolve.
        wait_for(
            "the child's turn to wrap up and wait for its own child",
            || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("child-t7").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                }
            },
        )
        .await;
        let stored = store.get_session("child-t7").await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            SessionState::WaitingForInput,
            "a child with a live child of its own supervises instead of stopping"
        );
        let parent_messages = store.messages("root-t7", false).await.unwrap();
        assert!(
            !parent_messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    ..
                } if child_id == "child-t7"
            )),
            "the supervising child authors no completion report yet"
        );

        let results = store.tool_calls("child-t7").await.unwrap();
        let error_of = |id: &str| {
            results
                .iter()
                .find(|call| call.call_id == id)
                .and_then(|call| call.result.as_ref())
                .and_then(|result| result["error"].as_str())
                .map(str::to_string)
        };
        assert_eq!(
            error_of("call-9").as_deref(),
            Some("session my-child is not a child of this session")
        );
        assert_eq!(
            error_of("call-10").as_deref(),
            Some("no child session ghost")
        );
        assert_eq!(
            error_of("call-11").as_deref(),
            None,
            "a child messaging its own child succeeds"
        );
        let nested_messages = store.messages("nested-t7", false).await.unwrap();
        assert!(
            nested_messages.iter().any(|(_, message)| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == "please expand")
            }),
            "the grandchild's thread received the message"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_child_ask_authors_an_ask_event_to_its_parent_and_the_child_waits() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6ask")).await.unwrap();
        store
            .create_session(&child_session_of("child-s6ask", "root-s6ask"))
            .await
            .unwrap();
        store
            .append_message(
                "child-s6ask",
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();

        // The child's turn calls ask: instead of hanging, its wake must end
        // with an authored Ask event in the parent's thread and the child
        // waiting for the parent's answer.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".into()),
                name: Some("ask".into()),
                args_delta: r#"{"message":"may I push?","options":["yes","no"]}"#.into(),
            },
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("child-s6ask".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the child to wait for the parent's answer", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-s6ask").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let stored = store.get_session("child-s6ask").await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            SessionState::WaitingForInput,
            "a child that asks waits for its parent instead of stopping"
        );
        assert_eq!(
            provider.captured_calls().len(),
            1,
            "the ask ended the child's wake"
        );
        let calls = provider.captured_calls();
        let child_tools: Vec<&str> = calls[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            child_tools.contains(&"ask"),
            "ask is advertised to a child so it can raise a question to its parent: {child_tools:?}"
        );

        let parent_messages = store.messages("root-s6ask", false).await.unwrap();
        let events: Vec<(&str, &str, &str)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind,
                    text,
                    ..
                } => Some((child_id.as_str(), kind.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            events,
            [("child-s6ask", "ask", "may I push?")],
            "the child's ask reaches the parent as one Ask event, not a report"
        );

        // The child's own thread keeps the question, so the parent's answer
        // resumes it with its question in context.
        let child_messages = store.messages("child-s6ask", false).await.unwrap();
        assert!(
            child_messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Ask { message, .. } if message == "may I push?")),
            "the child's own thread records the question it asked"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_root_answers_a_child_ask_and_the_child_resumes_and_finishes() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6ans")).await.unwrap();
        store
            .create_session(&child_session_of("child-s6ans", "root-s6ans"))
            .await
            .unwrap();
        store
            .append_message(
                "child-s6ans",
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        // The root answers on the user's behalf: its wake for the child's Ask
        // event calls message_child with the answer, and its later wake
        // handles the child's completion report.
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"child-s6ans","text":"yes, push to main"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("answered the child".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("noted the report".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        // The child asks its first turn, then resumes from its own thread
        // when the parent's answer arrives and finishes with a report.
        let child_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","options":["yes","no"]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("pushed to main".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let root_handle = spawn_loop(
            "root-s6ans".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-s6ans".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s6ans", root_handle.sender.clone());
        mailbox.register("child-s6ans", child_handle.sender.clone());

        child_handle.send(LoopEvent::Wake);

        wait_for("the child to finish after the parent's answer", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-s6ans").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
                    && store.messages("child-s6ans", false).await.unwrap().len() == 5
            }
        })
        .await;
        wait_for("the root's turns to finish", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 3 }
            }
        })
        .await;
        wait_for("the root to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-s6ans").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        // The parent's thread holds the child's Ask event and, after the
        // answer, its completion report — and no surfaced ask block.
        let parent_messages = store.messages("root-s6ans", false).await.unwrap();
        let events: Vec<(&str, &str)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind,
                    text,
                    ..
                } if child_id == "child-s6ans" => Some((kind.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            events,
            [("ask", "may I push?"), ("report", "pushed to main")],
            "the answer resumes the child, whose completion reports back"
        );
        assert!(
            !parent_messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Ask { .. })),
            "answering on the user's behalf never surfaces the question"
        );

        // The child resumed from its own thread: its second turn reads its
        // question and the parent's answer, and the answer landed verbatim in
        // its archived thread.
        let child_calls = child_provider.captured_calls();
        assert_eq!(child_calls.len(), 2);
        assert!(
            child_calls
                .iter()
                .all(|call| call.ask_recipient == AskRecipient::Parent),
            "a child session's asks are rendered as questions to its parent, not the user"
        );
        assert!(
            child_calls[1].messages.iter().any(|message| {
                matches!(&message.block, Block::Ask { message, .. } if message == "may I push?")
            }),
            "the resumed child still reads the question it asked: {:#?}",
            child_calls[1].messages
        );
        assert!(
            child_calls[1].messages.iter().any(|message| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == "yes, push to main")
            }),
            "the parent's answer is the child's next user message: {:#?}",
            child_calls[1].messages
        );

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn a_root_denies_a_child_ask_and_the_child_resumes_and_re_asks() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6den")).await.unwrap();
        store
            .create_session(&child_session_of("child-s6den", "root-s6den"))
            .await
            .unwrap();
        store
            .append_message(
                "child-s6den",
                Role::User,
                &Block::Text {
                    text: "push the change".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        let denial = "denied: never push to main directly; use a branch";
        // The root denies with a reason; the child resumes from its thread,
        // reads the denial, and asks a fresh question instead.
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: format!(r#"{{"id":"child-s6den","text":"{denial}"}}"#),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("denied the child".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("the re-ask stays pending".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let child_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to main?","options":["yes","no"]}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to a branch instead?"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let root_handle = spawn_loop(
            "root-s6den".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-s6den".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s6den", root_handle.sender.clone());
        mailbox.register("child-s6den", child_handle.sender.clone());

        child_handle.send(LoopEvent::Wake);

        wait_for("the child to ask again after the denial", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-s6den").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
                    && store
                        .messages("root-s6den", false)
                        .await
                        .unwrap()
                        .iter()
                        .filter(|(_, message)| {
                            matches!(
                                &message.block,
                                Block::ChildEvent {
                                    kind: ChildEventKind::Ask,
                                    ..
                                }
                            )
                        })
                        .count()
                        == 2
            }
        })
        .await;
        wait_for("the root's turns to finish", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 3 }
            }
        })
        .await;

        // Each question the child asks authors exactly one Ask event.
        let parent_messages = store.messages("root-s6den", false).await.unwrap();
        let asks: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Ask,
                    text,
                    ..
                } if child_id == "child-s6den" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            asks,
            ["may I push to main?", "may I push to a branch instead?"],
            "a re-ask is a new Ask event, not a replay of the old one"
        );

        // The denial reached the child: its second turn reads its original
        // question and the parent's denial, then chose to re-ask.
        let child_calls = child_provider.captured_calls();
        assert_eq!(child_calls.len(), 2);
        assert!(
            child_calls[1].messages.iter().any(|message| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == denial)
            }),
            "the denial is the message that resumed the child: {:#?}",
            child_calls[1].messages
        );

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn a_root_surfacing_a_child_ask_binds_the_leaf_durably_and_waits_for_the_answer() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6sur")).await.unwrap();
        store
            .create_session(&child_session_of("child-s6sur", "root-s6sur"))
            .await
            .unwrap();
        store
            .append_message(
                "child-s6sur",
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        // The root surfaces the child's question to the user with the child's
        // id bound; the user's answer is routed by the control plane, not by
        // another root turn, so the root's wake ends here.
        let root_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".into()),
                name: Some("ask".into()),
                args_delta:
                    r#"{"message":"may I push?","options":["yes","no"],"child_id":"child-s6sur"}"#
                        .into(),
            },
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let child_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".into()),
                name: Some("ask".into()),
                args_delta: r#"{"message":"may I push?","options":["yes","no"]}"#.into(),
            },
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let root_handle = spawn_loop(
            "root-s6sur".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-s6sur".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s6sur", root_handle.sender.clone());
        mailbox.register("child-s6sur", child_handle.sender.clone());

        child_handle.send(LoopEvent::Wake);

        wait_for("the root to surface the ask and record the binding", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("root-s6sur").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.get_pending_ask("root-s6sur").await.unwrap().is_some()
                }
            }
        })
        .await;
        let root_calls = root_provider.captured_calls();
        assert_eq!(root_calls.len(), 1, "the surface is the root's only turn");

        // The surfaced ask is bound to the child: one bound Ask block, one
        // ask tool call naming the child, and a durable binding record that
        // names the surfaced Ask block's row, so a later compaction cannot
        // lose the binding.
        let parent_messages = store.messages("root-s6sur", false).await.unwrap();
        let bound_asks: Vec<(&str, &str, i64)> = parent_messages
            .iter()
            .filter_map(|(id, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: Some(child_id),
                    ..
                } => Some((child_id.as_str(), message.as_str(), *id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            bound_asks,
            [(
                "child-s6sur",
                "may I push?",
                parent_messages.last().unwrap().0
            )],
            "the surfaced ask is bound to the child that asked"
        );
        let pending = store.get_pending_ask("root-s6sur").await.unwrap().unwrap();
        assert_eq!(pending.session_id, "root-s6sur");
        assert_eq!(pending.child_id, "child-s6sur");
        assert_eq!(pending.question, "may I push?");
        assert_eq!(
            pending.ask_message_id, bound_asks[0].2,
            "the binding names the surfaced Ask block's row"
        );
        let root_asks: Vec<_> = store
            .tool_calls("root-s6sur")
            .await
            .unwrap()
            .into_iter()
            .filter(|call| call.name == "ask")
            .collect();
        assert_eq!(root_asks.len(), 1, "one question is surfaced exactly once");
        assert_eq!(
            root_asks[0].args["child_id"], "child-s6sur",
            "the ask tool call carries the bound child"
        );

        // The child waits on the answer with its own question still at the
        // end of its thread; nothing has resumed it yet.
        let stored = store.get_session("child-s6sur").await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::WaitingForInput);
        let child_calls = child_provider.captured_calls();
        assert_eq!(child_calls.len(), 1, "the child asked once and waits");

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn when_the_user_redirects_the_root_cancels_the_pending_ask_and_notifies_the_child() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6red")).await.unwrap();
        store
            .create_session(&child_session_of("child-s6red", "root-s6red"))
            .await
            .unwrap();
        store
            .append_message(
                "child-s6red",
                Role::User,
                &Block::Text {
                    text: "push the change".into(),
                },
            )
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        let cancel_notice = "the user redirected instead of answering; your question is cancelled. Re-ask, adapt, or stop.";
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to main?","options":["yes","no"],"child_id":"child-s6red"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("message_child".into()),
                    args_delta: format!(r#"{{"id":"child-s6red","text":"{cancel_notice}"}}"#),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("cancelled the ask".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("noted the report".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let child_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to main?","options":["yes","no"]}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("understood — stopping the push".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let root_handle = spawn_loop(
            "root-s6red".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-s6red".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s6red", root_handle.sender.clone());
        mailbox.register("child-s6red", child_handle.sender.clone());

        child_handle.send(LoopEvent::Wake);

        wait_for("the root to surface the ask and record the binding", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("root-s6red").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.get_pending_ask("root-s6red").await.unwrap().is_some()
                }
            }
        })
        .await;
        let pending = store.get_pending_ask("root-s6red").await.unwrap().unwrap();
        assert_eq!(pending.child_id, "child-s6red");

        // The user redirects instead of answering: the root's next turn
        // decides the pending ask's fate and cancels it. The binding stays
        // pending until the root takes the ask over, so the root wakes with
        // the surfaced ask still bound.
        store
            .append_message(
                "root-s6red",
                Role::User,
                &Block::Text {
                    text: "stop the push attempt and review the README instead".into(),
                },
            )
            .await
            .unwrap();
        root_handle.send(LoopEvent::Wake);

        wait_for("the child to be notified and stop", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-s6red").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;
        wait_for("the root's turns to finish", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 4 }
            }
        })
        .await;
        wait_for("the pending binding to be cleared by the cancel", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move { store.get_pending_ask("root-s6red").await.unwrap().is_none() }
            }
        })
        .await;

        // The original question was surfaced exactly once; the redirect never
        // surfaced it again.
        let parent_messages = store.messages("root-s6red", false).await.unwrap();
        let bound_asks: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: Some(child_id),
                    ..
                } if child_id == "child-s6red" => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            bound_asks,
            ["may I push to main?"],
            "a cancelled ask is never surfaced a second time"
        );
        let root_asks: Vec<_> = store
            .tool_calls("root-s6red")
            .await
            .unwrap()
            .into_iter()
            .filter(|call| call.name == "ask")
            .collect();
        assert_eq!(root_asks.len(), 1, "one ask tool call for one question");

        // The cancellation notice reached the waiting child, which resumed
        // from its own thread and chose to stop.
        let child_calls = child_provider.captured_calls();
        assert_eq!(child_calls.len(), 2);
        assert!(
            child_calls[1].messages.iter().any(|message| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == cancel_notice)
            }),
            "the cancelled child reads the notice in its own thread: {:#?}",
            child_calls[1].messages
        );
        let child_messages = store.messages("child-s6red", false).await.unwrap();
        let child_texts: Vec<&str> = child_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            child_texts.contains(&"understood — stopping the push"),
            "the notified child reports its decision: {child_texts:?}"
        );

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn binding_to_a_child_without_a_pending_question_is_refused() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6np")).await.unwrap();
        // A running child, a stopped child that finished, and a child waiting
        // on nothing: none of them has an unanswered question of its own at
        // the end of its thread, so none may be bound.
        for (id, state) in [
            ("runner-s6np", SessionState::Running),
            ("done-s6np", SessionState::Stopped),
            ("waiter-s6np", SessionState::WaitingForInput),
        ] {
            store
                .create_session(&child_session_of(id, "root-s6np"))
                .await
                .unwrap();
            store
                .append_message(
                    id,
                    Role::User,
                    &Block::Text {
                        text: "do the work".into(),
                    },
                )
                .await
                .unwrap();
            store.set_state(id, state).await.unwrap();
        }
        store
            .append_message(
                "done-s6np",
                Role::Assistant,
                &Block::Text {
                    text: "all done".into(),
                },
            )
            .await
            .unwrap();

        // The root tries to surface a question for each child in turn; every
        // binding is refused and the wake continues, so the model can recover.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"runner-s6np"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"done-s6np"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-3".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"waiter-s6np"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("noted".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("root-s6np".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the refused bindings' wake to end", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-s6np").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let results = store.tool_calls("root-s6np").await.unwrap();
        let error_of = |id: &str| {
            results
                .iter()
                .find(|call| call.call_id == id)
                .and_then(|call| call.result.as_ref())
                .and_then(|result| result["error"].as_str())
                .map(str::to_string)
        };
        assert_eq!(
            error_of("call-1").as_deref(),
            Some("child session runner-s6np has no pending question to surface")
        );
        assert_eq!(
            error_of("call-2").as_deref(),
            Some("child session done-s6np has no pending question to surface")
        );
        assert_eq!(
            error_of("call-3").as_deref(),
            Some("child session waiter-s6np has no pending question to surface")
        );
        let messages = store.messages("root-s6np", false).await.unwrap();
        assert!(
            !messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Ask { .. })),
            "a refused binding records no surfaced ask"
        );
        assert!(
            store.get_pending_ask("root-s6np").await.unwrap().is_none(),
            "a refused binding records no pending ask"
        );
        // A refused ask continues the turn, so its error result must keep its
        // tool use in the next request: the scripted provider never checks,
        // but a real one rejects a dangling tool result with a 400.
        let calls = provider.captured_calls();
        assert_eq!(
            calls.len(),
            4,
            "three refused bindings, then the recovery turn"
        );
        for call in &calls {
            assert_tool_results_have_matching_tool_uses(call);
        }
        for id in ["call-1", "call-2", "call-3"] {
            assert!(
                calls[3].messages.iter().any(|message| matches!(
                    &message.block,
                    Block::ToolCall { id: use_id, name, .. } if use_id == id && name == "ask"
                )),
                "the refused ask {id} keeps its tool use in the recovery turn's window"
            );
        }

        handle.stop();
    }

    #[tokio::test]
    async fn surfacing_a_second_question_while_one_is_pending_is_refused() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6two")).await.unwrap();
        store
            .create_session(&child_session_of("child-a-s6two", "root-s6two"))
            .await
            .unwrap();
        // Child B asked too, but its event is injected: only child A's
        // question is surfaced, and the root must resolve it before another
        // question can reach the user.
        store
            .create_session(&child_session_of("child-b-s6two", "root-s6two"))
            .await
            .unwrap();
        store
            .append_message(
                "child-b-s6two",
                Role::Assistant,
                &Block::Ask {
                    message: "may I refactor?".into(),
                    options: vec![],
                    child_id: None,
                    answer: None,
                },
            )
            .await
            .unwrap();
        store
            .set_state("child-b-s6two", SessionState::WaitingForInput)
            .await
            .unwrap();

        let mailbox = Arc::new(TestMailbox::new());
        // The root surfaces child A's question, then — woken by child B's ask
        // event — tries to surface B's too. One question reaches the user at a
        // time, so the second surface is refused with the pending one named.
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"child-a-s6two"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I refactor?","child_id":"child-b-s6two"}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("waiting on the user".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let child_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".into()),
                name: Some("ask".into()),
                args_delta: r#"{"message":"may I push?","options":["yes","no"]}"#.into(),
            },
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let root_handle = spawn_loop(
            "root-s6two".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let child_handle = spawn_loop(
            "child-a-s6two".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s6two", root_handle.sender.clone());
        mailbox.register("child-a-s6two", child_handle.sender.clone());

        child_handle.send(LoopEvent::Wake);
        wait_for("child A to ask and the root to surface it", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("root-s6two").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.get_pending_ask("root-s6two").await.unwrap().is_some()
                }
            }
        })
        .await;

        deliver_child_event(
            &store,
            "root-s6two",
            "child-b-s6two",
            ChildEventKind::Ask,
            "may I refactor?",
        )
        .await;
        root_handle.send(LoopEvent::Wake);

        wait_for("the refused second surface's wake to end", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 3 }
            }
        })
        .await;

        let results = store.tool_calls("root-s6two").await.unwrap();
        let second = results
            .iter()
            .find(|call| call.call_id == "call-2")
            .and_then(|call| call.result.as_ref())
            .and_then(|result| result["error"].as_str())
            .map(str::to_string);
        assert_eq!(
            second.as_deref(),
            Some(
                "another question is pending with the user; send child child-a-s6two a message to cancel it before asking again"
            )
        );
        let pending = store.get_pending_ask("root-s6two").await.unwrap().unwrap();
        assert_eq!(
            pending.child_id, "child-a-s6two",
            "the first surface stays bound"
        );
        let bound_asks: Vec<String> = store
            .messages("root-s6two", false)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: Some(child_id),
                    ..
                } if child_id == "child-a-s6two" => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            bound_asks,
            vec!["may I push?".to_string()],
            "only child A's question reaches the user"
        );

        // The refused second surface continues the root's wake, so its error
        // result must keep its tool use in the follow-up request.
        for call in &root_provider.captured_calls() {
            assert_tool_results_have_matching_tool_uses(call);
        }

        root_handle.stop();
        child_handle.stop();
    }

    #[tokio::test]
    async fn a_root_ask_to_the_user_is_answered_like_before() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6reg")).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"continue?","options":["yes","no"]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("continuing".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("root-s6reg".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the root to wait for the answer", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-s6reg").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let messages = store.messages("root-s6reg", false).await.unwrap();
        let asks: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: None,
                    ..
                } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            asks,
            ["continue?"],
            "a root's own question stays unbound and reaches the user"
        );

        // The user's answer starts the root's next turn exactly as before.
        store
            .append_message(
                "root-s6reg",
                Role::User,
                &Block::Text { text: "yes".into() },
            )
            .await
            .unwrap();
        handle.send(LoopEvent::Wake);

        wait_for("the answer's turn to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 2 }
            }
        })
        .await;
        let calls = provider.captured_calls();
        assert!(
            calls[1].messages.iter().any(|message| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == "yes")
            }),
            "the root's next turn reads the user's answer: {:#?}",
            calls[1].messages
        );
        wait_for("the root to wait for input again", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-s6reg").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        handle.stop();
    }

    #[tokio::test]
    async fn a_child_ask_delivered_mid_turn_is_surfaced_once_in_its_own_wake() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6mid")).await.unwrap();
        store
            .create_session(&child_session_of("child-s6mid", "root-s6mid"))
            .await
            .unwrap();
        store
            .append_message(
                "root-s6mid",
                Role::User,
                &Block::Text {
                    text: "start".into(),
                },
            )
            .await
            .unwrap();
        // The child's own thread ends in its pending ask, the way a child
        // loop that asked leaves it; the injected event below delivers that
        // ask to the root without running a child loop.
        store
            .append_message(
                "child-s6mid",
                Role::Assistant,
                &Block::Ask {
                    message: "may I push?".into(),
                    options: vec!["yes".into(), "no".into()],
                    child_id: None,
                    answer: None,
                },
            )
            .await
            .unwrap();
        store
            .set_state("child-s6mid", SessionState::WaitingForInput)
            .await
            .unwrap();

        // The delay keeps the first turn in flight while the child's Ask
        // event lands behind it; the queued wake then surfaces the event
        // exactly once and the root's reaction ends in the ask.
        let provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::TextDelta("overseeing".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("ask".into()),
                        args_delta: r#"{"message":"may I push?","child_id":"child-s6mid"}"#.into(),
                    },
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
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("root-s6mid".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the first turn to start", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;

        deliver_child_event(
            &store,
            "root-s6mid",
            "child-s6mid",
            ChildEventKind::Ask,
            "may I push?",
        )
        .await;
        handle.send(LoopEvent::Wake);

        // The session reaches WaitingForInput at the end of both the
        // in-flight wake and the queued wake, so the state alone cannot tell
        // the two apart. The durable pending ask is written only when the
        // queued wake's ask tool has run, which is when the provider has seen
        // both calls and the surfaced ask sits in the thread.
        wait_for("the queued wake to surface the ask durably", || {
            let store = store.clone();
            async move { store.get_pending_ask("root-s6mid").await.unwrap().is_some() }
        })
        .await;

        let calls = provider.captured_calls();
        assert_eq!(calls.len(), 2, "the running wake and the event's wake");
        assert!(
            !calls[0]
                .messages
                .iter()
                .any(|message| { matches!(&message.block, Block::ChildEvent { .. }) }),
            "the in-flight turn's window predates the mid-turn ask"
        );
        assert!(
            calls[1].messages.iter().any(|message| {
                matches!(&message.block, Block::ChildEvent {
                    child_id,
                    kind,
                    text,
                    ..
                }
                    if child_id == "child-s6mid"
                        && *kind == ChildEventKind::Ask
                        && text == "may I push?")
            }),
            "the queued wake surfaces the child's ask event once: {:#?}",
            calls[1].messages
        );

        // The reaction surfaced exactly one bound ask, and the event was not
        // delivered to any other turn.
        let parent_messages = store.messages("root-s6mid", false).await.unwrap();
        let asks: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: Some(child_id),
                    ..
                } if child_id == "child-s6mid" => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            asks,
            ["may I push?"],
            "the child's ask is surfaced exactly once"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn ask_with_a_child_binding_to_a_foreign_or_unknown_child_is_refused() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s6ref")).await.unwrap();
        store.create_session(&session("other-s6ref")).await.unwrap();
        store
            .create_session(&child_session_of("foreign-child", "other-s6ref"))
            .await
            .unwrap();

        // A root may only surface a question bound to one of its own
        // children: a foreign child and an unknown id are refused and the
        // wake continues, so the model can recover.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"foreign-child"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"ghost"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-3".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"proceed?"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("root-s6ref".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the root to wait for input", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-s6ref").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let results = store.tool_calls("root-s6ref").await.unwrap();
        let error_of = |id: &str| {
            results
                .iter()
                .find(|call| call.call_id == id)
                .and_then(|call| call.result.as_ref())
                .and_then(|result| result["error"].as_str())
                .map(str::to_string)
        };
        assert_eq!(
            error_of("call-1").as_deref(),
            Some("session foreign-child is not a child of this session")
        );
        assert_eq!(
            error_of("call-2").as_deref(),
            Some("no child session ghost")
        );
        assert_eq!(
            error_of("call-3").as_deref(),
            None,
            "the unbound ask succeeds"
        );
        let messages = store.messages("root-s6ref", false).await.unwrap();
        let bound: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: Some(_),
                    ..
                } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            bound.is_empty(),
            "a refused binding records no surfaced ask"
        );
        let reached_user: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    message,
                    child_id: None,
                    ..
                } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reached_user, ["proceed?"]);
        // The refused bindings continued the wake, so each error result kept
        // its tool use in the follow-up requests.
        for call in &provider.captured_calls() {
            assert_tool_results_have_matching_tool_uses(call);
        }
        handle.stop();

        // A child session calling ask with a binding must name one of its own
        // children: a foreign child is refused, and the child's own plain ask
        // still reaches its parent.
        store
            .create_session(&child_session_of("child-s6ref", "root-s6ref"))
            .await
            .unwrap();
        store
            .append_message(
                "child-s6ref",
                Role::User,
                &Block::Text {
                    text: "do the change".into(),
                },
            )
            .await
            .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-4".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","child_id":"foreign-child"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-5".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("child-s6ref".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the child to wait after its plain ask", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-s6ref").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let results = store.tool_calls("child-s6ref").await.unwrap();
        let refused = results
            .iter()
            .find(|call| call.call_id == "call-4")
            .and_then(|call| call.result.as_ref())
            .and_then(|result| result["error"].as_str())
            .map(str::to_string);
        assert_eq!(
            refused.as_deref(),
            Some("session foreign-child is not a child of this session")
        );
        let parent_messages = store.messages("root-s6ref", false).await.unwrap();
        let events: Vec<&str> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Ask,
                    text,
                    ..
                } if child_id == "child-s6ref" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            events,
            ["may I push?"],
            "only the child's plain ask reaches its parent"
        );

        // The refused child ask continued the wake into the plain ask, so its
        // error result kept its tool use in the plain ask's request.
        for call in &provider.captured_calls() {
            assert_tool_results_have_matching_tool_uses(call);
        }

        handle.stop();
    }

    #[tokio::test]
    async fn todowrite_is_refused_and_not_advertised_on_a_child_session() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-todo")).await.unwrap();
        store
            .create_session(&child_session_of("child-todo", "root-todo"))
            .await
            .unwrap();
        store
            .append_message(
                "child-todo",
                Role::User,
                &Block::Text {
                    text: "do the work".into(),
                },
            )
            .await
            .unwrap();

        // The child's model calls todowrite anyway; the loop refuses it: the
        // todo list belongs to the tree owner, and a child's refusal must not
        // take the child down — the wake continues and finishes normally.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("todowrite".into()),
                    args_delta: r#"{"items":[{"id":"1","content":"plan","status":"todo"}]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("continuing without a todo list".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("child-todo".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the child to finish its wake and stop", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-todo").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;

        let first = provider.captured_calls();
        let names: Vec<&str> = first[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            !names.contains(&"todowrite"),
            "a child never sees the todo tool in its schema: {names:?}"
        );
        let messages = store.messages("child-todo", false).await.unwrap();
        assert!(
            messages.iter().any(|(_, message)| message.role == Role::User
                && matches!(
                    &message.block,
                    Block::ToolResult { id, name, is_error, content } if id == "call-1"
                        && name == "todowrite"
                        && *is_error
                        && content
                            == &json!({ "error": "todowrite is only available to the root session" })
                )),
            "a child's todowrite call records a refusal"
        );

        // The root still sees and uses the tool.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("todowrite".into()),
                    args_delta: r#"{"items":[{"id":"1","content":"plan","status":"todo"}]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("planned".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let deps = Arc::new(test_deps(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
        ));
        let handle = spawn_loop("root-todo".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the root's todowrite turn to finish", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-todo").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let calls = provider.captured_calls();
        let names: Vec<&str> = calls[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            names.contains(&"todowrite"),
            "the root sees the todo tool: {names:?}"
        );
        let messages = store.messages("root-todo", false).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|(_, message)| message.role == Role::User
                    && matches!(
                        &message.block,
                        Block::ToolResult { id, name, is_error, .. } if id == "call-2"
                            && name == "todowrite"
                            && !is_error
                    )),
            "the root's todowrite call succeeds"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn the_persona_catalog_is_advertised_only_to_spawn_capable_sessions() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-cat")).await.unwrap();
        store
            .create_session(&child_session_of("child-cat", "root-cat"))
            .await
            .unwrap();
        store
            .create_session(&session_allowing("read-cat", "file/read, grep"))
            .await
            .unwrap();

        let personas = HashMap::from([
            (
                "coder".to_string(),
                PersonaConfig {
                    description: "Makes changes directly".into(),
                    ..persona("mock-model", Permission::ReadWrite)
                },
            ),
            (
                "reviewer".to_string(),
                persona("mock-model", Permission::ReadOnly),
            ),
        ]);
        let spawner = Arc::new(FakeSpawner::ok("never-spawned"));

        // A spawn-capable session — root or child — sees the catalog as name
        // and description; a session whose allow-list drops spawn sees
        // neither the tool nor the catalog.
        for (id, name) in [
            ("root-cat", "root with spawn"),
            ("child-cat", "child with spawn"),
            ("read-cat", "session without spawn"),
        ] {
            let provider = Arc::new(ScriptedProvider::new(vec![vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ]]));
            let deps = Arc::new(test_deps_with_spawner(
                &store,
                provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                personas.clone(),
                HashMap::new(),
                spawner.clone(),
            ));
            let handle = spawn_loop(id.to_string(), deps);
            handle.send(LoopEvent::Wake);

            wait_for(&format!("the {name} turn to run"), {
                let provider = provider.clone();
                move || {
                    let provider = provider.clone();
                    async move { provider.captured_calls().len() == 1 }
                }
            })
            .await;
            let system = &provider.captured_calls()[0].system;
            let spawn_capable = name.contains("with spawn");
            assert_eq!(
                system.contains("Personas you may spawn:"),
                spawn_capable,
                "the catalog heading is advertised to the {name} only: {system}"
            );
            assert_eq!(
                system.contains("coder: Makes changes directly"),
                spawn_capable,
                "a persona's name and description advertise only to the {name}: {system}"
            );
            if spawn_capable {
                assert!(
                    system.contains("\n- reviewer"),
                    "a persona without a description advertises by name alone: {system}"
                );
            }
            handle.stop();
        }

        // Without the spawner no session is spawn-capable, however wide its
        // allow-list is: the catalog stays out of the prompt.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let deps = Arc::new(test_deps_with_personas(
            &store,
            provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            personas,
            HashMap::new(),
        ));
        let handle = spawn_loop("root-cat".into(), deps);
        handle.send(LoopEvent::Wake);

        wait_for("the spawner-less root turn to run", {
            let provider = provider.clone();
            move || {
                let provider = provider.clone();
                async move { provider.captured_calls().len() == 1 }
            }
        })
        .await;
        let system = &provider.captured_calls()[0].system;
        assert!(
            !system.contains("Personas you may spawn:"),
            "no catalog without a spawner: {system}"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_grandchild_ask_surfaces_to_the_root_and_the_user_answer_reaches_the_leaf_verbatim() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s7deep")).await.unwrap();
        store
            .create_session(&child_session_of("mid-s7deep", "root-s7deep"))
            .await
            .unwrap();
        let mut leaf = child_session_of("leaf-s7deep", "mid-s7deep");
        leaf.owner_id = "root-s7deep".into();
        store.create_session(&leaf).await.unwrap();
        store
            .append_message(
                "leaf-s7deep",
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();

        // The leaf asks; the mid-level parent surfaces the question upward
        // instead of answering; the root surfaces it to the user bound to the
        // leaf. The user's answer is routed by the control plane straight to
        // the leaf, whose completion reports back up the tree level by level.
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            // The root reads "ask from child mid-s7deep" and raises it with
            // the child it knows; the loop resolves the binding to the leaf.
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I push?","options":["yes","no"],"child_id":"mid-s7deep"}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("noted the report".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        // The mid-level session surfaces the leaf's question to its own
        // parent, then later handles the leaf's completion report.
        let mid_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I push?","options":["yes","no"],"child_id":"leaf-s7deep"}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("recorded the result".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        // The leaf asks, waits for the user's answer, and completes on it.
        let leaf_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","options":["yes","no"]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("pushed to main".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));

        let mailbox = Arc::new(TestMailbox::new());
        let root_handle = spawn_loop(
            "root-s7deep".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let mid_handle = spawn_loop(
            "mid-s7deep".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                mid_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let leaf_handle = spawn_loop(
            "leaf-s7deep".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                leaf_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s7deep", root_handle.sender.clone());
        mailbox.register("mid-s7deep", mid_handle.sender.clone());
        mailbox.register("leaf-s7deep", leaf_handle.sender.clone());

        leaf_handle.send(LoopEvent::Wake);

        wait_for(
            "the surface chain to reach the root and bind the direct child",
            {
                let store = store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        let stored = store.get_session("root-s7deep").await.unwrap().unwrap();
                        stored.state == SessionState::WaitingForInput
                            && store
                                .get_pending_ask("root-s7deep")
                                .await
                                .unwrap()
                                .is_some()
                            && store.model_calls("root-s7deep").await.unwrap().len() == 1
                    }
                }
            },
        )
        .await;

        // The surfaced ask at the top names the direct child the root's model
        // named; the pending row carries the direct child and the ORIGIN leaf
        // the user's answer routes to.
        let pending = store.get_pending_ask("root-s7deep").await.unwrap().unwrap();
        assert_eq!(pending.child_id, "mid-s7deep");
        assert_eq!(pending.origin_leaf, "leaf-s7deep");
        assert_eq!(pending.question, "may I push?");
        let root_messages = store.messages("root-s7deep", false).await.unwrap();
        let bound_asks: Vec<(&str, i64, &str)> = root_messages
            .iter()
            .filter_map(|(id, message)| match &message.block {
                Block::Ask {
                    child_id: Some(child_id),
                    message,
                    ..
                } => Some((child_id.as_str(), *id, message.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            bound_asks,
            [("mid-s7deep", root_messages.last().unwrap().0, "may I push?")],
            "the surfaced ask block names the direct child the root can message"
        );
        assert_eq!(
            pending.ask_message_id, bound_asks[0].1,
            "the binding names the surfaced Ask block's row"
        );

        // Each level is waiting on the question it raised: the leaf on its
        // own ask, the mid-level session on its re-raise.
        for (id, model_calls) in [
            ("mid-s7deep", mid_provider.captured_calls().len()),
            ("leaf-s7deep", leaf_provider.captured_calls().len()),
        ] {
            let stored = store.get_session(id).await.unwrap().unwrap();
            assert_eq!(stored.state, SessionState::WaitingForInput, "{id}");
            assert_eq!(model_calls, 1, "{id} asked once and waits");
        }

        // The user answers; the control plane routes the text verbatim to the
        // origin leaf's thread and wakes the leaf's own loop.
        let answer = "yes, push to main";
        let routed = store.route_answer("root-s7deep", answer).await.unwrap();
        assert_eq!(
            routed,
            RouteAnswer::Routed {
                leaf_id: "leaf-s7deep".into()
            }
        );
        mailbox.send("leaf-s7deep", LoopEvent::ParentMessage);

        wait_for("the leaf to finish and the reports to climb the tree", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let root = store.get_session("root-s7deep").await.unwrap().unwrap();
                    let mid = store.get_session("mid-s7deep").await.unwrap().unwrap();
                    let leaf = store.get_session("leaf-s7deep").await.unwrap().unwrap();
                    root.state == SessionState::WaitingForInput
                        && store.model_calls("root-s7deep").await.unwrap().len() == 2
                        && mid.state == SessionState::Stopped
                        && leaf.state == SessionState::Stopped
                }
            }
        })
        .await;

        // The answer reached the leaf verbatim and no intermediate level's
        // model ever relayed it.
        let leaf_messages = store.messages("leaf-s7deep", false).await.unwrap();
        let leaf_texts: Vec<&str> = leaf_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            leaf_texts,
            ["implement the change", answer, "pushed to main"],
            "the answer is the leaf's next message, verbatim"
        );
        for (id, tool_calls) in [
            (
                "root-s7deep",
                store.tool_calls("root-s7deep").await.unwrap(),
            ),
            ("mid-s7deep", store.tool_calls("mid-s7deep").await.unwrap()),
        ] {
            assert!(
                !tool_calls.iter().any(|call| call.name == "message_child"),
                "no level relayed the answer with message_child: {id}: {tool_calls:?}"
            );
        }
        let mid_messages = store.messages("mid-s7deep", false).await.unwrap();
        assert!(
            !mid_messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::Text { text } if text == answer
            )),
            "the answer is not addressed to the mid-level session's thread"
        );

        // The binding is cleared and the answer recorded on the surfaced ask.
        assert!(
            store
                .get_pending_ask("root-s7deep")
                .await
                .unwrap()
                .is_none(),
            "the binding is cleared once the ask is answered"
        );
        let root_messages = store.messages("root-s7deep", false).await.unwrap();
        let answered = root_messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::Ask {
                    child_id: Some(child_id),
                    answer,
                    ..
                } if child_id == "mid-s7deep" => Some(answer),
                _ => None,
            })
            .expect("the surfaced ask block exists");
        assert_eq!(answered.as_deref(), Some(answer));

        root_handle.stop();
        mid_handle.stop();
        leaf_handle.stop();
    }

    #[tokio::test]
    async fn when_the_user_redirects_at_depth_the_root_cancels_through_its_direct_child_and_can_surface_again()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&session("root-s7redir"))
            .await
            .unwrap();
        store
            .create_session(&child_session_of("mid-s7redir", "root-s7redir"))
            .await
            .unwrap();
        let mut leaf = child_session_of("leaf-s7redir", "mid-s7redir");
        leaf.owner_id = "root-s7redir".into();
        store.create_session(&leaf).await.unwrap();
        store
            .append_message(
                "leaf-s7redir",
                Role::User,
                &Block::Text {
                    text: "push the change".into(),
                },
            )
            .await
            .unwrap();

        // The leaf asks; mid re-raises; the root surfaces the question to the
        // user. The user then redirects instead of answering, so the root
        // model cancels by messaging the child it can actually message — its
        // direct child mid, never the leaf it does not know — and mid relays
        // the cancellation to the leaf. The leaf re-asks, the raise climbs
        // again, and the root surfaces the new question: that second surface
        // succeeds only if messaging mid cleared the root's pending row, so
        // the root is never locked out of asking again.
        let root_redirect = "the user redirected instead of answering; stop the push attempt";
        let mid_relay = "the user redirected; your question is cancelled. stop the push attempt";
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to main?","options":["yes","no"],"child_id":"mid-s7redir"}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("message_child".into()),
                    args_delta: format!(r#"{{"id":"mid-s7redir","text":"{root_redirect}"}}"#),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("relayed the redirect to mid".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-3".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I review the README instead?","options":["yes","no"],"child_id":"mid-s7redir"}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("awaiting the answer".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let mid_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I push to main?","options":["yes","no"],"child_id":"leaf-s7redir"}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("message_child".into()),
                    args_delta: format!(r#"{{"id":"leaf-s7redir","text":"{mid_relay}"}}"#),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("relayed the cancellation to the leaf".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-3".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I review the README instead?","options":["yes","no"],"child_id":"leaf-s7redir"}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let leaf_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to main?","options":["yes","no"]}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I review the README instead?","options":["yes","no"]}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));

        let mailbox = Arc::new(TestMailbox::new());
        let root_handle = spawn_loop(
            "root-s7redir".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let mid_handle = spawn_loop(
            "mid-s7redir".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                mid_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let leaf_handle = spawn_loop(
            "leaf-s7redir".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                leaf_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s7redir", root_handle.sender.clone());
        mailbox.register("mid-s7redir", mid_handle.sender.clone());
        mailbox.register("leaf-s7redir", leaf_handle.sender.clone());

        leaf_handle.send(LoopEvent::Wake);

        wait_for("the raise chain to surface at the root", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("root-s7redir").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store
                            .get_pending_ask("root-s7redir")
                            .await
                            .unwrap()
                            .is_some()
                        && store.model_calls("root-s7redir").await.unwrap().len() == 1
                }
            }
        })
        .await;
        let pending = store
            .get_pending_ask("root-s7redir")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pending.child_id, "mid-s7redir",
            "the pending row names the root's direct child, the session the root can message to cancel"
        );
        assert_eq!(
            pending.origin_leaf, "leaf-s7redir",
            "the pending row carries the origin leaf, where the user's answer routes"
        );

        // The user redirects instead of answering: the root model takes the
        // surfaced ask over and cancels it by messaging its direct child.
        store
            .append_message(
                "root-s7redir",
                Role::User,
                &Block::Text {
                    text: "stop the push attempt and review the README instead".into(),
                },
            )
            .await
            .unwrap();
        root_handle.send(LoopEvent::Wake);

        wait_for(
            "the cancel to reach the leaf and the re-ask to climb to the root again",
            {
                let store = store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        let root = store.get_session("root-s7redir").await.unwrap().unwrap();
                        let mid = store.get_session("mid-s7redir").await.unwrap().unwrap();
                        let leaf = store.get_session("leaf-s7redir").await.unwrap().unwrap();
                        root.state == SessionState::WaitingForInput
                            && store.model_calls("root-s7redir").await.unwrap().len() == 4
                            && mid.state == SessionState::WaitingForInput
                            && store.model_calls("mid-s7redir").await.unwrap().len() == 4
                            && leaf.state == SessionState::WaitingForInput
                            && store.model_calls("leaf-s7redir").await.unwrap().len() == 2
                    }
                }
            },
        )
        .await;

        // The root's second surface succeeded: messaging its direct child
        // cleared the first pending row, so the root was not locked out of
        // asking again, and the row now names the re-ask.
        let root_calls = store.tool_calls("root-s7redir").await.unwrap();
        let second_surface = root_calls
            .iter()
            .find(|call| call.call_id == "call-3")
            .and_then(|call| call.result.as_ref())
            .expect("the root's second ask tool call has a result");
        assert!(
            second_surface.get("error").is_none(),
            "the root surfaces the re-ask after cancelling the first: {second_surface}"
        );
        let pending = store
            .get_pending_ask("root-s7redir")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.child_id, "mid-s7redir");
        assert_eq!(pending.origin_leaf, "leaf-s7redir");
        assert_eq!(pending.question, "may I review the README instead?");

        // The cancellation reached the leaf through its own parent: the leaf
        // read the relayed notice and re-asked instead of hanging.
        let leaf_messages = store.messages("leaf-s7redir", false).await.unwrap();
        assert!(
            leaf_messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::Text { text } if text == mid_relay
            )),
            "the cancelled leaf reads the notice in its own thread"
        );
        let leaf_asks = store
            .tool_calls("leaf-s7redir")
            .await
            .unwrap()
            .into_iter()
            .filter(|call| call.name == "ask")
            .count();
        assert_eq!(leaf_asks, 2, "the leaf re-asked after the cancellation");

        root_handle.stop();
        mid_handle.stop();
        leaf_handle.stop();
    }

    #[tokio::test]
    async fn a_sibling_ask_raised_before_the_first_is_resolved_is_refused_and_the_surface_binds_the_first_leaf()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s7conc")).await.unwrap();
        store
            .create_session(&child_session_of("mid-s7conc", "root-s7conc"))
            .await
            .unwrap();
        let mut leaf = child_session_of("leaf-s7conc", "mid-s7conc");
        leaf.owner_id = "root-s7conc".into();
        store.create_session(&leaf).await.unwrap();
        let mut leaf2 = child_session_of("leaf2-s7conc", "mid-s7conc");
        leaf2.owner_id = "root-s7conc".into();
        store.create_session(&leaf2).await.unwrap();
        for (id, assignment) in [
            ("leaf-s7conc", "implement the change"),
            ("leaf2-s7conc", "refactor the module"),
        ] {
            store
                .append_message(
                    id,
                    Role::User,
                    &Block::Text {
                        text: assignment.into(),
                    },
                )
                .await
                .unwrap();
        }

        // The root's first wake is deliberately long-running, so the two
        // raises that follow land while it is busy and are handled by one
        // later wake: without a per-level pending guard, mid's second raise
        // (leaf2's question) would be authored before the root reacts to the
        // first, and the root would bind the surfaced question to the wrong
        // leaf. The root's surface turn is scripted for that later wake.
        let root_provider = Arc::new(ScriptedProvider::with_delay(
            vec![
                vec![
                    StreamEvent::TextDelta("overseeing".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".into()),
                        name: Some("ask".into()),
                        args_delta: r#"{"message":"may I push?","options":["yes","no"],"child_id":"mid-s7conc"}"#
                            .into(),
                    },
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
                vec![
                    StreamEvent::TextDelta("noted".into()),
                    StreamEvent::Stop {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                ],
            ],
            Duration::from_millis(300),
        ));
        // The mid-level session raises leaf's question, then — woken by
        // leaf2's own ask — tries to raise leaf2's too while the first raise
        // is unresolved: that second raise is refused, and the wake ends with
        // leaf2 still live so the mid-level session keeps supervising. Its
        // final wake reacts to leaf's completion report.
        let mid_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I push?","options":["yes","no"],"child_id":"leaf-s7conc"}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-2".into()),
                    name: Some("ask".into()),
                    args_delta:
                        r#"{"message":"may I refactor?","options":["yes","no"],"child_id":"leaf2-s7conc"}"#
                            .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("leaf2's question must wait for leaf's".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("leaf finished".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let leaf_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push?","options":["yes","no"]}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("pushed to main".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let leaf2_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".into()),
                name: Some("ask".into()),
                args_delta: r#"{"message":"may I refactor?","options":["yes","no"]}"#.into(),
            },
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));

        let mailbox = Arc::new(TestMailbox::new());
        let root_handle = spawn_loop(
            "root-s7conc".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let mid_handle = spawn_loop(
            "mid-s7conc".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                mid_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let leaf_handle = spawn_loop(
            "leaf-s7conc".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                leaf_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let leaf2_handle = spawn_loop(
            "leaf2-s7conc".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                leaf2_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s7conc", root_handle.sender.clone());
        mailbox.register("mid-s7conc", mid_handle.sender.clone());
        mailbox.register("leaf-s7conc", leaf_handle.sender.clone());
        mailbox.register("leaf2-s7conc", leaf2_handle.sender.clone());

        // The root's long first wake starts first; leaf asks and mid raises
        // its question while the root is busy.
        root_handle.send(LoopEvent::Wake);
        wait_for("the root's first wake to start", {
            let root_provider = root_provider.clone();
            move || {
                let root_provider = root_provider.clone();
                async move { root_provider.captured_calls().len() == 1 }
            }
        })
        .await;
        leaf_handle.send(LoopEvent::Wake);
        wait_for("leaf's question to be raised through mid", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("mid-s7conc").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.model_calls("mid-s7conc").await.unwrap().len() == 1
                }
            }
        })
        .await;

        // leaf2 asks while leaf's question is still unresolved: the mid-level
        // session's second re-raise is refused, so the root's later wake sees
        // exactly one raised question and binds it to leaf.
        leaf2_handle.send(LoopEvent::Wake);
        wait_for("mid's second ask tool call to resolve", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move { store.tool_calls("mid-s7conc").await.unwrap().len() == 2 }
            }
        })
        .await;

        wait_for(
            "the root to surface the first question bound to its leaf",
            {
                let store = store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        store
                            .get_pending_ask("root-s7conc")
                            .await
                            .unwrap()
                            .is_some()
                            && store.model_calls("root-s7conc").await.unwrap().len() >= 2
                    }
                }
            },
        )
        .await;

        // The durable binding for the surfaced question names the direct
        // child the root raised and the true origin leaf — leaf, not leaf2
        // whose ask mid tried to raise second.
        let pending = store.get_pending_ask("root-s7conc").await.unwrap().unwrap();
        assert_eq!(
            pending.child_id, "mid-s7conc",
            "the row names the root's direct child, whose question it carries"
        );
        assert_eq!(
            pending.origin_leaf, "leaf-s7conc",
            "the row carries the first question's origin leaf, not mid's later one"
        );
        let root_messages = store.messages("root-s7conc", false).await.unwrap();
        let raised: Vec<(&str, &str)> = root_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Ask,
                    text,
                    origin,
                    ..
                } if child_id == "mid-s7conc" => {
                    Some((text.as_str(), origin.as_deref().unwrap_or_default()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            raised,
            [("may I push?", "leaf-s7conc")],
            "exactly one question is raised through mid, and its event carries the origin leaf"
        );
        let mid_pending = store
            .get_pending_ask("mid-s7conc")
            .await
            .unwrap()
            .expect("the mid-level session holds its own outstanding raise");
        assert_eq!(
            mid_pending.child_id, "leaf-s7conc",
            "the guard row names the child the mid-level session raised"
        );
        let bound_asks: Vec<&str> = root_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Ask {
                    child_id: Some(_),
                    message,
                    ..
                } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            bound_asks,
            ["may I push?"],
            "the root surfaces only the first question"
        );
        let mid_results = store.tool_calls("mid-s7conc").await.unwrap();
        let refused = mid_results
            .iter()
            .find(|call| call.call_id == "call-2")
            .and_then(|call| call.result.as_ref())
            .and_then(|result| result["error"].as_str())
            .map(str::to_string);
        assert_eq!(
            refused.as_deref(),
            Some(
                "another question is pending with your parent; send child leaf-s7conc a message to cancel it before asking again"
            ),
            "a session with an unresolved re-raise refuses a further ask"
        );

        // The user's answer routes to the true leaf, which finishes: leaf is
        // not left hanging by the refused sibling raise.
        let answer = "yes, push to main";
        let routed = store.route_answer("root-s7conc", answer).await.unwrap();
        assert_eq!(
            routed,
            RouteAnswer::Routed {
                leaf_id: "leaf-s7conc".into()
            }
        );
        mailbox.send("leaf-s7conc", LoopEvent::ParentMessage);
        wait_for("the leaf to finish on the routed answer", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("leaf-s7conc").await.unwrap().unwrap();
                    stored.state == SessionState::Stopped
                        && store.model_calls("leaf-s7conc").await.unwrap().len() == 2
                }
            }
        })
        .await;
        let leaf_messages = store.messages("leaf-s7conc", false).await.unwrap();
        let leaf_texts: Vec<&str> = leaf_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            leaf_texts,
            ["implement the change", answer, "pushed to main"],
            "the answer reached the origin leaf verbatim"
        );
        // The leaf's completion report reached the mid-level session, which
        // closes the first raise: the guard row is cleared and the session
        // could raise again.
        wait_for("the leaf's report to reach the mid-level session", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    store.get_pending_ask("mid-s7conc").await.unwrap().is_none()
                        && store.model_calls("mid-s7conc").await.unwrap().len() == 4
                }
            }
        })
        .await;

        root_handle.stop();
        mid_handle.stop();
        leaf_handle.stop();
        leaf2_handle.stop();
    }

    #[tokio::test]
    async fn an_intermediate_parent_denies_its_childs_ask_without_waking_the_root() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s7den")).await.unwrap();
        store
            .create_session(&child_session_of("mid-s7den", "root-s7den"))
            .await
            .unwrap();
        let mut leaf = child_session_of("leaf-s7den", "mid-s7den");
        leaf.owner_id = "root-s7den".into();
        store.create_session(&leaf).await.unwrap();
        store
            .append_message(
                "leaf-s7den",
                Role::User,
                &Block::Text {
                    text: "push the change".into(),
                },
            )
            .await
            .unwrap();

        // The mid-level parent denies the leaf's ask on the user's behalf:
        // the denial lands in the leaf's thread, the leaf adapts and
        // finishes, and the root is woken only by the final completion
        // reports, never by the question itself.
        let denial = "denied: never push to main directly; use a branch";
        let root_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("noted the completion".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        // The mid-level session's first wake denies the leaf and ends; the
        // leaf is still live, so the mid-level session waits instead of
        // reporting, and its second wake reacts to the leaf's completion
        // report before it reports to the root and stops.
        let mid_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: format!(r#"{{"id":"leaf-s7den","text":"{denial}"}}"#),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("denied the push".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("the leaf finished".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let leaf_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("ask".into()),
                    args_delta: r#"{"message":"may I push to main?","options":["yes","no"]}"#
                        .into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("understood — using a branch".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));

        let mailbox = Arc::new(TestMailbox::new());
        let root_handle = spawn_loop(
            "root-s7den".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let mid_handle = spawn_loop(
            "mid-s7den".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                mid_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let leaf_handle = spawn_loop(
            "leaf-s7den".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                leaf_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s7den", root_handle.sender.clone());
        mailbox.register("mid-s7den", mid_handle.sender.clone());
        mailbox.register("leaf-s7den", leaf_handle.sender.clone());

        leaf_handle.send(LoopEvent::Wake);

        wait_for("the denial to resolve the leaf and the reports to climb", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let root = store.get_session("root-s7den").await.unwrap().unwrap();
                    let mid = store.get_session("mid-s7den").await.unwrap().unwrap();
                    let leaf = store.get_session("leaf-s7den").await.unwrap().unwrap();
                    root.state == SessionState::WaitingForInput
                        && store.model_calls("root-s7den").await.unwrap().len() == 1
                        && store.model_calls("mid-s7den").await.unwrap().len() == 3
                        && mid.state == SessionState::Stopped
                        && leaf.state == SessionState::Stopped
                }
            }
        })
        .await;

        // The question never surfaced: the root carries no Ask block, no
        // pending binding, and no message from the user answered it.
        let root_messages = store.messages("root-s7den", false).await.unwrap();
        assert!(
            !root_messages
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Ask { .. })),
            "a denied ask never surfaces to the root"
        );
        assert!(
            store.get_pending_ask("root-s7den").await.unwrap().is_none(),
            "a denied ask records no pending binding"
        );
        assert_eq!(
            store.model_calls("root-s7den").await.unwrap().len(),
            1,
            "the root's only turn reacts to the completion reports"
        );

        // The denial landed verbatim in the leaf's thread and the leaf
        // resumed from its own question.
        let leaf_calls = leaf_provider.captured_calls();
        assert_eq!(leaf_calls.len(), 2);
        assert!(
            leaf_calls[1].messages.iter().any(|message| {
                message.role == Role::User
                    && matches!(&message.block, Block::Text { text } if text == denial)
            }),
            "the denied leaf reads the denial in its own thread: {:#?}",
            leaf_calls[1].messages
        );

        root_handle.stop();
        mid_handle.stop();
        leaf_handle.stop();
    }

    #[tokio::test]
    async fn a_child_that_spawned_waits_for_its_child_and_reports_only_after_it_completes() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-s7sup")).await.unwrap();
        store
            .create_session(&child_session_of("mid-s7sup", "root-s7sup"))
            .await
            .unwrap();
        let mut leaf = child_session_of("leaf-s7sup", "mid-s7sup");
        leaf.owner_id = "root-s7sup".into();
        leaf.state = SessionState::Running;
        store.create_session(&leaf).await.unwrap();
        store
            .append_message(
                "leaf-s7sup",
                Role::User,
                &Block::Text {
                    text: "write the tests".into(),
                },
            )
            .await
            .unwrap();

        // The mid-level session spawns the leaf and has nothing more to do in
        // its wake; because the leaf is live it must wait for the leaf's
        // completion instead of reporting to the root and stopping. Only once
        // the leaf's report is handled does the mid-level session report.
        let root_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("noted the report".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let mid_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("spawn".into()),
                    args_delta: r#"{"persona":"coder","instructions":"write the tests"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("spawned the leaf, supervising".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("the leaf finished".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let leaf_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("tests written".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let spawner = Arc::new(FakeSpawner::ok("leaf-s7sup"));

        let mailbox = Arc::new(TestMailbox::new());
        let root_handle = spawn_loop(
            "root-s7sup".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let mid_deps = test_deps_with_mailbox(
            &store,
            mid_provider.clone(),
            instant_tools(),
            Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            mailbox.clone(),
        );
        let mid_deps = LoopDeps {
            spawner: Some(spawner),
            ..mid_deps
        };
        let mid_handle = spawn_loop("mid-s7sup".into(), Arc::new(mid_deps));
        let leaf_handle = spawn_loop(
            "leaf-s7sup".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                leaf_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-s7sup", root_handle.sender.clone());
        mailbox.register("mid-s7sup", mid_handle.sender.clone());
        mailbox.register("leaf-s7sup", leaf_handle.sender.clone());

        mid_handle.send(LoopEvent::Wake);

        // The spawn wake ends with the leaf live: the mid-level session
        // waits for the leaf instead of reporting and stopping.
        wait_for("the mid-level session to wait after spawning", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let stored = store.get_session("mid-s7sup").await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.model_calls("mid-s7sup").await.unwrap().len() == 2
                }
            }
        })
        .await;
        let root_messages = store.messages("root-s7sup", false).await.unwrap();
        assert!(
            !root_messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    ..
                } if child_id == "mid-s7sup"
            )),
            "a supervising child authors no completion report while its leaf is live"
        );

        // The leaf completes; its report wakes the mid-level session, which
        // handles it and only then reports to the root and stops.
        leaf_handle.send(LoopEvent::Wake);

        wait_for("the mid-level session to report and stop after the leaf", {
            let store = store.clone();
            move || {
                let store = store.clone();
                async move {
                    let root = store.get_session("root-s7sup").await.unwrap().unwrap();
                    let mid = store.get_session("mid-s7sup").await.unwrap().unwrap();
                    let leaf = store.get_session("leaf-s7sup").await.unwrap().unwrap();
                    root.state == SessionState::WaitingForInput
                        && store.model_calls("root-s7sup").await.unwrap().len() == 1
                        && store.model_calls("mid-s7sup").await.unwrap().len() == 3
                        && mid.state == SessionState::Stopped
                        && leaf.state == SessionState::Stopped
                }
            }
        })
        .await;
        let root_messages = store.messages("root-s7sup", false).await.unwrap();
        let reports: Vec<&str> = root_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Report,
                    text,
                    ..
                } if child_id == "mid-s7sup" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            ["the leaf finished"],
            "the mid-level session reports once, after its leaf completed"
        );

        root_handle.stop();
        mid_handle.stop();
        leaf_handle.stop();
    }

    /// A child row in the state a stop leaves it in: interrupted by the user.
    fn user_interrupted_child(id: &str, parent: &str) -> Session {
        let mut child = child_session_of(id, parent);
        child.state = SessionState::Interrupted;
        child.interrupt_cause = Some(InterruptCause::User);
        child
    }

    /// A child row in the state boot recovery leaves it in: interrupted by a
    /// crash.
    fn crash_interrupted_child(id: &str, parent: &str) -> Session {
        let mut child = child_session_of(id, parent);
        child.state = SessionState::Interrupted;
        child.interrupt_cause = Some(InterruptCause::Crash);
        child
    }

    #[tokio::test]
    async fn a_user_interrupted_child_ignores_plain_wakes_and_resumes_on_a_parent_message() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-guard")).await.unwrap();
        store
            .create_session(&user_interrupted_child("child-guard", "root-guard"))
            .await
            .unwrap();
        store
            .append_message(
                "child-guard",
                Role::User,
                &Block::Text {
                    text: "make the change".into(),
                },
            )
            .await
            .unwrap();

        // A second script would run a whole turn if a wake were let through;
        // the assertions prove none ran.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::TextDelta("made the change after all".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("an auto-resumed turn ran".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let mailbox = Arc::new(TestMailbox::new());
        let handle = spawn_loop(
            "child-guard".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("child-guard", handle.sender.clone());

        // The user's stop holds: a plain wake — the kind a child's own
        // children or a stray event would send — starts no turn and the
        // child authors nothing to its parent.
        handle.send(LoopEvent::Wake);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let stored = store.get_session("child-guard").await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            SessionState::Interrupted,
            "a plain wake does not resume a user-interrupted child"
        );
        assert_eq!(
            provider.captured_calls().len(),
            0,
            "a plain wake runs no model call on a user-interrupted child"
        );
        assert!(
            store
                .messages("root-guard", false)
                .await
                .unwrap()
                .is_empty(),
            "a user-interrupted child authors no event to its parent"
        );

        // The parent's message_child is the resume path: the message lands in
        // the child's thread and the child completes its work.
        store
            .append_message(
                "child-guard",
                Role::User,
                &Block::Text {
                    text: "please continue".into(),
                },
            )
            .await
            .unwrap();
        handle.send(LoopEvent::ParentMessage);
        wait_for("the resumed child to stop after reporting", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-guard").await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;
        let messages = store.messages("child-guard", false).await.unwrap();
        let texts: Vec<&str> = messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [
                "make the change",
                "please continue",
                "made the change after all"
            ],
            "the interrupted child resumes from its archived thread"
        );
        let parent_messages = store.messages("root-guard", false).await.unwrap();
        assert_eq!(
            parent_messages
                .iter()
                .filter(|(_, message)| matches!(
                    &message.block,
                    Block::ChildEvent {
                        child_id,
                        kind: ChildEventKind::Report,
                        ..
                    } if child_id == "child-guard"
                ))
                .count(),
            1,
            "the resumed child reports once"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_crash_interrupted_session_still_handles_a_wake_from_its_childrens_failures() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let mut root = session("root-crashwake");
        root.state = SessionState::Interrupted;
        root.interrupt_cause = Some(InterruptCause::Crash);
        store.create_session(&root).await.unwrap();
        store
            .create_session(&crash_interrupted_child(
                "child-crashwake",
                "root-crashwake",
            ))
            .await
            .unwrap();
        // The child's failure report is in the root's thread, the way boot
        // recovery leaves it; the wake that follows must reach the crashed
        // root so it can re-decide the child.
        deliver_child_event(
            &store,
            "root-crashwake",
            "child-crashwake",
            ChildEventKind::Failure,
            CRASH_FAILURE_TEXT,
        )
        .await;

        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("resuming the child".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let handle = spawn_loop(
            "root-crashwake".into(),
            Arc::new(test_deps(
                &store,
                provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
            )),
        );
        handle.send(LoopEvent::Wake);

        wait_for("the crashed root's re-decision turn to run", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("root-crashwake").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
                    && store.model_calls("root-crashwake").await.unwrap().len() == 1
            }
        })
        .await;
        let stored = store.get_session("root-crashwake").await.unwrap().unwrap();
        assert_eq!(
            stored.interrupt_cause,
            Some(InterruptCause::Crash),
            "the recorded cause survives the re-decision wake"
        );

        handle.stop();
    }

    #[tokio::test]
    async fn a_child_whose_turn_fails_authors_a_failure_event_to_its_parent() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-fail")).await.unwrap();
        store
            .create_session(&child_session_of("child-fail", "root-fail"))
            .await
            .unwrap();
        store
            .append_message(
                "child-fail",
                Role::User,
                &Block::Text {
                    text: "make the change".into(),
                },
            )
            .await
            .unwrap();

        // The child's stream ends without a Stop, so its turn fails.
        let child_provider = Arc::new(ScriptedProvider::new(vec![vec![StreamEvent::TextDelta(
            "partial".into(),
        )]]));
        let root_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("abandoned the failed child".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let mailbox = Arc::new(TestMailbox::new());
        let child_handle = spawn_loop(
            "child-fail".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                child_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let root_handle = spawn_loop(
            "root-fail".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("child-fail", child_handle.sender.clone());
        mailbox.register("root-fail", root_handle.sender.clone());

        child_handle.send(LoopEvent::Wake);

        // The failed child is parked as a crash interruption, and its parent
        // woke to its authored failure and decided not to resume it.
        wait_for("the failure to reach the parent's thread", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("child-fail").await.unwrap().unwrap();
                stored.state == SessionState::Interrupted
                    && store.model_calls("root-fail").await.unwrap().len() == 1
            }
        })
        .await;
        let stored = store.get_session("child-fail").await.unwrap().unwrap();
        assert_eq!(
            stored.interrupt_cause,
            Some(InterruptCause::Crash),
            "a turn that fails on its own is a crash interruption"
        );
        let parent_messages = store.messages("root-fail", false).await.unwrap();
        let failures: Vec<(&str, &str)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent {
                    child_id,
                    kind: ChildEventKind::Failure,
                    text,
                    ..
                } => Some((child_id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            failures,
            [("child-fail", CRASH_FAILURE_TEXT)],
            "the failed child authors one failure event to its parent"
        );
        let child_messages = store.messages("child-fail", false).await.unwrap();
        assert_eq!(
            child_messages.len(),
            1,
            "the child's thread holds only its assignment: {child_messages:?}"
        );
        assert_eq!(child_messages[0].1.role, Role::User);
        assert!(
            matches!(&child_messages[0].1.block, Block::Text { text } if text == "make the change"),
            "the failed turn commits nothing to the child's transcript"
        );

        child_handle.stop();
        root_handle.stop();
    }

    #[tokio::test]
    async fn a_parent_that_receives_crash_failures_resumes_one_child_and_abandons_another() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let mut root = session("root-red");
        root.state = SessionState::WaitingForInput;
        store.create_session(&root).await.unwrap();
        store
            .create_session(&crash_interrupted_child("child-red-a", "root-red"))
            .await
            .unwrap();
        store
            .create_session(&crash_interrupted_child("child-red-b", "root-red"))
            .await
            .unwrap();

        // Both children report their crashes the way boot recovery reports
        // them; the parent wakes once per report and decides each.
        deliver_child_event(
            &store,
            "root-red",
            "child-red-a",
            ChildEventKind::Failure,
            CRASH_FAILURE_TEXT,
        )
        .await;
        deliver_child_event(
            &store,
            "root-red",
            "child-red-b",
            ChildEventKind::Failure,
            CRASH_FAILURE_TEXT,
        )
        .await;

        let mailbox = Arc::new(TestMailbox::new());
        let root_provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("message_child".into()),
                    args_delta: r#"{"id":"child-red-a","text":"resume the review"}"#.into(),
                },
                StreamEvent::Stop {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("child a resumed".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("child b abandoned".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
            vec![
                StreamEvent::TextDelta("child a's report noted".into()),
                StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        ]));
        let resumed_provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::TextDelta("the review is done".into()),
            StreamEvent::Stop {
                input_tokens: 1,
                output_tokens: 1,
            },
        ]]));
        let root_handle = spawn_loop(
            "root-red".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                root_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        let resumed_handle = spawn_loop(
            "child-red-a".into(),
            Arc::new(test_deps_with_mailbox(
                &store,
                resumed_provider.clone(),
                instant_tools(),
                Arc::new(CollectSink(Arc::new(Mutex::new(Vec::new())))),
                mailbox.clone(),
            )),
        );
        mailbox.register("root-red", root_handle.sender.clone());
        mailbox.register("child-red-a", resumed_handle.sender.clone());

        root_handle.send(LoopEvent::Wake);
        root_handle.send(LoopEvent::Wake);

        let calls = root_provider.clone();
        let store_for_wait = store.clone();
        wait_for(
            "the resumed child to finish and the parent to react to both reports",
            move || {
                let store = store_for_wait.clone();
                let calls = calls.clone();
                async move {
                    let root = store.get_session("root-red").await.unwrap().unwrap();
                    let resumed = store.get_session("child-red-a").await.unwrap().unwrap();
                    let abandoned = store.get_session("child-red-b").await.unwrap().unwrap();
                    root.state == SessionState::WaitingForInput
                        && calls.captured_calls().len() == 4
                        && resumed.state == SessionState::Stopped
                        && abandoned.state == SessionState::Interrupted
                }
            },
        )
        .await;

        // The resumed child ran to completion; the abandoned one stayed
        // parked with no model call of its own.
        let resumed = store.get_session("child-red-a").await.unwrap().unwrap();
        assert_eq!(resumed.state, SessionState::Stopped);
        assert_eq!(
            resumed_provider.captured_calls().len(),
            1,
            "the resumed child ran exactly the turn its parent's message started"
        );
        let abandoned = store.get_session("child-red-b").await.unwrap().unwrap();
        assert_eq!(abandoned.state, SessionState::Interrupted);
        assert_eq!(
            abandoned.interrupt_cause,
            Some(InterruptCause::Crash),
            "an abandoned crash child stays interrupted with its cause"
        );
        let abandoned_messages = store.messages("child-red-b", false).await.unwrap();
        assert!(
            abandoned_messages.is_empty(),
            "an abandoned child is never woken"
        );

        // The parent's thread holds both failures and the resumed child's
        // completion report: resume-versus-abandon was decided from the
        // authored events.
        let parent_messages = store.messages("root-red", false).await.unwrap();
        let child_events: Vec<(&str, ChildEventKind)> = parent_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent { child_id, kind, .. } => Some((child_id.as_str(), *kind)),
                _ => None,
            })
            .collect();
        assert_eq!(
            child_events,
            [
                ("child-red-a", ChildEventKind::Failure),
                ("child-red-b", ChildEventKind::Failure),
                ("child-red-a", ChildEventKind::Report),
            ],
            "two failures reported, one child resumed and reported back"
        );
        let resumed_messages = store.messages("child-red-a", false).await.unwrap();
        let texts: Vec<&str> = resumed_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            ["resume the review", "the review is done"],
            "the parent's message resumed the child from its archived thread"
        );

        root_handle.stop();
        resumed_handle.stop();
    }
}
