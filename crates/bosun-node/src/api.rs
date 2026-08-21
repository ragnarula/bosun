use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bosun_common::error::ErrorExt;
use bosun_common::types::DirListing;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeDevRequest;
use bosun_common::types::SessionInfo;
use bosun_common::types::StopRequest;
use serde::Deserialize;
use thiserror::Error;
use tracing::info;
use tracing::instrument;

use crate::manager::NodeError;
use crate::manager::NodeManager;
use crate::manager::SessionRecord;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("failed to clone session: {0}")]
    Clone(NodeError),

    #[error("failed to start dev session: {0}")]
    Dev(NodeError),

    #[error("failed to list directory: {0}")]
    Dirs(NodeError),

    #[error("failed to stop session: {0}")]
    Stop(NodeError),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

fn status_for(error: &NodeError) -> StatusCode {
    match error {
        NodeError::CloneFailed { .. }
        | NodeError::NoBrowseRoots
        | NodeError::NotADirectory { .. }
        | NodeError::OutsideRoot { .. } => StatusCode::BAD_REQUEST,
        NodeError::DirNotFound { .. } => StatusCode::NOT_FOUND,
        NodeError::HealthTimeout { .. } | NodeError::Internal(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Clone(err)
            | ApiError::Dev(err)
            | ApiError::Dirs(err)
            | ApiError::Stop(err) => status_for(err),
        };

        let text = match &self {
            ApiError::Internal(_) => None,
            _ => Some(self.to_string()),
        };

        tracing::error!("error: {}", self.display_chain());

        match text {
            Some(text) => (status, text).into_response(),
            None => status.into_response(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct DirsQuery {
    path: Option<PathBuf>,
}

pub fn router(manager: Arc<NodeManager>) -> Router {
    Router::new()
        .route("/clone", post(clone))
        .route("/dev", post(dev))
        .route("/dirs", get(dirs))
        .route("/stop", post(stop))
        .with_state(manager)
}

#[instrument(skip_all)]
async fn clone(
    State(manager): State<Arc<NodeManager>>,
    Json(req): Json<NodeCloneRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let record = manager.run_clone(&req).await.map_err(ApiError::Clone)?;
    info!(session_id = %record.id, "session cloned");
    Ok(Json(session_info(record)))
}

#[instrument(skip_all)]
async fn dev(
    State(manager): State<Arc<NodeManager>>,
    Json(req): Json<NodeDevRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let record = manager.dev(&req).await.map_err(ApiError::Dev)?;
    Ok(Json(session_info(record)))
}

#[instrument(skip_all)]
async fn dirs(
    State(manager): State<Arc<NodeManager>>,
    Query(query): Query<DirsQuery>,
) -> Result<Json<DirListing>, ApiError> {
    let listing = manager
        .list_dir(query.path.as_deref())
        .map_err(ApiError::Dirs)?;
    Ok(Json(listing))
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

fn session_info(record: SessionRecord) -> SessionInfo {
    SessionInfo {
        id: record.id,
        repo_url: record.repo_url,
        git_ref: record.git_ref,
        dir: if record.reapable {
            None
        } else {
            Some(record.dir)
        },
        status: record.status,
        opencode_port: record.opencode_port,
        forwarder_addr: record.forwarder_addr,
    }
}
