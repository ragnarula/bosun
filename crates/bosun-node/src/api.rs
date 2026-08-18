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
use thiserror::Error;
use tracing::info;
use tracing::instrument;

use crate::manager::NodeError;
use crate::manager::NodeManager;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("failed to spawn session: {0}")]
    Spawn(NodeError),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, text) = match &self {
            ApiError::Spawn(NodeError::CloneFailed { .. }) => {
                (StatusCode::BAD_REQUEST, Some(self.to_string()))
            }
            ApiError::Spawn(_) | ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
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
        .with_state(manager)
}

#[instrument(skip_all)]
async fn spawn(
    State(manager): State<Arc<NodeManager>>,
    Json(req): Json<NodeSpawnRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let record = manager.spawn(&req).await.map_err(ApiError::Spawn)?;
    info!(session_id = %record.id, "session cloned");
    Ok(Json(SessionInfo {
        id: record.id,
        repo_url: record.repo_url,
        git_ref: record.git_ref,
        status: record.status,
    }))
}
