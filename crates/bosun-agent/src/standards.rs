//! Repo-standards discovery: the loop asks the node's executor which of
//! `AGENTS.md` and `CLAUDE.md` the working copy holds at its root, so the
//! system prompt can name them. The contents stay on the node; the model
//! reads them on demand with the file tools.

use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent_loop::ToolExecutor;

/// The executor answers the presence call on the node; a hung node must not
/// stall a turn, so the round trip is bounded.
const REPO_STANDARDS_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// The first turn can race the node's tunnel registration, so a transient
/// failure is retried briefly before giving up, like the skills fetch.
const REPO_STANDARDS_FETCH_ATTEMPTS: usize = 4;

/// Fetches the repo-standard files present at the working copy's root from
/// the node's executor. Ok with an empty list when the working copy holds
/// neither file.
pub async fn fetch_repo_standards(
    tools: &dyn ToolExecutor,
    session_id: &str,
) -> Result<Vec<String>, anyhow::Error> {
    let mut last_error = None;
    for _ in 0..REPO_STANDARDS_FETCH_ATTEMPTS {
        match fetch_repo_standards_once(tools, session_id).await {
            Ok(present) => return Ok(present),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to fetch repo standards")))
}

async fn fetch_repo_standards_once(
    tools: &dyn ToolExecutor,
    session_id: &str,
) -> Result<Vec<String>, anyhow::Error> {
    let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        REPO_STANDARDS_CALL_TIMEOUT,
        tools.call(
            session_id.to_string(),
            Uuid::new_v4().to_string(),
            "repo_standards".to_string(),
            Value::Null,
            delta_tx,
        ),
    )
    .await
    .context("the node did not answer the repo-standards request")??;
    if outcome.is_error {
        anyhow::bail!(
            "the node failed to list the working copy's repo standards: {}",
            outcome.content
        );
    }
    let present = outcome.content.get("present").ok_or_else(|| {
        anyhow::anyhow!("the node's repo-standards response has no \"present\" field")
    })?;
    Ok(serde_json::from_value(present.clone())?)
}
