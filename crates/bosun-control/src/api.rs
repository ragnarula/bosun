use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware::Next;
use axum::middleware::from_fn;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::Event as SseEvent;
use axum::response::sse::KeepAlive;
use axum::response::sse::Sse;
use axum::routing::get;
use axum::routing::post;
use bosun_agent::agent_loop::LoopEvent;
use bosun_agent::agent_loop::LoopMailbox;
use bosun_agent::provider::Provider;
use bosun_common::config::PersonaConfig;
use bosun_common::error::ErrorExt;
use bosun_common::session::Block;
use bosun_common::session::Event;
use bosun_common::session::InterruptCause;
use bosun_common::session::Permission;
use bosun_common::session::Role;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::tunnel::Tunnel;
use bosun_common::types::CloneRequest;
use bosun_common::types::CommandResult;
use bosun_common::types::DevRequest;
use bosun_common::types::DirListing;
use bosun_common::types::NodeCommand;
use bosun_common::types::NodeUpdateRequest;
use bosun_common::types::PollRequest;
use bosun_common::types::PollResponse;
use bosun_common::types::StopRequest;
use bosun_store::store::ModelCall;
use bosun_store::store::RouteAnswer;
use bosun_store::store::Store;
use bosun_store::store::StoreError;
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::stream;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use crate::commands::CommandQueue;
use crate::loops::AgentRegistry;
use crate::registry::NodeHealth;
use crate::registry::NodeRegistry;
use crate::tools::set_executor_permission;
use crate::tunnel::TunnelRegistry;

const SPAWN_TIMEOUT_SECS: u64 = 300;
/// How often the events stream polls the store for new durable events, so a
/// client that joined mid-turn still sees the terminal state quickly.
const EVENTS_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("node {node} is not up")]
    NodeNotUp { node: String },

    #[error(
        "node {node} predates auto-update and cannot parse an Update command; upgrade it out of band"
    )]
    NodePredatesAutoUpdate { node: String },

    #[error("node {node} rejected the request: {detail}")]
    NodeRejected { node: String, detail: String },

    #[error("node {node} is unreachable")]
    NodeUnreachable { node: String },

    #[error("session {id} was not found")]
    SessionNotFound { id: String },

    #[error(
        "session {id} is a child session and is watch-only; only its owner accepts user actions"
    )]
    ChildIsWatchOnly { id: String },

    #[error("no persona configured")]
    NoPersona,

    #[error("persona {persona} is not configured")]
    PersonaNotFound { persona: String },

    #[error("persona {persona} references model {model} which is not configured")]
    PersonaModelNotFound { persona: String, model: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::SessionNotFound { id } => ApiError::SessionNotFound { id },
            StoreError::Internal(error) => ApiError::Internal(error),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, text) = match &self {
            ApiError::NodeNotUp { .. }
            | ApiError::NodePredatesAutoUpdate { .. }
            | ApiError::NoPersona
            | ApiError::PersonaNotFound { .. }
            | ApiError::PersonaModelNotFound { .. } => {
                (StatusCode::BAD_REQUEST, Some(self.to_string()))
            }
            ApiError::NodeRejected { .. } | ApiError::NodeUnreachable { .. } => {
                (StatusCode::BAD_GATEWAY, Some(self.to_string()))
            }
            ApiError::ChildIsWatchOnly { .. } => (StatusCode::BAD_REQUEST, Some(self.to_string())),
            ApiError::SessionNotFound { .. } => (StatusCode::NOT_FOUND, Some(self.to_string())),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
        };

        tracing::error!("error: {}", self.display_chain());

        match text {
            Some(text) => (status, text).into_response(),
            None => status.into_response(),
        }
    }
}

pub struct AppState {
    pub registry: Arc<NodeRegistry>,
    pub commands: Arc<CommandQueue>,
    pub tunnels: Arc<TunnelRegistry>,
    pub store: Store,
    pub loops: Arc<AgentRegistry>,
    pub providers: HashMap<String, Arc<dyn Provider>>,
    /// Configured personas, keyed by persona name.
    pub personas: HashMap<String, PersonaConfig>,
    /// The persona sessions use when a request names none.
    pub default_persona: Option<String>,
    /// Skills injected into every session from the control plane's data dir.
    pub skills_dir: Option<PathBuf>,
}

impl AppState {
    /// Resolves the persona a session request names, falling back to the
    /// configured default, and the provider of its model. Resolution is live
    /// by name, so an unknown persona is a clear error at session start.
    /// Returns the persona's name, its config, and the model's provider.
    fn resolve_persona(
        &self,
        requested: &Option<String>,
    ) -> Result<(String, &PersonaConfig, Arc<dyn Provider>), ApiError> {
        let name = match requested {
            Some(name) => name.clone(),
            None => self.default_persona.clone().ok_or(ApiError::NoPersona)?,
        };
        let persona = self
            .personas
            .get(&name)
            .ok_or_else(|| ApiError::PersonaNotFound {
                persona: name.clone(),
            })?;
        let provider = self.providers.get(&persona.model).cloned().ok_or_else(|| {
            ApiError::PersonaModelNotFound {
                persona: name.clone(),
                model: persona.model.clone(),
            }
        })?;
        Ok((name, persona, provider))
    }
}

/// Control-plane boot recovery: sessions that were mid-flight when the
/// process died become `Interrupted`, and every surviving session's loop is
/// re-spawned so it rehydrates from the store and waits for the user.
pub async fn recover(state: &AppState) {
    let sessions = match state.store.list_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::error!(
                error = %error.display_chain(),
                "failed to list sessions on recovery"
            );
            return;
        }
    };
    for session in &sessions {
        if matches!(
            session.state,
            SessionState::Running | SessionState::Creating
        ) {
            if let Err(error) = state
                .store
                .mark_interrupted(&session.id, InterruptCause::Crash)
                .await
            {
                warn!(
                    session_id = %session.id,
                    error = %error.display_chain(),
                    "failed to mark the session interrupted"
                );
            }
            info!(
                session_id = %session.id,
                from = ?session.state,
                "session interrupted by control-plane restart"
            );
        }
        if state.providers.contains_key(&session.model) {
            state.loops.start(
                &session.id,
                state.store.clone(),
                state.providers[&session.model].clone(),
                state.tunnels.clone(),
                &session.model,
            );
        } else {
            warn!(
                session_id = %session.id,
                model = %session.model,
                "skipping loop: model not configured"
            );
        }
    }
    info!(count = sessions.len(), "recovered sessions");
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(crate::ui::pane))
        .route("/ui", get(crate::ui::pane))
        .route("/poll", post(poll))
        .route("/nodes", get(nodes))
        .route("/sessions", get(sessions).post(create_session))
        .route("/sessions/{id}", get(session_detail))
        .route("/sessions/{id}/messages", post(add_message))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/permission", post(set_permission))
        .route("/sessions/{id}/persona", post(switch_persona))
        .route("/sessions/{id}/model-calls", get(session_model_calls))
        .route("/sessions/{id}/events", get(events))
        .route("/clone", post(clone))
        .route("/dev", post(dev))
        .route("/nodes/{name}/dirs", get(dirs))
        .route("/nodes/{name}/update", post(node_update))
        .route("/stop", post(stop))
        .route("/tunnel/session/{id}", get(tunnel))
        .fallback(not_found)
        .layer(from_fn(add_version_header))
        .with_state(state)
}

/// Puts the control plane's version on every response, so a client learns it
/// is outdated from any command, with no extra roundtrip.
async fn add_version_header(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        bosun_common::types::X_BOSUN_VERSION,
        HeaderValue::from_static(bosun_common::version::VERSION),
    );
    response
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// The node's one outbound control request: it reports its heartbeat payload,
/// delivers the previous command's result, and takes the next command.
#[instrument(skip(state))]
async fn poll(
    State(state): State<Arc<AppState>>,
    Json(poll): Json<PollRequest>,
) -> Json<PollResponse> {
    state.registry.upsert(
        &poll.node_name,
        &poll.version,
        poll.update_status,
        SystemTime::now(),
    );
    if let Some(result) = poll.result {
        state.commands.report(&poll.node_name, result);
    }
    let command = state.commands.next(&poll.node_name).await;
    Json(PollResponse {
        command,
        version: bosun_common::version::VERSION.to_string(),
    })
}

#[instrument(skip(state))]
async fn nodes(State(state): State<Arc<AppState>>) -> Json<Vec<NodeHealth>> {
    Json(state.registry.list(SystemTime::now()))
}

#[instrument(skip(state))]
async fn sessions(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Session>>, ApiError> {
    let sessions = state.store.list_sessions().await?;
    Ok(Json(sessions))
}

#[instrument(skip(state))]
async fn session_detail(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Session>, ApiError> {
    let session = state
        .store
        .get_session(&id)
        .await?
        .ok_or_else(|| ApiError::SessionNotFound { id: id.clone() })?;
    Ok(Json(session))
}

/// One session's recorded model calls, oldest first, with their aggregates.
/// Token and cost fields are summed with None counted as zero.
#[derive(Serialize)]
pub struct ModelCallSummary {
    pub calls: Vec<ModelCall>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost: f64,
    pub completion_calls: u64,
    pub compaction_calls: u64,
}

#[instrument(skip(state))]
async fn session_model_calls(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ModelCallSummary>, ApiError> {
    if state.store.get_session(&id).await?.is_none() {
        return Err(ApiError::SessionNotFound { id });
    }
    let calls = state.store.model_calls(&id).await?;
    Ok(Json(ModelCallSummary {
        total_input_tokens: calls
            .iter()
            .map(|call| call.input_tokens.unwrap_or(0))
            .sum(),
        total_output_tokens: calls
            .iter()
            .map(|call| call.output_tokens.unwrap_or(0))
            .sum(),
        total_cost: calls.iter().map(|call| call.cost.unwrap_or(0.0)).sum(),
        completion_calls: calls
            .iter()
            .filter(|call| call.kind == "completion")
            .count() as u64,
        compaction_calls: calls
            .iter()
            .filter(|call| call.kind == "compaction")
            .count() as u64,
        calls,
    }))
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    node: String,
    dir: String,
    repo_url: Option<String>,
    git_ref: Option<String>,
    persona: Option<String>,
    prompt: Option<String>,
}

#[instrument(skip(state))]
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<Session>, ApiError> {
    let (persona, config, provider) = state.resolve_persona(&req.persona)?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let session = Session {
        id: session_id.clone(),
        node: req.node,
        repo_url: req.repo_url,
        git_ref: req.git_ref,
        dir: req.dir,
        model: config.model.clone(),
        persona: Some(persona),
        parent_id: None,
        owner_id: session_id,
        permission: config.permission,
        allowed_tools: config.allowed_tools.clone(),
        state: SessionState::Creating,
        interrupt_cause: None,
        created_at_secs: now_secs(),
        prompt: req.prompt.clone(),
    };
    start_session(&state, &session, provider).await?;

    let session = state
        .store
        .get_session(&session.id)
        .await?
        .expect("the session was just created");
    Ok(Json(session))
}

#[instrument(skip(state))]
async fn clone(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CloneRequest>,
) -> Result<Json<Session>, ApiError> {
    if state.registry.node(&req.node, SystemTime::now()).is_none() {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    }
    // Resolve the persona before the node does any work, so an unconfigured
    // persona rejects without leaving a cloned dir, executor, or tunnel
    // behind.
    let (persona, config, provider) = state.resolve_persona(&req.persona)?;
    let session_id = uuid::Uuid::new_v4().to_string();

    let command = NodeCommand::Clone {
        id: state.commands.next_id(),
        session_id: session_id.clone(),
        repo_url: req.repo_url.clone(),
        git_ref: req.git_ref.clone(),
        permission: config.permission,
    };
    let node_session = enqueue_and_await(&state, &req.node, command)
        .await
        .and_then(|result| match result {
            CommandResult::Session { session, .. } => Ok(session),
            CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                node: req.node.clone(),
                detail: message,
            }),
            _ => Err(ApiError::Internal(anyhow::anyhow!(
                "node answered clone with a non-session result"
            ))),
        })?;

    let dir = node_session
        .dir
        .map(|dir| dir.display().to_string())
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!(
                "node did not report a directory for the cloned session"
            ))
        })?;
    let session = Session {
        id: session_id.clone(),
        node: req.node.clone(),
        repo_url: node_session.repo_url.clone(),
        git_ref: node_session.git_ref.clone(),
        dir,
        model: config.model.clone(),
        persona: Some(persona),
        parent_id: None,
        owner_id: session_id,
        permission: config.permission,
        allowed_tools: config.allowed_tools.clone(),
        state: SessionState::Creating,
        interrupt_cause: None,
        created_at_secs: now_secs(),
        prompt: req.prompt.clone(),
    };
    start_session(&state, &session, provider).await?;

    let session = state
        .store
        .get_session(&session.id)
        .await?
        .expect("the session was just created");
    info!(session_id = %session.id, node = %req.node, "session cloned");
    Ok(Json(session))
}

#[instrument(skip(state))]
async fn dev(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DevRequest>,
) -> Result<Json<Session>, ApiError> {
    if state.registry.node(&req.node, SystemTime::now()).is_none() {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    }
    // Resolve the persona before the node does any work, so an unconfigured
    // persona rejects without leaving an executor or tunnel behind.
    let (persona, config, provider) = state.resolve_persona(&req.persona)?;
    let session_id = uuid::Uuid::new_v4().to_string();

    let command = NodeCommand::Dev {
        id: state.commands.next_id(),
        session_id: session_id.clone(),
        dir: req.dir.clone(),
        permission: config.permission,
    };
    let node_session = enqueue_and_await(&state, &req.node, command)
        .await
        .and_then(|result| match result {
            CommandResult::Session { session, .. } => Ok(session),
            CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                node: req.node.clone(),
                detail: message,
            }),
            _ => Err(ApiError::Internal(anyhow::anyhow!(
                "node answered dev with a non-session result"
            ))),
        })?;

    let dir = node_session
        .dir
        .map(|dir| dir.display().to_string())
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!(
                "node did not report a directory for the dev session"
            ))
        })?;
    let session = Session {
        id: session_id.clone(),
        node: req.node.clone(),
        repo_url: node_session.repo_url.clone(),
        git_ref: node_session.git_ref.clone(),
        dir,
        model: config.model.clone(),
        persona: Some(persona),
        parent_id: None,
        owner_id: session_id,
        permission: config.permission,
        allowed_tools: config.allowed_tools.clone(),
        state: SessionState::Creating,
        interrupt_cause: None,
        created_at_secs: now_secs(),
        prompt: req.prompt.clone(),
    };
    start_session(&state, &session, provider).await?;

    let session = state
        .store
        .get_session(&session.id)
        .await?
        .expect("the session was just created");
    info!(session_id = %session.id, node = %req.node, dir = %req.dir.display(), "dev session started");
    Ok(Json(session))
}

#[derive(Debug, Deserialize)]
struct DirsQuery {
    path: Option<PathBuf>,
}

#[instrument(skip(state))]
async fn dirs(
    State(state): State<Arc<AppState>>,
    AxumPath(node): AxumPath<String>,
    Query(query): Query<DirsQuery>,
) -> Result<Json<DirListing>, ApiError> {
    if state.registry.node(&node, SystemTime::now()).is_none() {
        return Err(ApiError::NodeNotUp { node });
    }

    let command = NodeCommand::Dirs {
        id: state.commands.next_id(),
        path: query.path,
    };
    let listing =
        enqueue_and_await(&state, &node, command)
            .await
            .and_then(|result| match result {
                CommandResult::Dirs { listing, .. } => Ok(listing),
                CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                    node,
                    detail: message,
                }),
                _ => Err(ApiError::Internal(anyhow::anyhow!(
                    "node answered dirs with a non-listing result"
                ))),
            })?;
    Ok(Json(listing))
}

/// Queues an `Update` command for a node, carrying the control plane's
/// version. The command is fire-and-forget: a successful update restarts the
/// node, so its result never arrives; the node reports the outcome through
/// its poll's update status instead.
///
/// The version handshake and the `Update` command ship in the same release,
/// so a node that reports a parsable version understands the command. A node
/// with no version or an unparsable one predates the handshake and cannot
/// parse an `Update` command at all: enqueuing one would wedge it, because
/// every poll returns the same command and every parse fails. Those nodes
/// must be upgraded out of band.
#[instrument(skip(state))]
async fn node_update(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<NodeUpdateRequest>,
) -> Result<StatusCode, ApiError> {
    let Some(node) = state.registry.node(&name, SystemTime::now()) else {
        return Err(ApiError::NodeNotUp { node: name.clone() });
    };
    if !parses_as_semver(&node.version) {
        return Err(ApiError::NodePredatesAutoUpdate { node: name.clone() });
    }
    state.commands.enqueue(
        &name,
        NodeCommand::Update {
            id: state.commands.next_id(),
            version: bosun_common::version::VERSION.to_string(),
            force: req.force,
        },
        None,
    );
    info!(node = %name, force = req.force, "update command queued");
    Ok(StatusCode::ACCEPTED)
}

/// Whether a version string parses as semver. Old nodes report an empty
/// version, and anything unparsable predates the version handshake.
fn parses_as_semver(version: &str) -> bool {
    bosun_common::version::compare(version, version).is_some()
}

/// Refuses a user action aimed at a child session: children are watch-only,
/// and messages, interrupts, permission and persona changes, and stops reach
/// only the tree owner.
fn ensure_root(session: &Session) -> Result<(), ApiError> {
    if session.parent_id.is_some() {
        return Err(ApiError::ChildIsWatchOnly {
            id: session.id.clone(),
        });
    }
    Ok(())
}

#[instrument(skip(state))]
async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<StatusCode, ApiError> {
    let Some(session) = state.store.get_session(&req.session_id).await? else {
        return Ok(StatusCode::NO_CONTENT);
    };
    ensure_root(&session)?;

    if state
        .registry
        .node(&session.node, SystemTime::now())
        .is_some()
    {
        let command = NodeCommand::Stop {
            id: state.commands.next_id(),
            session_id: req.session_id.clone(),
        };
        enqueue_and_await(&state, &session.node, command)
            .await
            .and_then(|result| match result {
                CommandResult::Stop { .. } => Ok(()),
                CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                    node: session.node.clone(),
                    detail: message,
                }),
                _ => Err(ApiError::Internal(anyhow::anyhow!(
                    "node answered stop with a non-stop result"
                ))),
            })?;
    } else {
        // The node is down: still enqueue the stop so it runs on the node's
        // next poll. Without it a restarted node would keep this session's
        // executor and tunnel alive while the control plane forgets the
        // session, and nothing would ever stop them.
        let (reply, _reply_rx) = oneshot::channel();
        state.commands.enqueue(
            &session.node,
            NodeCommand::Stop {
                id: state.commands.next_id(),
                session_id: req.session_id.clone(),
            },
            Some(reply),
        );
    }

    state.loops.stop(&req.session_id);
    state.tunnels.unregister(&req.session_id);
    state.store.remove_session(&req.session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct AddMessageRequest {
    content: String,
    /// A new instruction rather than an answer: while a surfaced child ask
    /// is pending, an answer routes mechanically to the origin leaf, and a
    /// redirect wakes the root model to decide the pending ask's fate. With
    /// no pending ask the flag changes nothing.
    #[serde(default)]
    redirect: bool,
}

/// Accepts a user message for a session. Only the owner of a tree accepts
/// input: attaching to a child session is watch-only, so a message aimed at
/// a child is refused.
#[instrument(skip(state))]
async fn add_message(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<AddMessageRequest>,
) -> Result<StatusCode, ApiError> {
    let Some(session) = state.store.get_session(&id).await? else {
        return Err(ApiError::SessionNotFound { id });
    };
    ensure_root(&session)?;
    // An answer to a surfaced child ask is routed by the control plane, not
    // by the root model: the text goes verbatim to the origin leaf and that
    // leaf's loop is woken, exactly like a `message_child` message, while the
    // root is left out of the turn entirely.
    if !req.redirect {
        match state.store.route_answer(&id, &req.content).await? {
            RouteAnswer::Routed { leaf_id } => {
                state.loops.send(&leaf_id, LoopEvent::ParentMessage);
                info!(
                    session_id = %id,
                    leaf_id = %leaf_id,
                    "routed the user's answer to the origin leaf"
                );
                return Ok(StatusCode::NO_CONTENT);
            }
            RouteAnswer::LeafGone { leaf_id } => {
                warn!(
                    session_id = %id,
                    leaf_id = %leaf_id,
                    "the pending ask's origin leaf is gone; treating the answer as a root message"
                );
            }
            // A message with no pending ask is an ordinary root message.
            RouteAnswer::NoBinding => {}
        }
    }
    state
        .store
        .append_message(&id, Role::User, &Block::Text { text: req.content })
        .await?;
    state.loops.wake(&id);
    Ok(StatusCode::NO_CONTENT)
}

#[instrument(skip(state))]
async fn interrupt(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let Some(session) = state.store.get_session(&id).await? else {
        return Err(ApiError::SessionNotFound { id });
    };
    ensure_root(&session)?;
    state.loops.interrupt(&id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct PermissionBody {
    permission: Permission,
}

#[instrument(skip(state))]
async fn set_permission(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<PermissionBody>,
) -> Result<StatusCode, ApiError> {
    let Some(session) = state.store.get_session(&id).await? else {
        return Err(ApiError::SessionNotFound { id: id.clone() });
    };
    ensure_root(&session)?;
    state.store.set_permission(&id, req.permission).await?;
    // The forward is best-effort: the store is authoritative and the loop
    // gates the tool schema per turn, so a node restart reverting the
    // executor to its persisted permission is not a safety hole.
    if let Err(error) = set_executor_permission(&state.tunnels, &id, req.permission).await {
        warn!(
            msg = "failed to forward the permission change to the executor",
            session_id = %id,
            error = %error.display_chain()
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct SwitchPersonaRequest {
    persona: String,
}

/// Switches a session to another persona: the stored session's persona,
/// model, permission and allowed-tool spec are replaced in one store
/// transaction and the switch is recorded on the event stream. The loop
/// re-resolves the session's model at every turn, so the switch applies to
/// the next model call; a turn in flight is not aborted. When the persona's
/// permission differs from the stored one, the executor's permission is
/// toggled best-effort through `/permission` exactly like `set_permission`.
#[instrument(skip(state))]
async fn switch_persona(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<SwitchPersonaRequest>,
) -> Result<StatusCode, ApiError> {
    let Some(session) = state.store.get_session(&id).await? else {
        return Err(ApiError::SessionNotFound { id: id.clone() });
    };
    ensure_root(&session)?;
    let (name, config, _provider) = state.resolve_persona(&Some(req.persona))?;
    let permission_changed = session.permission != config.permission;
    state
        .store
        .switch_persona(
            &id,
            &name,
            &config.model,
            config.permission,
            &config.allowed_tools,
        )
        .await?;
    // The toggle is best-effort like `set_permission`: the store is
    // authoritative and the loop gates the tool schema per turn.
    if permission_changed
        && let Err(error) = set_executor_permission(&state.tunnels, &id, config.permission).await
    {
        warn!(
            msg = "failed to forward the permission change to the executor",
            session_id = %id,
            error = %error.display_chain()
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after: Option<i64>,
}

/// SSE stream for a session: durable store events replayed from `after`, then
/// live text deltas from the loop's broadcast channel. While the stream is
/// open the store is polled for new durable events, so a client that joined
/// mid-turn still sees the terminal state.
#[instrument(skip(state))]
async fn events(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    if state.store.get_session(&id).await?.is_none() {
        return Err(ApiError::SessionNotFound { id });
    }
    // The explicit `after=` cursor wins; otherwise resume from the
    // `Last-Event-ID` header, which EventSource sends automatically on
    // reconnect with the seq of the last durable frame it received.
    let after = query
        .after
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0);

    let replayed = state.store.events_after(&id, after).await?;
    let last_seq = replayed.last().map(|(seq, _)| *seq).unwrap_or(after);
    let replay = stream::iter(replayed.into_iter().map(durable_frame)).boxed();

    // Polls the store for durable events past the last emitted seq. Events
    // at or below the cursor are skipped, so a replay that overlaps a poll
    // never duplicates a frame.
    let store = state.store.clone();
    let poll = stream::unfold(
        (store, id.clone(), last_seq),
        |(store, id, mut last_seq)| async move {
            tokio::time::sleep(EVENTS_POLL_INTERVAL).await;
            let events = store.events_after(&id, last_seq).await.unwrap_or_default();
            let mut frames = Vec::new();
            for (seq, event) in events {
                if seq <= last_seq {
                    continue;
                }
                last_seq = seq;
                frames.push(durable_frame((seq, event)));
            }
            Some((stream::iter(frames), (store, id, last_seq)))
        },
    )
    .flatten()
    .boxed();

    let live = match state.loops.subscribe(&id) {
        Some(rx) => stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(text) => {
                        let payload = json!({ "delta": text });
                        let event = SseEvent::default()
                            .json_data(&payload)
                            .expect("serializing a json value cannot fail");
                        return Some((Ok::<_, Infallible>(event), rx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed(),
        None => stream::empty().boxed(),
    };

    Ok(Sse::new(stream::select(replay.chain(poll), live))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// One durable frame: the seq in the SSE id and in the payload, so a client
/// reconnecting with `after=` or `Last-Event-ID` resumes exactly where it
/// left off.
fn durable_frame((seq, event): (i64, Event)) -> Result<SseEvent, Infallible> {
    let payload = json!({ "seq": seq, "event": event });
    Ok(SseEvent::default()
        .id(seq.to_string())
        .json_data(&payload)
        .expect("serializing a json value cannot fail"))
}

/// Enqueues a command for the node and waits for its result, delivered in the
/// node's next poll.
async fn enqueue_and_await(
    state: &Arc<AppState>,
    node: &str,
    command: NodeCommand,
) -> Result<CommandResult, ApiError> {
    let (reply, reply_rx) = oneshot::channel();
    state.commands.enqueue(node, command, Some(reply));
    match tokio::time::timeout(Duration::from_secs(SPAWN_TIMEOUT_SECS), reply_rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(ApiError::Internal(anyhow::anyhow!(
            "node {node} dropped the command reply"
        ))),
        Err(_) => Err(ApiError::NodeUnreachable {
            node: node.to_string(),
        }),
    }
}

/// Accepts a node's outbound tunnel for a session. The node keeps the
/// connection; tool calls open logical connections on it per request.
async fn tunnel(
    State(state): State<Arc<AppState>>,
    AxumPath(session_id): AxumPath<String>,
    mut req: Request<Body>,
) -> Response {
    // The store cannot gate this route: the node opens the tunnel right after
    // spawning the executor, which races the control plane creating the store
    // session for a clone. A tunnel for a session the store does not know is
    // inert — no loop dispatches tools on it — so it is harmless.
    if !wants_tunnel_upgrade(&req) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let tunnels = state.tunnels.clone();
    tokio::spawn(async move {
        let stream = match upgrade.await {
            Ok(stream) => stream,
            Err(error) => {
                warn!(session_id = %session_id, error = %error, "session tunnel upgrade failed");
                return;
            }
        };
        let (tunnel, _opens) = Tunnel::new(TokioIo::new(stream));
        tunnels.register(&session_id, tunnel.clone());
        tunnel.closed().await;
        tunnels.unregister(&session_id);
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "bosun-tunnel")
        .body(Body::empty())
        .unwrap_or_else(|error| {
            warn!(error = %error, "failed to build the 101 response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

fn wants_tunnel_upgrade(req: &Request<Body>) -> bool {
    let connection = req
        .headers()
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        && upgrade.eq_ignore_ascii_case("bosun-tunnel")
}

fn now_secs() -> i64 {
    bosun_common::time::unix_secs(SystemTime::now())
}

/// Creates the session in the store, starts its loop, and kicks off the first
/// turn when a prompt is present.
async fn start_session(
    state: &AppState,
    session: &Session,
    provider: Arc<dyn Provider>,
) -> Result<(), ApiError> {
    state.store.create_session(session).await?;
    state.loops.start(
        &session.id,
        state.store.clone(),
        provider,
        state.tunnels.clone(),
        &session.model,
    );
    match &session.prompt {
        Some(prompt) => {
            state
                .store
                .append_message(
                    &session.id,
                    Role::User,
                    &Block::Text {
                        text: prompt.clone(),
                    },
                )
                .await?;
            state.loops.wake(&session.id);
        }
        None => {
            state
                .store
                .set_state(&session.id, SessionState::WaitingForInput)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use bosun_agent::adapters::provider_for;
    use bosun_agent::config::ResolvedModel;
    use bosun_agent::provider::ProviderCall;
    use bosun_agent::provider::ProviderError;
    use bosun_agent::provider::StreamEvent;
    use bosun_common::config::ModelConfig;
    use bosun_common::types::SessionInfo;
    use bosun_common::types::UpdateStatus;
    use bosun_test_support::stub_backend;
    use bosun_test_support::wait_for;
    use futures_util::stream::BoxStream;
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    use serde_json::Value;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;

    use super::*;
    use crate::tunnel::TunnelError;

    /// A provider that answers every turn with one text delta and a stop and
    /// records each request's system prompt, so a test can see what the loop
    /// sent under a session's persona. `model` is the name the loop records
    /// on the session's model calls.
    struct RecordingProvider {
        systems: Arc<Mutex<Vec<String>>>,
        model: String,
    }

    impl RecordingProvider {
        fn new(systems: Arc<Mutex<Vec<String>>>, model: &str) -> Self {
            Self {
                systems,
                model: model.to_string(),
            }
        }
    }

    impl Provider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn chat_stream<'a>(
            &'a self,
            call: ProviderCall<'a>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            self.systems.lock().unwrap().push(call.system.to_string());
            let items: Vec<Result<StreamEvent, ProviderError>> = vec![
                Ok(StreamEvent::TextDelta("hi".into())),
                Ok(StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            ];
            Ok(stream::iter(items).boxed())
        }
    }

    /// A control-plane state backed by a fresh store in `dir`, with no models
    /// configured.
    fn test_state(dir: &tempfile::TempDir) -> Arc<AppState> {
        Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: Store::open(&dir.path().join("sessions.db")).unwrap(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )),
            providers: HashMap::new(),
            personas: HashMap::new(),
            default_persona: None,
            skills_dir: None,
        })
    }

    async fn serve(state: Arc<AppState>) -> SocketAddr {
        let app = router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// One parsed SSE frame: the `id:` line (None for live delta frames) and
    /// the JSON `data:` payload.
    struct SseFrame {
        id: Option<String>,
        data: Value,
    }

    /// Reads SSE frames from a response until `done` is satisfied. The stream
    /// stays open once the loop is running, so reading stops on the condition
    /// rather than on end-of-body.
    async fn read_sse_frames<F>(response: reqwest::Response, mut done: F) -> Vec<SseFrame>
    where
        F: FnMut(&[SseFrame]) -> bool,
    {
        read_sse_frames_until(response, Duration::from_secs(5), &mut done)
            .await
            .expect("timed out waiting for sse frames")
    }

    /// Reads SSE frames for a fixed window and returns whatever arrived, so a
    /// test can assert that nothing was emitted without waiting for the stream
    /// to end.
    async fn read_sse_frames_for(response: reqwest::Response, duration: Duration) -> Vec<SseFrame> {
        read_sse_frames_until(response, duration, &mut |_| false)
            .await
            .unwrap_or_default()
    }

    async fn read_sse_frames_until<F>(
        response: reqwest::Response,
        duration: Duration,
        done: &mut F,
    ) -> Option<Vec<SseFrame>>
    where
        F: FnMut(&[SseFrame]) -> bool,
    {
        let mut frames = Vec::new();
        let mut buffer = String::new();
        let mut stream = response.bytes_stream();
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            if done(&frames) {
                return Some(frames);
            }
            let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(error))) => panic!("failed to read an sse chunk: {error}"),
                Ok(None) | Err(_) => return None,
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let frame = buffer[..end].to_string();
                buffer = buffer[end + 2..].to_string();
                let id = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("id:").map(str::trim))
                    .map(ToString::to_string);
                let data = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data:"))
                    .unwrap_or_default();
                if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
                    frames.push(SseFrame { id, data: value });
                }
            }
        }
    }

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            node: "n1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/tmp/x".into(),
            model: "test".into(),
            persona: Some("coder".into()),
            parent_id: None,
            owner_id: id.to_string(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            state: SessionState::Creating,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: None,
        }
    }

    /// A child of `session(owner)`: born on its parent's node and directory.
    fn child_session(id: &str, owner: &str) -> Session {
        let mut child = session(id);
        child.repo_url = None;
        child.persona = Some("reviewer".into());
        child.parent_id = Some(owner.to_string());
        child.owner_id = owner.to_string();
        child.state = SessionState::Creating;
        child
    }

    /// A provider that only exists to fill the providers map; model resolution
    /// checks keys, not calls.
    struct DummyProvider;

    impl Provider for DummyProvider {
        fn name(&self) -> &str {
            "dummy"
        }

        fn model(&self) -> &str {
            "dummy"
        }

        fn chat_stream<'a>(
            &'a self,
            _call: bosun_agent::provider::ProviderCall<'a>,
        ) -> Result<
            futures_util::stream::BoxStream<
                'static,
                Result<bosun_agent::provider::StreamEvent, bosun_agent::provider::ProviderError>,
            >,
            bosun_agent::provider::ProviderError,
        > {
            Ok(stream::empty().boxed())
        }
    }

    /// A state whose providers map holds one dummy provider per model and
    /// whose persona catalog holds one persona per `(name, model)` pair, all
    /// read-write with every tool allowed.
    fn state_with_personas(
        dir: &tempfile::TempDir,
        providers: &[&str],
        personas: &[(&str, &str)],
        default_persona: Option<&str>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: Store::open(&dir.path().join("sessions.db")).unwrap(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )),
            providers: providers
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        Arc::new(DummyProvider) as Arc<dyn Provider>,
                    )
                })
                .collect(),
            personas: personas
                .iter()
                .map(|(name, model)| {
                    (
                        name.to_string(),
                        PersonaConfig {
                            model: model.to_string(),
                            permission: Permission::ReadWrite,
                            allowed_tools: "*".into(),
                            description: String::new(),
                            system_prompt: None,
                        },
                    )
                })
                .collect(),
            default_persona: default_persona.map(ToString::to_string),
            skills_dir: None,
        })
    }

    fn persona_for<'a>(state: &'a AppState, name: &str) -> (&'a PersonaConfig, String) {
        let requested = Some(name.to_string());
        let (_name, persona, _provider) = state.resolve_persona(&requested).unwrap();
        (persona, persona.model.clone())
    }

    /// A persona config with the given surface, used by the switch tests.
    fn persona_config(
        model: &str,
        permission: Permission,
        allowed_tools: &str,
        system_prompt: Option<&str>,
    ) -> PersonaConfig {
        PersonaConfig {
            model: model.to_string(),
            permission,
            allowed_tools: allowed_tools.to_string(),
            description: String::new(),
            system_prompt: system_prompt.map(ToString::to_string),
        }
    }

    /// A state built the way main() builds one: the loop registry and the
    /// request path share the same provider and persona maps, so a persona
    /// switch re-resolves the loop's provider from the session's new model.
    fn state_with_catalog(
        dir: &tempfile::TempDir,
        providers: HashMap<String, Arc<dyn Provider>>,
        personas: HashMap<String, PersonaConfig>,
        default_persona: Option<&str>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: Store::open(&dir.path().join("sessions.db")).unwrap(),
            loops: Arc::new(AgentRegistry::new(
                None,
                providers.clone(),
                personas.clone(),
                HashMap::new(),
            )),
            providers,
            personas,
            default_persona: default_persona.map(ToString::to_string),
            skills_dir: None,
        })
    }

    #[test]
    fn resolve_persona_prefers_the_requested_name() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(
            &dir,
            &["alpha", "beta"],
            &[("coder", "beta"), ("reviewer", "alpha")],
            Some("reviewer"),
        );
        let (persona, model) = persona_for(&state, "coder");
        assert_eq!(persona.model, "beta");
        assert_eq!(model, "beta");
    }

    #[test]
    fn resolve_persona_uses_the_default_when_none_is_requested() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(
            &dir,
            &["alpha", "beta"],
            &[("coder", "beta"), ("reviewer", "alpha")],
            Some("reviewer"),
        );
        let (_name, persona, _provider) = state.resolve_persona(&None).unwrap();
        assert_eq!(persona.model, "alpha", "the default persona wins");
    }

    #[test]
    fn resolve_persona_without_a_default_is_no_persona() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(&dir, &["alpha"], &[("coder", "alpha")], None);
        assert!(matches!(
            state.resolve_persona(&None),
            Err(ApiError::NoPersona)
        ));
    }

    #[test]
    fn resolve_persona_rejects_an_unknown_requested_name() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(&dir, &["alpha"], &[("coder", "alpha")], Some("coder"));
        let requested = Some("ghost".to_string());
        assert!(matches!(
            state.resolve_persona(&requested),
            Err(ApiError::PersonaNotFound { persona }) if persona == "ghost"
        ));
    }

    #[test]
    fn resolve_persona_rejects_an_unknown_default() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(&dir, &["alpha"], &[("coder", "alpha")], Some("ghost"));
        assert!(matches!(
            state.resolve_persona(&None),
            Err(ApiError::PersonaNotFound { persona }) if persona == "ghost"
        ));
    }

    #[test]
    fn resolve_persona_rejects_a_persona_whose_model_has_no_provider() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(&dir, &[], &[("coder", "ghost")], Some("coder"));
        assert!(matches!(
            state.resolve_persona(&None),
            Err(ApiError::PersonaModelNotFound { persona, model })
                if persona == "coder" && model == "ghost"
        ));
    }

    #[tokio::test]
    async fn recover_marks_running_and_creating_interrupted() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let mut running = session("running");
        running.state = SessionState::Running;
        let mut creating = session("creating");
        creating.state = SessionState::Creating;
        let mut waiting = session("waiting");
        waiting.state = SessionState::WaitingForInput;
        state.store.create_session(&running).await.unwrap();
        state.store.create_session(&creating).await.unwrap();
        state.store.create_session(&waiting).await.unwrap();

        recover(&state).await;

        assert_eq!(
            state
                .store
                .get_session("running")
                .await
                .unwrap()
                .unwrap()
                .state,
            SessionState::Interrupted
        );
        assert_eq!(
            state
                .store
                .get_session("running")
                .await
                .unwrap()
                .unwrap()
                .interrupt_cause,
            Some(InterruptCause::Crash),
            "a boot-time interruption is recorded as a crash"
        );
        assert_eq!(
            state
                .store
                .get_session("creating")
                .await
                .unwrap()
                .unwrap()
                .state,
            SessionState::Interrupted
        );
        assert_eq!(
            state
                .store
                .get_session("waiting")
                .await
                .unwrap()
                .unwrap()
                .state,
            SessionState::WaitingForInput
        );
        assert_eq!(
            state
                .store
                .get_session("waiting")
                .await
                .unwrap()
                .unwrap()
                .interrupt_cause,
            None,
            "a session that was not mid-flight is not interrupted"
        );
    }

    #[tokio::test]
    async fn recover_starts_loops_for_sessions_with_a_configured_model() {
        let dir = tempdir().unwrap();
        let state = state_with_personas(&dir, &["test"], &[("coder", "test")], None);
        let mut ghost = session("ghost");
        ghost.model = "ghost".into();
        state.store.create_session(&session("s1")).await.unwrap();
        state.store.create_session(&ghost).await.unwrap();

        recover(&state).await;

        assert!(
            state.loops.subscribe("s1").is_some(),
            "a session with a configured model gets a loop"
        );
        assert!(
            state.loops.subscribe("ghost").is_none(),
            "a session without a configured model gets no loop"
        );
    }

    #[tokio::test]
    async fn the_sessions_api_reports_parent_and_owner_for_clients_to_render_the_tree() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("child-1", "root-1"))
            .await
            .unwrap();

        let sessions: Value = client
            .get(format!("http://{addr}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let sessions = sessions.as_array().unwrap();
        let root = sessions
            .iter()
            .find(|s| s["id"] == "root-1")
            .expect("the root session is listed");
        assert_eq!(root["parent_id"], serde_json::Value::Null);
        assert_eq!(root["owner_id"], "root-1", "a root session owns itself");
        let child = sessions
            .iter()
            .find(|s| s["id"] == "child-1")
            .expect("the child session is listed");
        assert_eq!(child["parent_id"], "root-1");
        assert_eq!(child["owner_id"], "root-1");

        let detail: Value = client
            .get(format!("http://{addr}/sessions/child-1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["parent_id"], "root-1");
        assert_eq!(detail["owner_id"], "root-1");
    }

    #[tokio::test]
    async fn a_child_session_accepts_no_user_messages() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("child-1", "root-1"))
            .await
            .unwrap();

        let response = client
            .post(format!("http://{addr}/sessions/child-1/messages"))
            .json(&json!({ "content": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("watch-only"), "{text}");
        assert!(
            store.messages("child-1", true).await.unwrap().is_empty(),
            "a refused message records nothing"
        );

        // The owner keeps accepting input.
        let response = client
            .post(format!("http://{addr}/sessions/root-1/messages"))
            .json(&json!({ "content": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.messages("root-1", true).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mutating_endpoints_refuse_a_child_session() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("child-1", "root-1"))
            .await
            .unwrap();

        // interrupt, permission, persona, and stop are user actions, and a
        // child is watch-only: every one is refused.
        let refused: Vec<(&str, reqwest::Response)> = vec![
            ("interrupt", {
                client
                    .post(format!("http://{addr}/sessions/child-1/interrupt"))
                    .send()
                    .await
                    .unwrap()
            }),
            ("permission", {
                client
                    .post(format!("http://{addr}/sessions/child-1/permission"))
                    .json(&json!({ "permission": "read_only" }))
                    .send()
                    .await
                    .unwrap()
            }),
            ("persona", {
                client
                    .post(format!("http://{addr}/sessions/child-1/persona"))
                    .json(&json!({ "persona": "coder" }))
                    .send()
                    .await
                    .unwrap()
            }),
            ("stop", {
                client
                    .post(format!("http://{addr}/stop"))
                    .json(&json!({ "session_id": "child-1" }))
                    .send()
                    .await
                    .unwrap()
            }),
        ];
        for (action, response) in refused {
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{action} on a child session must be refused"
            );
            let text = response.text().await.unwrap();
            assert!(text.contains("watch-only"), "{action}: {text}");
        }

        let child = store
            .get_session("child-1")
            .await
            .unwrap()
            .expect("a refused stop leaves the child stored");
        assert_eq!(child.state, SessionState::Creating, "no action touched it");
        assert_eq!(
            child.interrupt_cause, None,
            "a refused interrupt records no cause"
        );

        // The owner keeps accepting the same actions.
        let response = client
            .post(format!("http://{addr}/sessions/root-1/interrupt"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let response = client
            .post(format!("http://{addr}/sessions/root-1/permission"))
            .json(&json!({ "permission": "read_only" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn wants_tunnel_upgrade_accepts_the_bosun_tunnel_protocol() {
        let req = HttpRequest::builder()
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "bosun-tunnel")
            .body(Body::empty())
            .unwrap();
        assert!(wants_tunnel_upgrade(&req));

        let plain = HttpRequest::builder().body(Body::empty()).unwrap();
        assert!(!wants_tunnel_upgrade(&plain));

        let ws = HttpRequest::builder()
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .body(Body::empty())
            .unwrap();
        assert!(!wants_tunnel_upgrade(&ws));
    }

    #[tokio::test]
    async fn tunnel_route_carries_the_upgrade() {
        use http_body_util::BodyExt;

        let backend_addr = stub_backend().await;
        let session_id = uuid::Uuid::new_v4().to_string();

        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .store
            .create_session(&Session {
                id: session_id.clone(),
                node: "n1".into(),
                repo_url: None,
                git_ref: None,
                dir: "/tmp/x".into(),
                model: "test".into(),
                persona: Some("coder".into()),
                parent_id: None,
                owner_id: session_id.clone(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                state: SessionState::WaitingForInput,
                interrupt_cause: None,
                created_at_secs: 1_700_000_000,
                prompt: None,
            })
            .await
            .unwrap();
        let addr = serve(state.clone()).await;

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            http1::handshake::<_, http_body_util::Empty<bytes::Bytes>>(TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let request = HttpRequest::builder()
            .method("GET")
            .uri(format!("/tunnel/session/{session_id}"))
            .header(header::HOST, addr.to_string())
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "bosun-tunnel")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        // The test owns the node side of the upgraded connection: relay every
        // opened logical connection to the stub backend.
        let upgraded = hyper::upgrade::on(response).await.unwrap();
        let (node_tunnel, mut opens) = Tunnel::new(TokioIo::new(upgraded));
        tokio::spawn(async move {
            while let Some(event) = opens.recv().await {
                let tunnel = node_tunnel.clone();
                tokio::spawn(async move {
                    let mut backend = TcpStream::connect(backend_addr).await.unwrap();
                    let mut logical = tunnel.attach(event.conn_id, event.rx).unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut backend, &mut logical).await;
                });
            }
        });

        // The control plane registers the tunnel after the 101; a request
        // opened on it then reaches the relayed backend.
        let tunnels = state.tunnels.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stream = match tunnels.open(&session_id).await {
                Ok(stream) => stream,
                Err(TunnelError::NoTunnel { .. }) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                Err(TunnelError::NoTunnel { .. }) => {
                    panic!("the tunnel never registered after the 101");
                }
                Err(TunnelError::TunnelClosed { .. }) => panic!("the tunnel closed"),
            };
            let (mut sender, conn) =
                http1::handshake::<_, http_body_util::Empty<bytes::Bytes>>(TokioIo::new(stream))
                    .await
                    .unwrap();
            tokio::spawn(async move {
                let _ = conn.with_upgrades().await;
            });
            let request = HttpRequest::builder()
                .method("GET")
                .uri("/")
                .header(header::HOST, "executor")
                .body(http_body_util::Empty::<bytes::Bytes>::new())
                .unwrap();
            let response =
                tokio::time::timeout(Duration::from_secs(5), sender.send_request(request))
                    .await
                    .expect("the request through the tunnel timed out")
                    .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], b"ok");
            break;
        }
    }

    #[tokio::test]
    async fn pane_is_served_at_the_root() {
        let dir = tempdir().unwrap();
        let addr = serve(test_state(&dir)).await;
        let client = reqwest::Client::new();

        for path in ["/", "/ui"] {
            let response = client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path} serves the pane");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("text/html; charset=utf-8"),
                "{path} is text/html"
            );
            let body = response.text().await.unwrap();
            assert!(body.contains("Bosun"), "{path} contains the pane title");
        }
    }

    #[tokio::test]
    async fn poll_response_carries_the_control_plane_version() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        // A queued command makes the poll answer immediately instead of
        // holding for the queue's timeout.
        let (reply, _reply_rx) = oneshot::channel();
        state.commands.enqueue(
            "node-1",
            NodeCommand::Stop {
                id: 1,
                session_id: "s1".into(),
            },
            Some(reply),
        );

        let response = client
            .post(format!("http://{addr}/poll"))
            .json(&json!({
                "node_name": "node-1",
                "status": "up",
                "version": "0.5.5",
                "result": null
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(bosun_common::types::X_BOSUN_VERSION)
                .and_then(|value| value.to_str().ok()),
            Some(bosun_common::version::VERSION),
            "every response carries the control-plane version"
        );
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["version"], bosun_common::version::VERSION);
    }

    #[tokio::test]
    async fn poll_records_the_node_version_in_the_registry() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let (reply, _reply_rx) = oneshot::channel();
        state.commands.enqueue(
            "node-1",
            NodeCommand::Stop {
                id: 1,
                session_id: "s1".into(),
            },
            Some(reply),
        );

        let response = client
            .post(format!("http://{addr}/poll"))
            .json(&json!({
                "node_name": "node-1",
                "status": "up",
                "version": "0.9.0",
                "result": null
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let nodes: Value = client
            .get(format!("http://{addr}/nodes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(nodes[0]["name"], "node-1");
        assert_eq!(nodes[0]["version"], "0.9.0");
    }

    #[tokio::test]
    async fn poll_records_the_node_update_status_in_the_registry() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let (reply, _reply_rx) = oneshot::channel();
        state.commands.enqueue(
            "node-1",
            NodeCommand::Stop {
                id: 1,
                session_id: "s1".into(),
            },
            Some(reply),
        );
        client
            .post(format!("http://{addr}/poll"))
            .json(&json!({
                "node_name": "node-1",
                "status": "up",
                "version": "0.5.5",
                "update_status": {"failed": "checksum mismatch"},
                "result": null
            }))
            .send()
            .await
            .unwrap();

        let (reply, _reply_rx) = oneshot::channel();
        state.commands.enqueue(
            "node-2",
            NodeCommand::Stop {
                id: 1,
                session_id: "s1".into(),
            },
            Some(reply),
        );
        client
            .post(format!("http://{addr}/poll"))
            .json(&json!({
                "node_name": "node-2",
                "status": "up",
                "version": "0.5.5",
                "update_status": "disabled",
                "result": null
            }))
            .send()
            .await
            .unwrap();

        let nodes: Value = client
            .get(format!("http://{addr}/nodes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(nodes[0]["update_status"]["failed"], "checksum mismatch");
        assert_eq!(nodes[1]["update_status"], "disabled");
    }

    /// A fake OpenAI-compatible provider: answers every chat completion with
    /// one text delta and a stop, so a wake completes a full turn.
    async fn fake_provider() -> SocketAddr {
        fake_provider_with_delay(Duration::ZERO).await
    }

    /// A fake provider that holds the first delta back for `delay`, so a test
    /// can connect to the events stream while a turn is still in flight.
    async fn fake_provider_with_delay(delay: Duration) -> SocketAddr {
        fake_provider_with_delay_and_requests(delay, Arc::new(AtomicUsize::new(0)))
            .await
            .0
    }

    /// The fake provider's address plus a counter of chat requests it has
    /// started, so a test can switch a persona while a turn is in flight.
    async fn fake_provider_with_delay_and_requests(
        delay: Duration,
        requests: Arc<AtomicUsize>,
    ) -> (SocketAddr, Arc<AtomicUsize>) {
        use axum::routing::post;

        let requests_for_server = requests.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let requests = requests_for_server.clone();
                async move { completions(delay, requests).await }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, requests)
    }

    /// A fake OpenAI-compatible provider that serves one scripted completion
    /// per request, in order. Each script holds the JSON chunk payloads of
    /// one streaming answer; the closing `[DONE]` marker is appended here.
    /// Two loops need two providers, because each loop consumes its own
    /// provider's scripts.
    async fn scripted_provider(scripts: Arc<Mutex<VecDeque<Vec<Value>>>>) -> SocketAddr {
        use axum::routing::post;

        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let scripts = scripts.clone();
                async move { scripted_completions(scripts).await }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn scripted_completions(
        scripts: Arc<Mutex<VecDeque<Vec<Value>>>>,
    ) -> axum::response::Response {
        let chunks = scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("no script left for the provider call");
        let mut frames = chunks
            .into_iter()
            .map(|chunk| {
                Ok::<_, Infallible>(
                    SseEvent::default()
                        .json_data(&chunk)
                        .expect("serializing a json value cannot fail"),
                )
            })
            .collect::<Vec<_>>();
        frames.push(Ok(SseEvent::default().data("[DONE]")));
        Sse::new(futures_util::stream::iter(frames)).into_response()
    }

    /// One OpenAI text chunk.
    fn text_chunk(text: &str) -> Value {
        json!({ "choices": [{ "index": 0, "delta": { "content": text } }] })
    }

    /// One fragment of a tool call's arguments. The first fragment of a call
    /// carries its id and name, like a real provider stream.
    fn tool_call_fragment(id: &str, name: &str, args_fragment: &str) -> Value {
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": args_fragment },
        }] } }] })
    }

    /// One fragment of the `spawn` tool call's arguments. The first fragment
    /// of a call carries its id and name, like a real provider stream.
    fn spawn_call_fragment(with_id_name: bool, args_fragment: &str) -> Value {
        let mut function = json!({ "arguments": args_fragment });
        if with_id_name {
            function["name"] = json!("spawn");
        }
        let mut tool_call = json!({ "index": 0, "type": "function", "function": function });
        if with_id_name {
            tool_call["id"] = json!("call-1");
        }
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [tool_call] } }] })
    }

    async fn completions(delay: Duration, requests: Arc<AtomicUsize>) -> axum::response::Response {
        requests.fetch_add(1, Ordering::Relaxed);
        let stream = futures_util::stream::unfold(0u8, move |step| async move {
            match step {
                0 => {
                    tokio::time::sleep(delay).await;
                    Some((
                        Ok::<_, Infallible>(
                            SseEvent::default()
                                .json_data(json!({
                                    "choices": [{ "index": 0, "delta": { "content": "hi" } }]
                                }))
                                .unwrap(),
                        ),
                        1,
                    ))
                }
                1 => Some((Ok::<_, Infallible>(SseEvent::default().data("[DONE]")), 2)),
                _ => None,
            }
        });
        Sse::new(stream).into_response()
    }

    fn openai_provider(addr: SocketAddr) -> Arc<dyn Provider> {
        openai_provider_with_model(addr, "test")
    }

    fn openai_provider_with_model(addr: SocketAddr, model: &str) -> Arc<dyn Provider> {
        let resolved = ResolvedModel {
            config: ModelConfig {
                provider: "openai".into(),
                name: model.into(),
                base_url: Some(format!("http://{addr}")),
                api_key: "x".into(),
                price_input_per_mtok: 0.0,
                price_output_per_mtok: 0.0,
            },
            api_key: "x".into(),
        };
        Arc::from(provider_for(&resolved).unwrap())
    }

    #[tokio::test]
    async fn create_session_then_messages_drive_the_loop() {
        let provider_addr = fake_provider().await;
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: store.clone(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )),
            providers: HashMap::from([("test".to_string(), openai_provider(provider_addr))]),
            personas: HashMap::from([(
                "test".to_string(),
                PersonaConfig {
                    model: "test".into(),
                    permission: Permission::ReadWrite,
                    allowed_tools: "*".into(),
                    description: String::new(),
                    system_prompt: None,
                },
            )]),
            default_persona: Some("test".into()),
            skills_dir: None,
        });
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "persona": "test",
                "prompt": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session: Value = response.json().await.unwrap();
        let id = session["id"].as_str().unwrap().to_string();
        wait_for("the session to wait for input", || {
            let client = client.clone();
            let id = id.clone();
            async move {
                let sessions: Value = client
                    .get(format!("http://{addr}/sessions"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                sessions.as_array().unwrap().iter().any(|s| {
                    s["id"].as_str() == Some(id.as_str()) && s["state"] == "waiting_for_input"
                })
            }
        })
        .await;

        let frames = read_sse_frames(
            client
                .get(format!("http://{addr}/sessions/{id}/events?after=0"))
                .send()
                .await
                .unwrap(),
            |frames| {
                let has_assistant_text = frames.iter().any(|frame| {
                    let event = &frame.data["event"];
                    event["kind"] == "message"
                        && event["message"]["role"] == "assistant"
                        && event["message"]["block"]["kind"] == "text"
                        && event["message"]["block"]["text"] == "hi"
                });
                let has_waiting_state = frames.iter().any(|frame| {
                    frame.data["event"]["kind"] == "state"
                        && frame.data["event"]["state"] == "waiting_for_input"
                });
                has_assistant_text && has_waiting_state
            },
        )
        .await;
        let seqs: Vec<i64> = frames
            .iter()
            .map(|frame| frame.data["seq"].as_i64().unwrap())
            .collect();
        assert!(
            seqs.windows(2).all(|pair| pair[0] < pair[1]),
            "event seqs are strictly increasing: {seqs:?}"
        );
        assert!(
            frames.iter().all(|frame| frame.id.is_some()),
            "every durable frame carries its seq as the sse id"
        );

        let response = client
            .post(format!("http://{addr}/sessions/{id}/messages"))
            .json(&json!({ "content": "again" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the second turn to answer", || {
            let store = store.clone();
            let id = id.clone();
            async move { store.messages(&id, true).await.unwrap().len() == 4 }
        })
        .await;

        wait_for("the session to wait for input", || {
            let store = store.clone();
            let id = id.clone();
            async move {
                let stored = store.get_session(&id).await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let response = client
            .post(format!("http://{addr}/sessions/{id}/interrupt"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // The session is idle at waiting_for_input, so the interrupt is
        // ignored: it is not a killed turn.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stored = store.get_session(&id).await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::WaitingForInput);

        let response = client
            .post(format!("http://{addr}/sessions/{id}/permission"))
            .json(&json!({ "permission": "read_only" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the permission to change", || {
            let store = store.clone();
            let id = id.clone();
            async move {
                let stored = store.get_session(&id).await.unwrap().unwrap();
                stored.permission == Permission::ReadOnly
            }
        })
        .await;

        let response = client
            .post(format!("http://{addr}/stop"))
            .json(&json!({ "session_id": id.clone() }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let sessions: Value = client
            .get(format!("http://{addr}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sessions.as_array().unwrap().len(), 0);

        let response = client
            .post(format!("http://{addr}/stop"))
            .json(&json!({ "session_id": id }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_session_derives_model_permission_and_allowed_tools_from_its_persona() {
        let provider_addr = fake_provider().await;
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: store.clone(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )),
            providers: HashMap::from([("test".to_string(), openai_provider(provider_addr))]),
            personas: HashMap::from([(
                "reviewer".to_string(),
                PersonaConfig {
                    model: "test".into(),
                    permission: Permission::ReadOnly,
                    allowed_tools: "file/read, grep".into(),
                    description: "Reviews without touching".into(),
                    system_prompt: None,
                },
            )]),
            default_persona: Some("reviewer".into()),
            skills_dir: None,
        });
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        // No persona named: the default persona resolves, and the request's
        // permission field no longer exists on the wire.
        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "prompt": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session: Value = response.json().await.unwrap();
        assert_eq!(session["model"], "test");
        assert_eq!(session["permission"], "read_only");
        assert_eq!(session["allowed_tools"], "file/read, grep");
        assert_eq!(
            session["persona"], "reviewer",
            "the default persona is stored on the session"
        );
        assert_eq!(
            session["parent_id"],
            serde_json::Value::Null,
            "a session the user starts is a root"
        );
        assert_eq!(
            session["owner_id"], session["id"],
            "a root session owns itself"
        );
        assert_eq!(
            session["interrupt_cause"],
            serde_json::Value::Null,
            "a fresh session has not been interrupted"
        );

        // The loop starts and idles once the turn completes.
        let id = session["id"].as_str().unwrap().to_string();
        wait_for("the session to wait for input", || {
            let store = store.clone();
            let id = id.clone();
            async move {
                let stored = store.get_session(&id).await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;
    }

    #[tokio::test]
    async fn spawn_creates_a_child_session_that_runs_concurrently_and_reports() {
        // The root's model answers with a spawn call and then an
        // acknowledgement; the child's model (a separate persona and provider)
        // answers with the review text. The child's completion wakes the root
        // once more, so a third root script reacts to the authored report.
        // Two scripted servers keep the two loops' scripts apart, so the
        // interleaving stays deterministic.
        let root_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                vec![spawn_call_fragment(
                    true,
                    r#"{"persona":"reviewer","instructions":"review the change"}"#,
                )],
                vec![text_chunk("acknowledged")],
                vec![text_chunk("thanks for the review")],
            ])));
        let child_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> = Arc::new(Mutex::new(VecDeque::from(
            vec![vec![text_chunk("the change looks good")]],
        )));
        let root_addr = scripted_provider(root_scripts).await;
        let child_addr = scripted_provider(child_scripts).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let node_timeout = Duration::from_secs(4);
        let nodes = Arc::new(NodeRegistry::new(node_timeout));
        let commands = Arc::new(CommandQueue::new(node_timeout));
        let tunnels = Arc::new(TunnelRegistry::new());
        let providers = HashMap::from([
            (
                "main-model".to_string(),
                openai_provider_with_model(root_addr, "main-model"),
            ),
            (
                "reviewer-model".to_string(),
                openai_provider_with_model(child_addr, "reviewer-model"),
            ),
        ]);
        let personas = HashMap::from([
            (
                "coder".to_string(),
                PersonaConfig {
                    model: "main-model".into(),
                    permission: Permission::ReadWrite,
                    allowed_tools: "*".into(),
                    description: "Makes changes".into(),
                    system_prompt: None,
                },
            ),
            (
                "reviewer".to_string(),
                PersonaConfig {
                    model: "reviewer-model".into(),
                    permission: Permission::ReadWrite,
                    allowed_tools: "*".into(),
                    description: "Reviews changes".into(),
                    system_prompt: None,
                },
            ),
        ]);
        let loops = Arc::new(AgentRegistry::new(
            None,
            providers.clone(),
            personas.clone(),
            HashMap::new(),
        ));
        let state = Arc::new(AppState {
            registry: nodes.clone(),
            commands: commands.clone(),
            tunnels: tunnels.clone(),
            store: store.clone(),
            loops,
            providers,
            personas,
            default_persona: Some("coder".into()),
            skills_dir: None,
        });
        state.loops.attach_child_spawner(nodes, commands, tunnels);

        // A node that answers `Dev` commands like a real node: it starts an
        // executor on the requested dir and reports the session. The commands
        // it received are captured, so the test can see the child's executor
        // was requested on the parent's working copy.
        let seen: Arc<Mutex<Vec<NodeCommand>>> = Arc::new(Mutex::new(Vec::new()));
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();
        let seen_for_node = seen.clone();
        let addr_for_node = addr;
        tokio::spawn(async move {
            let mut result: Option<CommandResult> = None;
            loop {
                let poll = json!({
                    "node_name": "n1",
                    "status": "up",
                    "version": bosun_common::version::VERSION,
                    "result": result,
                });
                let response: Value = match client
                    .post(format!("http://{addr_for_node}/poll"))
                    .json(&poll)
                    .send()
                    .await
                {
                    Ok(response) => response.json().await.unwrap(),
                    Err(_) => break,
                };
                let Some(command) = response["command"].clone().as_object().cloned() else {
                    result = None;
                    continue;
                };
                let command: NodeCommand = serde_json::from_value(Value::Object(command)).unwrap();
                seen_for_node.lock().unwrap().push(command.clone());
                match command {
                    NodeCommand::Dev {
                        ref dir,
                        ref session_id,
                        ..
                    } => {
                        let id = command.id();
                        result = Some(CommandResult::Session {
                            id,
                            session: SessionInfo {
                                id: session_id.clone(),
                                repo_url: None,
                                git_ref: None,
                                dir: Some(dir.clone()),
                                status: "running".into(),
                            },
                        });
                    }
                    NodeCommand::Stop { .. } => {
                        result = Some(CommandResult::Stop { id: command.id() });
                    }
                    _ => break,
                }
            }
        });

        // Register the node before the root session exists: the spawn tool
        // call refuses a node that is not up.
        let state_for_ready = state.clone();
        wait_for("the fake node to register", move || {
            let state = state_for_ready.clone();
            async move { state.registry.node("n1", SystemTime::now()).is_some() }
        })
        .await;

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/work/repo",
                "persona": "coder",
                "prompt": "review the change for me"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let root: Value = response.json().await.unwrap();
        let root_id = root["id"].as_str().unwrap().to_string();

        // The root's first turn calls spawn; the tool result carries the
        // child's id, and the root's turn continues to its own second answer.
        wait_for("the spawn tool result with the child id to land", || {
            let store = store.clone();
            let root_id = root_id.clone();
            async move {
                let messages = store.messages(&root_id, false).await.unwrap();
                messages.iter().any(|(_, message)| {
                    matches!(&message.block, Block::ToolResult { name, content, .. } if name == "spawn" && content.get("child_id").is_some())
                })
            }
        })
        .await;
        let root_messages = store.messages(&root_id, false).await.unwrap();
        let child_id = root_messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::ToolResult { name, content, .. } if name == "spawn" => {
                    content["child_id"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .expect("the spawn result names the child");

        // The child is a full session on the parent's node and working copy,
        // under its own persona, and its executor was requested through a Dev
        // command for its own session id.
        wait_for("the child to run its assignment and stop", || {
            let store = store.clone();
            let child_id = child_id.clone();
            async move {
                let stored = store.get_session(&child_id).await.unwrap().unwrap();
                stored.state == SessionState::Stopped
            }
        })
        .await;
        let child = store
            .get_session(&child_id)
            .await
            .unwrap()
            .expect("the child session row exists");
        assert_eq!(child.parent_id.as_deref(), Some(root_id.as_str()));
        assert_eq!(child.owner_id, root_id);
        assert_eq!(child.node, "n1");
        assert_eq!(
            child.dir, "/work/repo",
            "the child runs on the parent's working copy"
        );
        assert_eq!(child.persona.as_deref(), Some("reviewer"));
        assert_eq!(child.model, "reviewer-model");
        assert_eq!(child.permission, Permission::ReadWrite);
        assert_eq!(
            child.prompt.as_deref(),
            Some("review the change"),
            "the assignment is stored as the child's prompt"
        );

        let dev = seen
            .lock()
            .unwrap()
            .iter()
            .find_map(|command| match command {
                NodeCommand::Dev {
                    session_id,
                    dir,
                    permission,
                    ..
                } if session_id == &child_id => Some((dir.clone(), *permission)),
                _ => None,
            });
        let (dev_dir, dev_permission) =
            dev.expect("a Dev command for the child's executor was queued");
        assert_eq!(dev_dir, std::path::PathBuf::from("/work/repo"));
        assert_eq!(dev_permission, Permission::ReadWrite);

        // The child stored its own transcript and model call; the parent's
        // thread shows exactly one authored report, not the child's raw work.
        let child_messages = store.messages(&child_id, false).await.unwrap();
        let texts: Vec<&str> = child_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            ["review the change", "the change looks good"],
            "the child's thread is its assignment followed by its own answer"
        );
        assert_eq!(store.model_calls(&child_id).await.unwrap().len(), 1);

        wait_for("the child report to reach the parent's thread", || {
            let store = store.clone();
            let root_id = root_id.clone();
            let child_id = child_id.clone();
            async move {
                let messages = store.messages(&root_id, false).await.unwrap();
                messages.iter().any(|(_, message)| matches!(
                    &message.block,
                    Block::ChildEvent { child_id: id, text, .. } if id == &child_id && text == "the change looks good"
                ))
            }
        })
        .await;
        let root_messages = store.messages(&root_id, false).await.unwrap();
        let reports: Vec<&str> = root_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::ChildEvent { child_id: id, .. } if id == &child_id => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            [child_id.as_str()],
            "one authored report per completion"
        );

        // The root's own turn finished and the session waits for the user.
        wait_for("the root to wait for input", || {
            let store = store.clone();
            let root_id = root_id.clone();
            async move {
                let stored = store.get_session(&root_id).await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;
    }

    /// Builds a control-plane state whose root and its one child each run a
    /// loop against their own scripted provider: two sessions, two models,
    /// two fake providers, like the spawn test's wiring. The child's thread
    /// starts with an assignment, so waking the child makes it ask. Returns
    /// the state, the store, and the root and child ids.
    async fn state_with_child_tree(
        dir: &tempfile::TempDir,
        root_scripts: Arc<Mutex<VecDeque<Vec<Value>>>>,
        child_scripts: Arc<Mutex<VecDeque<Vec<Value>>>>,
    ) -> (Arc<AppState>, Store, String, String) {
        let root_addr = scripted_provider(root_scripts).await;
        let child_addr = scripted_provider(child_scripts).await;
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let mut root = session("root-s6");
        root.model = "root-model".into();
        root.persona = None;
        let mut child = child_session("child-s6", "root-s6");
        child.model = "child-model".into();
        child.persona = None;
        store.create_session(&root).await.unwrap();
        store.create_session(&child).await.unwrap();
        store
            .append_message(
                &child.id,
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();
        let providers = HashMap::from([
            (
                "root-model".to_string(),
                openai_provider_with_model(root_addr, "root-model"),
            ),
            (
                "child-model".to_string(),
                openai_provider_with_model(child_addr, "child-model"),
            ),
        ]);
        let tunnels = Arc::new(TunnelRegistry::new());
        let loops = Arc::new(AgentRegistry::new(
            None,
            providers.clone(),
            HashMap::new(),
            HashMap::new(),
        ));
        loops.start(
            "root-s6",
            store.clone(),
            providers["root-model"].clone(),
            tunnels.clone(),
            "root-model",
        );
        loops.start(
            "child-s6",
            store.clone(),
            providers["child-model"].clone(),
            tunnels.clone(),
            "child-model",
        );
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels,
            store: store.clone(),
            loops,
            providers,
            personas: HashMap::new(),
            default_persona: None,
            skills_dir: None,
        });
        (state, store, root.id, child.id)
    }

    #[tokio::test]
    async fn user_actions_are_refused_on_a_grandchild_like_on_any_child() {
        // The watch-only guards gate on parent_id, so they hold at any depth:
        // messages, interrupts, permission and persona changes, and stops all
        // refuse a grandchild session exactly as they refuse a child.
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-watch")).await.unwrap();
        store
            .create_session(&child_session("child-watch", "root-watch"))
            .await
            .unwrap();
        let mut grandchild = child_session("grand-watch", "child-watch");
        grandchild.owner_id = "root-watch".into();
        store.create_session(&grandchild).await.unwrap();
        let state = state_with_catalog(
            &dir,
            HashMap::from([(
                "test".to_string(),
                Arc::new(DummyProvider) as Arc<dyn Provider>,
            )]),
            HashMap::new(),
            None,
        );
        let addr = serve(state).await;
        let client = reqwest::Client::new();
        let url = |path: &str| format!("http://{addr}{path}");

        let bodies: Vec<(String, Option<serde_json::Value>)> = vec![
            (
                "/sessions/grand-watch/messages".to_string(),
                Some(json!({ "content": "hi" })),
            ),
            ("/sessions/grand-watch/interrupt".to_string(), None),
            (
                "/sessions/grand-watch/permission".to_string(),
                Some(json!({ "permission": "read_only" })),
            ),
            (
                "/sessions/grand-watch/persona".to_string(),
                Some(json!({ "persona": "coder" })),
            ),
            (
                "/stop".to_string(),
                Some(json!({ "session_id": "grand-watch" })),
            ),
        ];
        for (path, body) in bodies {
            let mut request = client.post(url(&path));
            if let Some(body) = body {
                request = request.json(&body);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            let text = response.text().await.unwrap();
            assert!(
                text.contains("watch-only"),
                "{path} refused as watch-only: {text}"
            );
        }
        assert!(
            store.get_session("grand-watch").await.unwrap().is_some(),
            "a refused stop leaves the grandchild in place"
        );
    }

    #[tokio::test]
    async fn a_user_answer_to_a_surfaced_child_ask_routes_to_the_child_without_a_root_model_call() {
        let answer = "yes, push to main";
        let root_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                // The root surfaces the child's question, bound to the child.
                vec![tool_call_fragment(
                    "call-1",
                    "ask",
                    r#"{"message":"may I push?","options":["yes","no"],"child_id":"child-s6"}"#,
                )],
                // The root's only later turn reacts to the child's completion
                // report: the answer itself is routed by the control plane, so
                // no root turn relays it.
                vec![text_chunk("noted the report")],
            ])));
        let child_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                vec![tool_call_fragment(
                    "call-1",
                    "ask",
                    r#"{"message":"may I push?","options":["yes","no"]}"#,
                )],
                vec![text_chunk("pushed to main")],
            ])));
        let dir = tempdir().unwrap();
        let (state, store, root_id, child_id) =
            state_with_child_tree(&dir, root_scripts, child_scripts).await;
        // The child's assignment is in its thread; waking it makes it ask.
        state.loops.wake(&child_id);
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        wait_for("the child to ask and the root to surface the bound ask", {
            let store = store.clone();
            let root_id = root_id.clone();
            move || {
                let store = store.clone();
                let root_id = root_id.clone();
                async move {
                    let stored = store.get_session(&root_id).await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.get_pending_ask(&root_id).await.unwrap().is_some()
                        && store.model_calls(&root_id).await.unwrap().len() == 1
                }
            }
        })
        .await;

        // The user answers. The answer routes verbatim to the child and no
        // root model call is spent on it.
        let response = client
            .post(format!("http://{addr}/sessions/{root_id}/messages"))
            .json(&json!({ "content": answer }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the child to resume on the answer and finish", {
            let store = store.clone();
            let child_id = child_id.clone();
            move || {
                let store = store.clone();
                let child_id = child_id.clone();
                async move {
                    let stored = store.get_session(&child_id).await.unwrap().unwrap();
                    stored.state == SessionState::Stopped
                        && store.model_calls(&child_id).await.unwrap().len() == 2
                }
            }
        })
        .await;
        wait_for("the root to react to the child report and wait", {
            let store = store.clone();
            let root_id = root_id.clone();
            move || {
                let store = store.clone();
                let root_id = root_id.clone();
                async move {
                    let stored = store.get_session(&root_id).await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.model_calls(&root_id).await.unwrap().len() == 2
                }
            }
        })
        .await;

        // The root ran exactly the surface turn and the report-reaction turn:
        // no turn forwarded the answer, and no message_child tool call exists.
        let root_calls = store.model_calls(&root_id).await.unwrap();
        assert_eq!(
            root_calls.len(),
            2,
            "the answer routing spent no root model turn"
        );
        let root_tool_calls = store.tool_calls(&root_id).await.unwrap();
        assert!(
            !root_tool_calls
                .iter()
                .any(|call| call.name == "message_child"),
            "the root never relayed the answer: {root_tool_calls:?}"
        );

        // The answer landed verbatim in the child's thread and the child
        // finished its work.
        let child_messages = store.messages(&child_id, false).await.unwrap();
        let child_texts: Vec<&str> = child_messages
            .iter()
            .filter_map(|(_, message)| match &message.block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            child_texts,
            ["implement the change", answer, "pushed to main"],
            "the answer is the child's next message, verbatim"
        );

        // The surfaced ask records the answer on the root's thread, and the
        // binding is cleared.
        let root_messages = store.messages(&root_id, false).await.unwrap();
        let asked = root_messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::Ask {
                    child_id: Some(child_id),
                    answer,
                    ..
                } if child_id == "child-s6" => Some(answer),
                _ => None,
            })
            .expect("the surfaced ask block exists");
        assert_eq!(
            asked.as_deref(),
            Some(answer),
            "the answer is recorded on the surfaced ask"
        );
        assert!(
            !root_messages.iter().any(|(_, message)| matches!(
                &message.block,
                Block::Text { text } if text == answer
            )),
            "the answer is not addressed to the root's thread"
        );
        assert!(
            store.get_pending_ask(&root_id).await.unwrap().is_none(),
            "the binding is cleared once the ask is answered"
        );
    }

    /// Builds a control-plane state whose root, its child, and that child's
    /// own child each run a loop against their own scripted provider: three
    /// sessions, three models, three fake providers. The leaf's thread starts
    /// with an assignment, so waking the leaf makes it ask. Returns the state,
    /// the store, and the root, mid, and leaf ids.
    async fn state_with_tree(
        dir: &tempfile::TempDir,
        root_scripts: Arc<Mutex<VecDeque<Vec<Value>>>>,
        mid_scripts: Arc<Mutex<VecDeque<Vec<Value>>>>,
        leaf_scripts: Arc<Mutex<VecDeque<Vec<Value>>>>,
    ) -> (Arc<AppState>, Store, String, String, String) {
        let root_addr = scripted_provider(root_scripts).await;
        let mid_addr = scripted_provider(mid_scripts).await;
        let leaf_addr = scripted_provider(leaf_scripts).await;
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let mut root = session("root-s7api");
        root.model = "root-model".into();
        root.persona = None;
        let mut mid = child_session("mid-s7api", "root-s7api");
        mid.model = "mid-model".into();
        mid.persona = None;
        let mut leaf = child_session("leaf-s7api", "mid-s7api");
        leaf.model = "leaf-model".into();
        leaf.persona = None;
        leaf.owner_id = "root-s7api".into();
        store.create_session(&root).await.unwrap();
        store.create_session(&mid).await.unwrap();
        store.create_session(&leaf).await.unwrap();
        store
            .append_message(
                &leaf.id,
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();
        let providers = HashMap::from([
            (
                "root-model".to_string(),
                openai_provider_with_model(root_addr, "root-model"),
            ),
            (
                "mid-model".to_string(),
                openai_provider_with_model(mid_addr, "mid-model"),
            ),
            (
                "leaf-model".to_string(),
                openai_provider_with_model(leaf_addr, "leaf-model"),
            ),
        ]);
        let tunnels = Arc::new(TunnelRegistry::new());
        let loops = Arc::new(AgentRegistry::new(
            None,
            providers.clone(),
            HashMap::new(),
            HashMap::new(),
        ));
        for (id, model) in [
            ("root-s7api", "root-model"),
            ("mid-s7api", "mid-model"),
            ("leaf-s7api", "leaf-model"),
        ] {
            loops.start(
                id,
                store.clone(),
                providers[model].clone(),
                tunnels.clone(),
                model,
            );
        }
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels,
            store: store.clone(),
            loops,
            providers,
            personas: HashMap::new(),
            default_persona: None,
            skills_dir: None,
        });
        (state, store, root.id, mid.id, leaf.id)
    }

    #[tokio::test]
    async fn a_user_answer_at_depth_routes_to_the_grandchild_leaf_without_any_model_relay() {
        let answer = "yes, push to main";
        // The root surfaces the question it read from its child, naming that
        // child; the binding row keeps the child's direct identity for the
        // root to act on and the origin leaf two levels down for the answer.
        // The root's only later turn reacts to the completion reports.
        let root_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                vec![tool_call_fragment(
                    "call-1",
                    "ask",
                    r#"{"message":"may I push?","options":["yes","no"],"child_id":"mid-s7api"}"#,
                )],
                vec![text_chunk("noted the report")],
            ])));
        // The mid-level session re-raises the leaf's question to its parent,
        // then later handles the leaf's completion report.
        let mid_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                vec![tool_call_fragment(
                    "call-1",
                    "ask",
                    r#"{"message":"may I push?","options":["yes","no"],"child_id":"leaf-s7api"}"#,
                )],
                vec![text_chunk("recorded the result")],
            ])));
        let leaf_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                vec![tool_call_fragment(
                    "call-1",
                    "ask",
                    r#"{"message":"may I push?","options":["yes","no"]}"#,
                )],
                vec![text_chunk("pushed to main")],
            ])));
        let dir = tempdir().unwrap();
        let (state, store, root_id, mid_id, leaf_id) =
            state_with_tree(&dir, root_scripts, mid_scripts, leaf_scripts).await;
        // The leaf's assignment is in its thread; waking it makes it ask, and
        // the surface chain climbs to the root on its own.
        state.loops.wake(&leaf_id);
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        wait_for("the chain to surface at the root", {
            let store = store.clone();
            let root_id = root_id.clone();
            move || {
                let store = store.clone();
                let root_id = root_id.clone();
                async move {
                    let stored = store.get_session(&root_id).await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.get_pending_ask(&root_id).await.unwrap().is_some()
                        && store.model_calls(&root_id).await.unwrap().len() == 1
                }
            }
        })
        .await;
        let pending = store
            .get_pending_ask(&root_id)
            .await
            .unwrap()
            .expect("a pending ask");
        assert_eq!(
            pending.child_id, mid_id,
            "the row names the root's direct child, the session the root can message to cancel"
        );
        assert_eq!(
            pending.origin_leaf, leaf_id,
            "the row carries the origin leaf the user's answer routes to"
        );

        // The user answers; the answer routes verbatim to the leaf and no
        // model at any level is spent relaying it.
        let response = client
            .post(format!("http://{addr}/sessions/{root_id}/messages"))
            .json(&json!({ "content": answer }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the leaf to resume and the reports to climb the tree", {
            let store = store.clone();
            let root_id = root_id.clone();
            let mid_id = mid_id.clone();
            let leaf_id = leaf_id.clone();
            move || {
                let store = store.clone();
                let root_id = root_id.clone();
                let mid_id = mid_id.clone();
                let leaf_id = leaf_id.clone();
                async move {
                    let root = store.get_session(&root_id).await.unwrap().unwrap();
                    let mid = store.get_session(&mid_id).await.unwrap().unwrap();
                    let leaf = store.get_session(&leaf_id).await.unwrap().unwrap();
                    root.state == SessionState::WaitingForInput
                        && store.model_calls(&root_id).await.unwrap().len() == 2
                        && mid.state == SessionState::Stopped
                        && store.model_calls(&mid_id).await.unwrap().len() == 2
                        && leaf.state == SessionState::Stopped
                        && store.model_calls(&leaf_id).await.unwrap().len() == 2
                }
            }
        })
        .await;

        // The answer reached the leaf verbatim; the root ran exactly the
        // surface turn and the report-reaction turn, and never messaged a
        // child to relay the answer.
        let leaf_messages = store.messages(&leaf_id, false).await.unwrap();
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
        let root_calls = store.model_calls(&root_id).await.unwrap();
        assert_eq!(
            root_calls.len(),
            2,
            "the answer routing spent no root model turn"
        );
        let root_tool_calls = store.tool_calls(&root_id).await.unwrap();
        assert!(
            !root_tool_calls
                .iter()
                .any(|call| call.name == "message_child"),
            "the root never relayed the answer: {root_tool_calls:?}"
        );
        let mid_tool_calls = store.tool_calls(&mid_id).await.unwrap();
        assert!(
            !mid_tool_calls
                .iter()
                .any(|call| call.name == "message_child"),
            "the mid-level session never relayed the answer: {mid_tool_calls:?}"
        );
        assert!(
            store.get_pending_ask(&root_id).await.unwrap().is_none(),
            "the binding is cleared once the ask is answered"
        );
    }

    #[tokio::test]
    async fn a_redirect_while_a_surfaced_ask_is_pending_wakes_the_root_and_keeps_the_binding() {
        let redirect = "stop the push attempt and review the README instead";
        let root_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![
                vec![tool_call_fragment(
                    "call-1",
                    "ask",
                    r#"{"message":"may I push?","options":["yes","no"],"child_id":"child-s6"}"#,
                )],
                // The redirect wakes the root, which decides to hold the
                // pending ask: it answers with text and does not message the
                // child, so the binding stays for a later answer.
                vec![text_chunk("understood — the question still stands")],
            ])));
        let child_scripts: Arc<Mutex<VecDeque<Vec<Value>>>> =
            Arc::new(Mutex::new(VecDeque::from(vec![vec![tool_call_fragment(
                "call-1",
                "ask",
                r#"{"message":"may I push?","options":["yes","no"]}"#,
            )]])));
        let dir = tempdir().unwrap();
        let (state, store, root_id, child_id) =
            state_with_child_tree(&dir, root_scripts, child_scripts).await;
        // The child's assignment is in its thread; waking it makes it ask.
        state.loops.wake(&child_id);
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        wait_for("the child to ask and the root to surface the bound ask", {
            let store = store.clone();
            let root_id = root_id.clone();
            move || {
                let store = store.clone();
                let root_id = root_id.clone();
                async move {
                    let stored = store.get_session(&root_id).await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.get_pending_ask(&root_id).await.unwrap().is_some()
                        && store.model_calls(&root_id).await.unwrap().len() == 1
                }
            }
        })
        .await;
        let child_messages_before = store.messages(&child_id, false).await.unwrap().len();

        // The user redirects instead of answering: the message is an ordinary
        // root message, so the root wakes to decide the pending ask's fate.
        let response = client
            .post(format!("http://{addr}/sessions/{root_id}/messages"))
            .json(&json!({ "content": redirect, "redirect": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the root's redirect turn to run and end", {
            let store = store.clone();
            let root_id = root_id.clone();
            move || {
                let store = store.clone();
                let root_id = root_id.clone();
                async move {
                    let stored = store.get_session(&root_id).await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                        && store.model_calls(&root_id).await.unwrap().len() == 2
                }
            }
        })
        .await;

        // The redirect reached the root as a normal message...
        let root_messages = store.messages(&root_id, false).await.unwrap();
        assert!(
            root_messages.iter().any(
                |(_, message)| matches!(&message.block, Block::Text { text } if text == redirect)
            ),
            "the redirect text is in the root's thread"
        );
        // ...was never routed to the child...
        let child_messages = store.messages(&child_id, false).await.unwrap();
        assert_eq!(
            child_messages.len(),
            child_messages_before,
            "a redirect is not routed to the child"
        );
        assert!(!child_messages.iter().any(
            |(_, message)| matches!(&message.block, Block::Text { text } if text == redirect)
        ));
        let child = store.get_session(&child_id).await.unwrap().unwrap();
        assert_eq!(
            child.state,
            SessionState::WaitingForInput,
            "the child still waits on its question"
        );
        // ...and the binding stayed pending: the root chose to hold, so the
        // user can still answer the surfaced question.
        let pending = store.get_pending_ask(&root_id).await.unwrap().unwrap();
        assert_eq!(pending.child_id, child_id);
        let surfaced = root_messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::Ask {
                    child_id: Some(child_id),
                    answer,
                    ..
                } if child_id == "child-s6" => Some(answer),
                _ => None,
            })
            .expect("the surfaced ask block exists");
        assert_eq!(surfaced, &None, "a held ask is not answered");
    }

    #[tokio::test]
    async fn a_persona_prompt_reaches_the_loop_as_the_system_prompt() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let systems = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(RecordingProvider::new(systems.clone(), "test"));
        let personas = HashMap::from([(
            "coder".to_string(),
            PersonaConfig {
                model: "test".into(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                description: "Makes changes".into(),
                system_prompt: Some("You are the coder persona.".into()),
            },
        )]);
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: store.clone(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                personas.clone(),
                HashMap::new(),
            )),
            providers: HashMap::from([("test".to_string(), provider)]),
            personas,
            default_persona: Some("coder".into()),
            skills_dir: None,
        });
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "prompt": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session: Value = response.json().await.unwrap();
        assert_eq!(session["persona"], "coder");

        wait_for(
            "the persona prompt to reach the provider's system prompt",
            {
                let systems = systems.clone();
                move || {
                    let systems = systems.clone();
                    async move {
                        let recorded = systems.lock().unwrap();
                        recorded
                            .iter()
                            .any(|system| system.contains("You are the coder persona."))
                    }
                }
            },
        )
        .await;

        let recorded = systems.lock().unwrap();
        let system = recorded.first().expect("a system prompt was recorded");
        assert!(system.contains("You are the coder persona."), "{system}");
        assert!(!system.contains("You are Bosun"), "{system}");
    }

    #[tokio::test]
    async fn a_persona_switch_replaces_the_stored_session_fields_and_records_the_event() {
        let dir = tempdir().unwrap();
        let state = state_with_catalog(
            &dir,
            HashMap::from([
                (
                    "test".to_string(),
                    Arc::new(DummyProvider) as Arc<dyn Provider>,
                ),
                (
                    "cheap".to_string(),
                    Arc::new(DummyProvider) as Arc<dyn Provider>,
                ),
            ]),
            HashMap::from([
                (
                    "coder".to_string(),
                    persona_config("test", Permission::ReadWrite, "*", None),
                ),
                (
                    "reviewer".to_string(),
                    persona_config("cheap", Permission::ReadOnly, "file/read, grep", None),
                ),
            ]),
            Some("coder"),
        );
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();
        store.create_session(&session("s1")).await.unwrap();

        let response = client
            .post(format!("http://{addr}/sessions/s1/persona"))
            .json(&json!({ "persona": "reviewer" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let stored = store.get_session("s1").await.unwrap().unwrap();
        assert_eq!(stored.persona.as_deref(), Some("reviewer"));
        assert_eq!(stored.model, "cheap");
        assert_eq!(stored.permission, Permission::ReadOnly);
        assert_eq!(stored.allowed_tools, "file/read, grep");

        let session: Value = client
            .get(format!("http://{addr}/sessions/s1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(session["persona"], "reviewer");
        assert_eq!(session["model"], "cheap");

        let frames = read_sse_frames(
            client
                .get(format!("http://{addr}/sessions/s1/events?after=0"))
                .send()
                .await
                .unwrap(),
            |frames| {
                let has_persona = frames.iter().any(|frame| {
                    frame.data["event"]["kind"] == "persona"
                        && frame.data["event"]["persona"] == "reviewer"
                });
                let has_permission = frames.iter().any(|frame| {
                    frame.data["event"]["kind"] == "permission"
                        && frame.data["event"]["permission"] == "read_only"
                });
                has_persona && has_permission
            },
        )
        .await;
        let kinds: Vec<&str> = frames
            .iter()
            .filter_map(|frame| frame.data["event"]["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["persona", "permission"]);
    }

    #[tokio::test]
    async fn a_persona_switch_to_an_unknown_persona_is_refused_and_changes_nothing() {
        let dir = tempdir().unwrap();
        let state = state_with_catalog(
            &dir,
            HashMap::from([(
                "test".to_string(),
                Arc::new(DummyProvider) as Arc<dyn Provider>,
            )]),
            HashMap::from([(
                "coder".to_string(),
                persona_config("test", Permission::ReadWrite, "*", None),
            )]),
            Some("coder"),
        );
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();
        store.create_session(&session("s1")).await.unwrap();

        let response = client
            .post(format!("http://{addr}/sessions/s1/persona"))
            .json(&json!({ "persona": "ghost" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("persona ghost is not configured"), "{text}");

        let stored = store.get_session("s1").await.unwrap().unwrap();
        assert_eq!(stored.persona.as_deref(), Some("coder"));
        assert_eq!(stored.model, "test");
        assert_eq!(stored.permission, Permission::ReadWrite);
        assert!(
            store.events_after("s1", 0).await.unwrap().is_empty(),
            "a refused switch records nothing"
        );

        // A missing session 404s even when the persona exists.
        let response = client
            .post(format!("http://{addr}/sessions/ghost/persona"))
            .json(&json!({ "persona": "coder" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_next_model_call_after_a_persona_switch_uses_the_new_persona() {
        let dir = tempdir().unwrap();
        let coder_systems = Arc::new(Mutex::new(Vec::new()));
        let reviewer_systems = Arc::new(Mutex::new(Vec::new()));
        let providers: HashMap<String, Arc<dyn Provider>> = HashMap::from([
            (
                "test".to_string(),
                Arc::new(RecordingProvider::new(coder_systems.clone(), "test"))
                    as Arc<dyn Provider>,
            ),
            (
                "beta".to_string(),
                Arc::new(RecordingProvider::new(reviewer_systems.clone(), "beta"))
                    as Arc<dyn Provider>,
            ),
        ]);
        let personas = HashMap::from([
            (
                "coder".to_string(),
                persona_config(
                    "test",
                    Permission::ReadWrite,
                    "*",
                    Some("You are the coder persona."),
                ),
            ),
            (
                "reviewer".to_string(),
                persona_config(
                    "beta",
                    Permission::ReadOnly,
                    "file/read, grep",
                    Some("You are the reviewer persona."),
                ),
            ),
        ]);
        let state = state_with_catalog(&dir, providers, personas, Some("coder"));
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "prompt": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session: Value = response.json().await.unwrap();
        let id = session["id"].as_str().unwrap().to_string();

        wait_for("the first model call to run under the coder persona", {
            let store = store.clone();
            let id = id.clone();
            move || {
                let store = store.clone();
                let id = id.clone();
                async move {
                    let calls = store.model_calls(&id).await.unwrap();
                    calls.len() == 1 && calls[0].model == "test"
                }
            }
        })
        .await;
        {
            let recorded = coder_systems.lock().unwrap();
            assert!(
                recorded
                    .iter()
                    .any(|system| system.contains("You are the coder persona.")),
                "the coder provider saw its own system prompt"
            );
        }

        let response = client
            .post(format!("http://{addr}/sessions/{id}/persona"))
            .json(&json!({ "persona": "reviewer" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let stored = store.get_session(&id).await.unwrap().unwrap();
        assert_eq!(stored.model, "beta");
        assert_eq!(stored.permission, Permission::ReadOnly);

        let response = client
            .post(format!("http://{addr}/sessions/{id}/messages"))
            .json(&json!({ "content": "continue" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the second model call to run under the reviewer persona", {
            let store = store.clone();
            let id = id.clone();
            move || {
                let store = store.clone();
                let id = id.clone();
                async move {
                    let calls = store.model_calls(&id).await.unwrap();
                    calls.len() == 2 && calls[1].model == "beta"
                }
            }
        })
        .await;
        let recorded = reviewer_systems.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|system| system.contains("You are the reviewer persona.")),
            "the reviewer provider saw its own system prompt"
        );
        assert!(
            !recorded
                .iter()
                .any(|system| system.contains("You are the coder persona.")),
            "the switch replaced the old persona's system prompt"
        );
    }

    #[tokio::test]
    async fn a_persona_switch_mid_turn_does_not_abort_the_running_turn() {
        let dir = tempdir().unwrap();
        let slow_requests = Arc::new(AtomicUsize::new(0));
        let (slow_addr, slow_requests_observed) =
            fake_provider_with_delay_and_requests(Duration::from_millis(500), slow_requests).await;
        let fast_addr = fake_provider().await;
        let providers: HashMap<String, Arc<dyn Provider>> = HashMap::from([
            (
                "test".to_string(),
                openai_provider_with_model(slow_addr, "test"),
            ),
            (
                "beta".to_string(),
                openai_provider_with_model(fast_addr, "beta"),
            ),
        ]);
        let personas = HashMap::from([
            (
                "coder".to_string(),
                persona_config("test", Permission::ReadWrite, "*", None),
            ),
            (
                "reviewer".to_string(),
                persona_config("beta", Permission::ReadOnly, "*", None),
            ),
        ]);
        let state = state_with_catalog(&dir, providers, personas, Some("coder"));
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "prompt": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session: Value = response.json().await.unwrap();
        let id = session["id"].as_str().unwrap().to_string();

        // The slow provider holds its first delta, so the request is still in
        // flight when the switch lands.
        wait_for("the slow turn's request to reach the provider", || {
            let observed = slow_requests_observed.clone();
            async move { observed.load(Ordering::Relaxed) >= 1 }
        })
        .await;

        let response = client
            .post(format!("http://{addr}/sessions/{id}/persona"))
            .json(&json!({ "persona": "reviewer" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // The running turn is not aborted: it finishes under the model it
        // started with and the session reaches waiting_for_input.
        wait_for("the in-flight turn to finish", {
            let store = store.clone();
            let id = id.clone();
            move || {
                let store = store.clone();
                let id = id.clone();
                async move {
                    let stored = store.get_session(&id).await.unwrap().unwrap();
                    stored.state == SessionState::WaitingForInput
                }
            }
        })
        .await;
        let calls = store.model_calls(&id).await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].model, "test",
            "the running turn keeps the model it started under"
        );

        let response = client
            .post(format!("http://{addr}/sessions/{id}/messages"))
            .json(&json!({ "content": "continue" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        wait_for("the next turn to run under the switched model", {
            let store = store.clone();
            let id = id.clone();
            move || {
                let store = store.clone();
                let id = id.clone();
                async move {
                    let calls = store.model_calls(&id).await.unwrap();
                    calls.len() == 2 && calls[1].model == "beta"
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_persona_switch_toggles_the_executor_permission_only_when_it_differs() {
        // A stub executor whose /permission handler records every body it
        // receives, reached through a registered tunnel like a node's.
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = {
            use axum::extract::State as AxumState;
            use axum::routing::post as axum_post;

            async fn handle_permission(
                AxumState(seen): AxumState<Arc<Mutex<Vec<String>>>>,
                body: axum::body::Bytes,
            ) -> Json<serde_json::Value> {
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
                Json(json!({}))
            }
            axum::Router::new()
                .route("/permission", axum_post(handle_permission))
                .with_state(seen.clone())
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _) = Tunnel::new(cp_side);
        let (node_tunnel, mut opens) = Tunnel::new(node_side);
        let tunnels = Arc::new(TunnelRegistry::new());
        tunnels.register("s1", cp_tunnel);
        tokio::spawn(async move {
            while let Some(event) = opens.recv().await {
                let tunnel = node_tunnel.clone();
                tokio::spawn(async move {
                    let mut backend = TcpStream::connect(backend).await.unwrap();
                    let mut logical = tunnel.attach(event.conn_id, event.rx).unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut backend, &mut logical).await;
                });
            }
        });

        let dir = tempdir().unwrap();
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels,
            store: Store::open(&dir.path().join("sessions.db")).unwrap(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )),
            providers: HashMap::from([(
                "m".to_string(),
                Arc::new(DummyProvider) as Arc<dyn Provider>,
            )]),
            personas: HashMap::from([
                (
                    "coder".to_string(),
                    persona_config("m", Permission::ReadWrite, "*", None),
                ),
                (
                    "reviewer".to_string(),
                    persona_config("m", Permission::ReadOnly, "*", None),
                ),
                (
                    "architect".to_string(),
                    persona_config("m", Permission::ReadWrite, "*", None),
                ),
            ]),
            default_persona: Some("coder".into()),
            skills_dir: None,
        });
        let store = state.store.clone();
        state.store.create_session(&session("s1")).await.unwrap();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        // coder (read_write) -> reviewer (read_only): the executor toggles.
        let response = client
            .post(format!("http://{addr}/sessions/s1/persona"))
            .json(&json!({ "persona": "reviewer" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            *seen.lock().unwrap(),
            [r#"{"permission":"read_only"}"#],
            "the executor receives the new read-only permission"
        );

        // reviewer (read_only) -> architect (read_write): the executor toggles.
        let response = client
            .post(format!("http://{addr}/sessions/s1/persona"))
            .json(&json!({ "persona": "architect" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            store
                .get_session("s1")
                .await
                .unwrap()
                .unwrap()
                .persona
                .as_deref(),
            Some("architect")
        );
        {
            let bodies = seen.lock().unwrap();
            assert_eq!(
                *bodies,
                [
                    r#"{"permission":"read_only"}"#,
                    r#"{"permission":"read_write"}"#
                ]
            );
        }

        // architect (read_write) -> coder (read_write): the permission does
        // not differ, so the executor is not touched again.
        let response = client
            .post(format!("http://{addr}/sessions/s1/persona"))
            .json(&json!({ "persona": "coder" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "a same-permission switch never reaches the executor"
        );
        assert_eq!(
            store
                .get_session("s1")
                .await
                .unwrap()
                .unwrap()
                .persona
                .as_deref(),
            Some("coder")
        );
    }

    #[tokio::test]
    async fn events_stream_replays_after_a_sequence() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        store.create_session(&session("s1")).await.unwrap();
        store
            .append_message(
                "s1",
                Role::User,
                &Block::Text {
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        store
            .set_state("s1", SessionState::WaitingForInput)
            .await
            .unwrap();

        let frames = read_sse_frames(
            client
                .get(format!("http://{addr}/sessions/s1/events"))
                .send()
                .await
                .unwrap(),
            |frames| frames.len() >= 2,
        )
        .await;
        let seqs: Vec<i64> = frames
            .iter()
            .map(|frame| frame.data["seq"].as_i64().unwrap())
            .collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(frames[0].id.as_deref(), Some("1"), "the sse id is the seq");
        assert_eq!(frames[1].id.as_deref(), Some("2"), "the sse id is the seq");
        assert_eq!(frames[0].data["event"]["kind"], "message");
        assert_eq!(frames[1].data["event"]["kind"], "state");
        assert_eq!(frames[1].data["event"]["state"], "waiting_for_input");

        let frames = read_sse_frames(
            client
                .get(format!("http://{addr}/sessions/s1/events?after=1"))
                .send()
                .await
                .unwrap(),
            |frames| !frames.is_empty(),
        )
        .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data["seq"], 2);

        // A cursor past the last event replays nothing; the stream stays open
        // polling for new durable events, so no frame arrives.
        let frames = read_sse_frames_for(
            client
                .get(format!("http://{addr}/sessions/s1/events?after=2"))
                .send()
                .await
                .unwrap(),
            Duration::from_secs(2),
        )
        .await;
        assert!(frames.is_empty());
    }

    #[tokio::test]
    async fn events_stream_delivers_the_terminal_state_live() {
        let provider_addr = fake_provider_with_delay(Duration::from_millis(800)).await;
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
            store: store.clone(),
            loops: Arc::new(AgentRegistry::new(
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )),
            providers: HashMap::from([("test".to_string(), openai_provider(provider_addr))]),
            personas: HashMap::from([(
                "test".to_string(),
                PersonaConfig {
                    model: "test".into(),
                    permission: Permission::ReadWrite,
                    allowed_tools: "*".into(),
                    description: String::new(),
                    system_prompt: None,
                },
            )]),
            default_persona: Some("test".into()),
            skills_dir: None,
        });
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/sessions"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "persona": "test",
                "prompt": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let id = response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Connect while the first turn is still in flight: the terminal state
        // can then only arrive live, not from replay.
        wait_for("the turn to start running", || {
            let store = store.clone();
            let id = id.clone();
            async move {
                let stored = store.get_session(&id).await.unwrap().unwrap();
                stored.state == SessionState::Running
            }
        })
        .await;

        let frames = read_sse_frames(
            client
                .get(format!("http://{addr}/sessions/{id}/events?after=0"))
                .send()
                .await
                .unwrap(),
            |frames| {
                frames.iter().any(|frame| {
                    frame.data["event"]["kind"] == "state"
                        && frame.data["event"]["state"] == "waiting_for_input"
                })
            },
        )
        .await;
        let state_frame = frames
            .iter()
            .find(|frame| {
                frame.data["event"]["kind"] == "state"
                    && frame.data["event"]["state"] == "waiting_for_input"
            })
            .expect("the terminal state event arrives on the open stream");
        let seq = state_frame.data["seq"].as_i64().expect("a seq");
        assert_eq!(
            state_frame.id.as_deref(),
            Some(seq.to_string().as_str()),
            "the sse id matches the seq"
        );
    }

    #[tokio::test]
    async fn events_stream_resumes_from_last_event_id() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        store.create_session(&session("s1")).await.unwrap();
        store
            .append_message(
                "s1",
                Role::User,
                &Block::Text {
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        store
            .set_state("s1", SessionState::WaitingForInput)
            .await
            .unwrap();

        // EventSource reconnects with Last-Event-ID instead of `after=`, so
        // the stream must resume past the replayed seq.
        let frames = read_sse_frames(
            client
                .get(format!("http://{addr}/sessions/s1/events"))
                .header("last-event-id", "1")
                .send()
                .await
                .unwrap(),
            |frames| !frames.is_empty(),
        )
        .await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data["seq"], 2);
        assert_eq!(frames[0].data["event"]["kind"], "state");
    }

    #[tokio::test]
    async fn clone_with_an_unconfigured_persona_rejects_before_the_node_sees_a_command() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .registry
            .upsert("n1", "0.5.5", UpdateStatus::default(), SystemTime::now());
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/clone"))
            .json(&json!({
                "node": "n1",
                "repo_url": "https://example.com/repo",
                "persona": "ghost"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("persona ghost is not configured"), "{text}");
        assert!(
            !state.commands.pending("n1"),
            "the node must never receive a command"
        );
        assert!(state.store.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn dev_with_an_unconfigured_persona_rejects_before_the_node_sees_a_command() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .registry
            .upsert("n1", "0.5.5", UpdateStatus::default(), SystemTime::now());
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/dev"))
            .json(&json!({
                "node": "n1",
                "dir": "/tmp/x",
                "persona": "ghost"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("persona ghost is not configured"), "{text}");
        assert!(
            !state.commands.pending("n1"),
            "the node must never receive a command"
        );
        assert!(state.store.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clone_without_any_persona_rejects_before_the_node_sees_a_command() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .registry
            .upsert("n1", "0.5.5", UpdateStatus::default(), SystemTime::now());
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/clone"))
            .json(&json!({
                "node": "n1",
                "repo_url": "https://example.com/repo"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("no persona configured"), "{text}");
        assert!(
            !state.commands.pending("n1"),
            "the node must never receive a command"
        );
        assert!(state.store.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn tunnel_accepts_a_session_the_store_does_not_know() {
        let dir = tempdir().unwrap();
        let addr = serve(test_state(&dir)).await;

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            http1::handshake::<_, http_body_util::Empty<bytes::Bytes>>(TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let request = HttpRequest::builder()
            .method("GET")
            .uri("/tunnel/session/ghost")
            .header(header::HOST, addr.to_string())
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "bosun-tunnel")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SWITCHING_PROTOCOLS,
            "the tunnel must not depend on the store: a node's tunnel can arrive before the control plane records the clone"
        );
    }

    #[tokio::test]
    async fn session_404s() {
        let dir = tempdir().unwrap();
        let addr = serve(test_state(&dir)).await;
        let client = reqwest::Client::new();

        let response = client
            .get(format!("http://{addr}/sessions/missing"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = client
            .post(format!("http://{addr}/sessions/missing/messages"))
            .json(&json!({ "content": "hi" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn model_calls_endpoint_returns_totals_and_rows() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        store.create_session(&session("s1")).await.unwrap();
        store
            .append_model_call(
                "s1",
                "claude",
                "anthropic",
                "completion",
                Some(100),
                Some(50),
                Some(0.125),
            )
            .await
            .unwrap();
        store
            .append_model_call(
                "s1",
                "claude",
                "anthropic",
                "completion",
                Some(200),
                Some(10),
                None,
            )
            .await
            .unwrap();
        store
            .append_model_call(
                "s1",
                "claude",
                "anthropic",
                "compaction",
                Some(1000),
                None,
                Some(0.25),
            )
            .await
            .unwrap();

        let response = client
            .get(format!("http://{addr}/sessions/s1/model-calls"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let summary: Value = response.json().await.unwrap();

        let calls = summary["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[2]["kind"], "compaction",
            "rows come oldest first, newest last"
        );
        assert_eq!(calls[0]["cost"], 0.125, "the row carries its recorded cost");
        assert_eq!(
            calls[2]["cost"], 0.25,
            "the newest call's cost is recorded on its row"
        );
        assert_eq!(summary["total_input_tokens"], 1300);
        assert_eq!(summary["total_output_tokens"], 60);
        assert_eq!(summary["total_cost"], 0.375);
        assert_eq!(summary["completion_calls"], 2);
        assert_eq!(summary["compaction_calls"], 1);

        let response = client
            .get(format!("http://{addr}/sessions/missing/model-calls"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_removes_the_session() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state).await;
        let client = reqwest::Client::new();

        store.create_session(&session("s1")).await.unwrap();

        let response = client
            .post(format!("http://{addr}/stop"))
            .json(&json!({ "session_id": "s1" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let sessions: Value = client
            .get(format!("http://{addr}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sessions.as_array().unwrap().len(), 0);

        let response = client
            .post(format!("http://{addr}/stop"))
            .json(&json!({ "session_id": "s1" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn stop_with_the_node_down_still_reaches_the_node_on_its_next_poll() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.store.clone();
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        store.create_session(&session("s1")).await.unwrap();

        let response = client
            .post(format!("http://{addr}/stop"))
            .json(&json!({ "session_id": "s1" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            state.commands.pending("n1"),
            "the stop command must be waiting for the node's next poll"
        );
        assert!(store.get_session("s1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn node_update_enqueues_an_update_command_with_the_cp_version() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .registry
            .upsert("n1", "0.5.5", UpdateStatus::default(), SystemTime::now());
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/nodes/n1/update"))
            .json(&json!({ "force": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let command = state
            .commands
            .next("n1")
            .await
            .expect("the update command must be queued");
        let NodeCommand::Update { version, force, .. } = command else {
            panic!("the queued command must be an update");
        };
        assert_eq!(version, bosun_common::version::VERSION);
        assert!(force);
    }

    #[tokio::test]
    async fn node_update_is_fire_and_forget_and_keeps_no_pending_reply() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .registry
            .upsert("n1", "0.5.5", UpdateStatus::default(), SystemTime::now());
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/nodes/n1/update"))
            .json(&json!({ "force": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            state.commands.pending("n1"),
            "the update command must be waiting for the node's next poll"
        );

        let command = state
            .commands
            .next("n1")
            .await
            .expect("the update command must be queued");
        assert!(matches!(command, NodeCommand::Update { .. }));
        assert!(
            !state.commands.pending("n1"),
            "a fire-and-forget update must leave no reply channel behind"
        );
    }

    #[tokio::test]
    async fn node_update_defaults_force_to_false() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        state
            .registry
            .upsert("n1", "0.5.5", UpdateStatus::default(), SystemTime::now());
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/nodes/n1/update"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let command = state
            .commands
            .next("n1")
            .await
            .expect("the update command must be queued");
        let NodeCommand::Update { force, .. } = command else {
            panic!("the queued command must be an update");
        };
        assert!(!force);
    }

    #[tokio::test]
    async fn node_update_refuses_a_node_that_predates_the_version_handshake() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        state
            .registry
            .upsert("n1", "", UpdateStatus::default(), SystemTime::now());
        let response = client
            .post(format!("http://{addr}/nodes/n1/update"))
            .json(&json!({ "force": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("predates auto-update"), "{text}");
        assert!(
            !state.commands.pending("n1"),
            "a node without a version must never receive an update command"
        );

        state.registry.upsert(
            "n2",
            "not-a-version",
            UpdateStatus::default(),
            SystemTime::now(),
        );
        let response = client
            .post(format!("http://{addr}/nodes/n2/update"))
            .json(&json!({ "force": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            !state.commands.pending("n2"),
            "a node with an unparsable version must never receive an update command"
        );
    }

    #[tokio::test]
    async fn node_update_rejects_an_unknown_node_without_queueing() {
        let dir = tempdir().unwrap();
        let state = test_state(&dir);
        let addr = serve(state.clone()).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/nodes/ghost/update"))
            .json(&json!({ "force": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("ghost is not up"), "{text}");
        assert!(!state.commands.pending("ghost"));
    }
}
