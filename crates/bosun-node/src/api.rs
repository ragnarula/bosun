use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use bosun_common::error::ErrorExt;
use bosun_common::types::NodeSpawnRequest;
use bosun_common::types::SessionInfo;
use bosun_common::types::StopRequest;
use thiserror::Error;
use tracing::info;
use tracing::instrument;

use crate::manager::NodeError;
use crate::manager::NodeManager;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("failed to spawn session: {0}")]
    Spawn(NodeError),

    #[error("failed to stop session: {0}")]
    Stop(NodeError),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, text) = match &self {
            ApiError::Spawn(NodeError::CloneFailed { .. }) => {
                (StatusCode::BAD_REQUEST, Some(self.to_string()))
            }
            ApiError::Spawn(NodeError::HealthTimeout { .. }) => {
                (StatusCode::INTERNAL_SERVER_ERROR, Some(self.to_string()))
            }
            ApiError::Spawn(_) | ApiError::Stop(_) | ApiError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, None)
            }
        };

        tracing::error!("error: {}", self.display_chain());

        match text {
            Some(text) => (status, text).into_response(),
            None => status.into_response(),
        }
    }
}

pub fn router(manager: Arc<NodeManager>) -> Router {
    Router::new()
        .route("/spawn", post(spawn))
        .route("/stop", post(stop))
        .with_state(manager)
}

#[instrument(skip_all)]
async fn spawn(
    State(manager): State<Arc<NodeManager>>,
    Json(req): Json<NodeSpawnRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let record = manager.spawn(&req).await.map_err(ApiError::Spawn)?;
    info!(session_id = %record.id, "session spawned");
    Ok(Json(SessionInfo {
        id: record.id,
        repo_url: record.repo_url,
        git_ref: record.git_ref,
        status: record.status,
        opencode_port: record.opencode_port,
        forwarder_addr: record.forwarder_addr,
    }))
}

#[instrument(skip_all)]
async fn stop(
    State(manager): State<Arc<NodeManager>>,
    Json(req): Json<StopRequest>,
) -> Result<StatusCode, ApiError> {
    manager
        .stop(&req.session_id)
        .await
        .map_err(ApiError::Stop)?;
    Ok(StatusCode::NO_CONTENT)
}
