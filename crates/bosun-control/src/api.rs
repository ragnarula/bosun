use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bosun_common::error::ErrorExt;
use bosun_common::types::CloneRequest;
use bosun_common::types::DevRequest;
use bosun_common::types::DirListing;
use bosun_common::types::Heartbeat;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeDevRequest;
use bosun_common::types::SessionInfo;
use bosun_common::types::StopRequest;
use serde::Deserialize;
use thiserror::Error;
use tracing::info;
use tracing::instrument;

use crate::gateway::Gateway;
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
    pub gateway: Arc<Gateway>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/heartbeat", post(heartbeat))
        .route("/nodes", get(nodes))
        .route("/sessions", get(sessions))
        .route("/clone", post(clone))
        .route("/dev", post(dev))
        .route("/nodes/{name}/dirs", get(dirs))
        .route("/stop", post(stop))
        .fallback(crate::gateway::route)
        .with_state(state)
}

#[instrument(skip(state))]
async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Json(heartbeat): Json<Heartbeat>,
) -> StatusCode {
    state.registry.upsert(&heartbeat, SystemTime::now());
    for session in &heartbeat.sessions {
        if let Some(forwarder_addr) = &session.forwarder_addr {
            state.gateway.ensure(&session.id, forwarder_addr);
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
    Json(state.registry.sessions(SystemTime::now()))
}

#[instrument(skip(state))]
async fn clone(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CloneRequest>,
) -> Result<Json<SessionHealth>, ApiError> {
    let Some(health) = state.registry.node(&req.node, SystemTime::now()) else {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    let request = NodeCloneRequest {
        session_id: session_id.clone(),
        repo_url: req.repo_url.clone(),
        git_ref: req.git_ref.clone(),
    };

    let url = format!("http://{}/clone", health.control_addr);
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
        .with_context(|| format!("failed to parse clone response from node {}", req.node))?;
    register_session(&state, &req.node, session.clone())?;
    info!(session_id = %session.id, node = %req.node, "session cloned");

    Ok(Json(to_health(req.node, session)))
}

#[instrument(skip(state))]
async fn dev(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DevRequest>,
) -> Result<Json<SessionHealth>, ApiError> {
    let Some(health) = state.registry.node(&req.node, SystemTime::now()) else {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    let request = NodeDevRequest {
        session_id: session_id.clone(),
        dir: req.dir.clone(),
    };

    let url = format!("http://{}/dev", health.control_addr);
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
        .with_context(|| format!("failed to parse dev response from node {}", req.node))?;
    register_session(&state, &req.node, session.clone())?;
    info!(session_id = %session.id, node = %req.node, dir = %req.dir.display(), "dev session started");

    Ok(Json(to_health(req.node, session)))
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
    let Some(health) = state.registry.node(&node, SystemTime::now()) else {
        return Err(ApiError::NodeNotUp { node });
    };

    let mut request = state
        .client
        .get(format!("http://{}/dirs", health.control_addr));
    if let Some(path) = &query.path {
        request = request.query(&[("path", path.to_string_lossy().to_string())]);
    }
    let response = match request.send().await {
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

    let listing: DirListing = response
        .json()
        .await
        .with_context(|| format!("failed to parse dirs response from node {node}"))?;
    Ok(Json(listing))
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

    state.gateway.remove(&req.session_id);
    state.registry.remove_session(&node, &req.session_id);
    info!(session_id = %req.session_id, node = %node, "session stopped");
    Ok(StatusCode::NO_CONTENT)
}

fn register_session(state: &AppState, node: &str, session: SessionInfo) -> Result<(), ApiError> {
    let Some(forwarder_addr) = session.forwarder_addr.clone() else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "session {} reported no forwarder address",
            session.id
        )));
    };
    state.gateway.ensure(&session.id, &forwarder_addr);
    state.registry.add_session(node, session);
    Ok(())
}

fn to_health(node: String, session: SessionInfo) -> SessionHealth {
    SessionHealth {
        id: session.id,
        node,
        repo_url: session.repo_url,
        git_ref: session.git_ref,
        dir: session.dir,
        status: session.status,
    }
}
