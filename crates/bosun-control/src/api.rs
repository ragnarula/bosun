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
use bosun_agent::provider::Provider;
use bosun_common::config::PersonaConfig;
use bosun_common::error::ErrorExt;
use bosun_common::session::Block;
use bosun_common::session::Event;
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
                .set_state(&session.id, SessionState::Interrupted)
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
    let session = Session {
        id: uuid::Uuid::new_v4().to_string(),
        node: req.node,
        repo_url: req.repo_url,
        git_ref: req.git_ref,
        dir: req.dir,
        model: config.model.clone(),
        persona: Some(persona),
        permission: config.permission,
        allowed_tools: config.allowed_tools.clone(),
        state: SessionState::Creating,
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
        id: session_id,
        node: req.node.clone(),
        repo_url: node_session.repo_url.clone(),
        git_ref: node_session.git_ref.clone(),
        dir,
        model: config.model.clone(),
        persona: Some(persona),
        permission: config.permission,
        allowed_tools: config.allowed_tools.clone(),
        state: SessionState::Creating,
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
        id: session_id,
        node: req.node.clone(),
        repo_url: node_session.repo_url.clone(),
        git_ref: node_session.git_ref.clone(),
        dir,
        model: config.model.clone(),
        persona: Some(persona),
        permission: config.permission,
        allowed_tools: config.allowed_tools.clone(),
        state: SessionState::Creating,
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

#[instrument(skip(state))]
async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<StatusCode, ApiError> {
    let Some(session) = state.store.get_session(&req.session_id).await? else {
        return Ok(StatusCode::NO_CONTENT);
    };

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
}

#[instrument(skip(state))]
async fn add_message(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<AddMessageRequest>,
) -> Result<StatusCode, ApiError> {
    if state.store.get_session(&id).await?.is_none() {
        return Err(ApiError::SessionNotFound { id });
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
    if state.store.get_session(&id).await?.is_none() {
        return Err(ApiError::SessionNotFound { id });
    }
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
    if state.store.get_session(&id).await?.is_none() {
        return Err(ApiError::SessionNotFound { id: id.clone() });
    }
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
    use std::net::SocketAddr;
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
    use bosun_common::types::UpdateStatus;
    use bosun_test_support::stub_backend;
    use bosun_test_support::wait_for;
    use futures_util::stream::BoxStream;
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    use serde_json::Value;
    use std::sync::Mutex;
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
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            state: SessionState::Creating,
            created_at_secs: 1_700_000_000,
            prompt: None,
        }
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
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                state: SessionState::WaitingForInput,
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
