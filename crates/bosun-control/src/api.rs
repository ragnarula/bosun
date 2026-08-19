use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bosun_common::error::ErrorExt;
use bosun_common::types::Heartbeat;
use bosun_common::types::NodeSpawnRequest;
use bosun_common::types::SessionInfo;
use bosun_common::types::SpawnRequest;
use bosun_common::types::StopRequest;
use thiserror::Error;
use tracing::debug;
use tracing::info;
use tracing::instrument;

use crate::proxy::ProxyManager;
use crate::registry::NodeHealth;
use crate::registry::NodeRegistry;
use crate::registry::SessionHealth;

const SPAWN_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("node {node} is not up")]
    NodeNotUp { node: String },

    #[error("node {node} rejected the request: {detail}")]
    NodeRejected { node: String, detail: String },

    #[error("node {node} is unreachable")]
    NodeUnreachable { node: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, text) = match &self {
            ApiError::NodeNotUp { .. } => (StatusCode::BAD_REQUEST, Some(self.to_string())),
            ApiError::NodeRejected { .. } | ApiError::NodeUnreachable { .. } => {
                (StatusCode::BAD_GATEWAY, Some(self.to_string()))
            }
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
    pub client: reqwest::Client,
    pub template_path: PathBuf,
    pub proxies: Arc<ProxyManager>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/heartbeat", post(heartbeat))
        .route("/nodes", get(nodes))
        .route("/sessions", get(sessions))
        .route("/spawn", post(spawn))
        .route("/stop", post(stop))
        .with_state(state)
}

#[instrument(skip(state))]
async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Json(heartbeat): Json<Heartbeat>,
) -> StatusCode {
    state.registry.upsert(&heartbeat, SystemTime::now());
    for session in &heartbeat.sessions {
        let Some(forwarder_addr) = &session.forwarder_addr else {
            continue;
        };
        if let Err(error) = state.proxies.ensure(&session.id, forwarder_addr).await {
            debug!(
                session_id = %session.id,
                error = %error.display_chain(),
                "failed to start proxy for session"
            );
        }
    }
    info!(
        node = %heartbeat.node_name,
        session_count = heartbeat.sessions.len(),
        "node heartbeated"
    );
    StatusCode::NO_CONTENT
}

#[instrument(skip(state))]
async fn nodes(State(state): State<Arc<AppState>>) -> Json<Vec<NodeHealth>> {
    Json(state.registry.list(SystemTime::now()))
}

#[instrument(skip(state))]
async fn sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionHealth>> {
    let mut sessions = state.registry.sessions(SystemTime::now());
    for session in &mut sessions {
        session.proxy_port = state.proxies.port(&session.id);
    }
    Json(sessions)
}

#[instrument(skip(state))]
async fn spawn(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<SessionHealth>, ApiError> {
    let Some(health) = state.registry.node(&req.node, SystemTime::now()) else {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    };

    let template = tokio::fs::read_to_string(&state.template_path)
        .await
        .with_context(|| format!("failed to read template {}", state.template_path.display()))?;

    let session_id = uuid::Uuid::new_v4().to_string();
    let request = NodeSpawnRequest {
        session_id: session_id.clone(),
        repo_url: req.repo_url.clone(),
        git_ref: req.git_ref.clone(),
        opencode_config: template,
    };

    let url = format!("http://{}/spawn", health.control_addr);
    let response = match state
        .client
        .post(&url)
        .json(&request)
        .timeout(Duration::from_secs(SPAWN_TIMEOUT_SECS))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return Err(ApiError::NodeUnreachable {
                node: req.node.clone(),
            });
        }
    };

    if !response.status().is_success() {
        let detail = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("node returned no readable error"));
        return Err(ApiError::NodeRejected {
            node: req.node.clone(),
            detail,
        });
    }

    let session: SessionInfo = response
        .json()
        .await
        .with_context(|| format!("failed to parse spawn response from node {}", req.node))?;
    let Some(forwarder_addr) = session.forwarder_addr.clone() else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "session {} reported no forwarder address",
            session.id
        )));
    };
    let proxy_port = state.proxies.ensure(&session.id, &forwarder_addr).await?;
    state.registry.add_session(&req.node, session.clone());
    info!(session_id = %session.id, node = %req.node, "session spawned");

    Ok(Json(SessionHealth {
        id: session.id,
        node: req.node,
        repo_url: session.repo_url,
        git_ref: session.git_ref,
        status: session.status,
        proxy_port: Some(proxy_port),
    }))
}

#[instrument(skip(state))]
async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now();
    let Some((node, _)) = state.registry.session(&req.session_id, now) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let Some(node_health) = state.registry.node(&node, now) else {
        return Ok(StatusCode::NO_CONTENT);
    };

    let url = format!("http://{}/stop", node_health.control_addr);
    let response = match state.client.post(&url).json(&req).send().await {
        Ok(response) => response,
        Err(_) => {
            return Err(ApiError::NodeUnreachable { node });
        }
    };

    if !response.status().is_success() {
        let detail = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("node returned no readable error"));
        return Err(ApiError::NodeRejected { node, detail });
    }

    state.proxies.remove(&req.session_id);
    state.registry.remove_session(&node, &req.session_id);
    info!(session_id = %req.session_id, node = %node, "session stopped");
    Ok(StatusCode::NO_CONTENT)
}
