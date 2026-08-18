use std::sync::Arc;
use std::time::SystemTime;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use axum::routing::post;
use bosun_common::types::Heartbeat;
use tracing::info;

use crate::registry::NodeHealth;
use crate::registry::NodeRegistry;

pub fn router(registry: Arc<NodeRegistry>) -> Router {
    Router::new()
        .route("/heartbeat", post(heartbeat))
        .route("/nodes", get(nodes))
        .with_state(registry)
}

async fn heartbeat(
    State(registry): State<Arc<NodeRegistry>>,
    Json(heartbeat): Json<Heartbeat>,
) -> axum::http::StatusCode {
    registry.upsert(&heartbeat.node_name, SystemTime::now());
    info!(node = %heartbeat.node_name, "node heartbeated");
    axum::http::StatusCode::NO_CONTENT
}

async fn nodes(State(registry): State<Arc<NodeRegistry>>) -> Json<Vec<NodeHealth>> {
    Json(registry.list(SystemTime::now()))
}
