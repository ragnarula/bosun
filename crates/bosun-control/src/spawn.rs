//! Spawning real child sessions from an agent loop's `spawn` tool. The child
//! is a full session: the control plane asks the node to start its own
//! executor on the parent's working copy, creates the child's session row,
//! and starts its own loop. The parent's turn gets the child's id back and
//! continues; the child runs concurrently and reports when it completes.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use std::time::SystemTime;

use bosun_agent::agent_loop::ChildSpawner;
use bosun_agent::agent_loop::SpawnChild;
use bosun_agent::agent_loop::SpawnError;
use bosun_common::session::Block;
use bosun_common::session::Role;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::types::CommandResult;
use bosun_common::types::NodeCommand;
use bosun_common::types::SessionInfo;
use bosun_store::store::Store;
use bosun_store::store::StoreError;
use tokio::sync::oneshot;
use tracing::info;

use crate::commands::CommandQueue;
use crate::loops::AgentRegistry;
use crate::registry::NodeRegistry;
use crate::tunnel::TunnelRegistry;

/// How long the parent's spawn tool call waits for the node to start the
/// child's executor, mirroring the clone and dev request timeout.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(300);

/// The registry-owned child spawner. It holds a weak reference back to the
/// registry: the registry starts the child's loop and owns this spawner, so
/// a strong reference would leak the whole registry.
pub struct ChildSessionSpawner {
    pub registry: Weak<AgentRegistry>,
    pub nodes: Arc<NodeRegistry>,
    pub commands: Arc<CommandQueue>,
    pub tunnels: Arc<TunnelRegistry>,
}

impl ChildSpawner for ChildSessionSpawner {
    fn spawn(
        &self,
        store: Store,
        request: SpawnChild,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, SpawnError>> + Send>> {
        let registry = self.registry.clone();
        let nodes = self.nodes.clone();
        let commands = self.commands.clone();
        let tunnels = self.tunnels.clone();
        Box::pin(
            async move { spawn_child(&registry, &nodes, &commands, tunnels, store, request).await },
        )
    }
}

/// Starts the child's executor on the node through a `Start` command (the
/// internal command for running an executor in a directory that already
/// exists on the node), creates the child's session row, starts its loop,
/// hands it the assignment as its first user message, and returns the child's
/// id.
async fn spawn_child(
    registry: &Weak<AgentRegistry>,
    nodes: &NodeRegistry,
    commands: &CommandQueue,
    tunnels: Arc<TunnelRegistry>,
    store: Store,
    request: SpawnChild,
) -> Result<String, SpawnError> {
    let registry = registry
        .upgrade()
        .ok_or_else(|| SpawnError::Failed("the agent registry is shutting down".to_string()))?;
    let SpawnChild {
        parent,
        persona_name,
        persona,
        instructions,
    } = request;
    let parent_log_id = parent.id.clone();
    let persona_log_name = persona_name.clone();
    if nodes.node(&parent.node, SystemTime::now()).is_none() {
        return Err(SpawnError::Failed(format!(
            "node {} is not up",
            parent.node
        )));
    }

    let child_id = uuid::Uuid::new_v4().to_string();
    // The child runs in the parent's working copy, a directory this node
    // already created. The start command is the internal executor-in-
    // existing-dir command; the node confines the directory to its browse
    // roots exactly like dev, so a clone-session parent needs a root that
    // covers the node's work_dir.
    let command = NodeCommand::Start {
        id: commands.next_id(),
        session_id: child_id.clone(),
        dir: PathBuf::from(&parent.dir),
        permission: persona.permission,
    };
    let node_session = enqueue_and_await(commands, &parent.node, command).await?;

    let dir = node_session
        .dir
        .map(|dir| dir.display().to_string())
        .ok_or_else(|| {
            SpawnError::Failed(format!(
                "node {} did not report a directory for the child session",
                parent.node
            ))
        })?;
    let child = Session {
        id: child_id.clone(),
        node: parent.node,
        repo_url: parent.repo_url,
        git_ref: parent.git_ref,
        dir,
        model: persona.model.clone(),
        persona: Some(persona_name),
        parent_id: Some(parent.id),
        owner_id: parent.owner_id,
        permission: persona.permission,
        allowed_tools: persona.allowed_tools.clone(),
        state: SessionState::Creating,
        interrupt_cause: None,
        created_at_secs: bosun_common::time::unix_secs(SystemTime::now()),
        prompt: Some(instructions.clone()),
    };
    store.create_session(&child).await.map_err(store_error)?;

    // The child's loop and its first turn reuse the root start flow: the
    // executor is already up, the assignment is the child's first user
    // message, and the child runs its own turns from here on.
    let provider = registry
        .providers
        .get(&persona.model)
        .cloned()
        .ok_or_else(|| SpawnError::Failed(format!("no provider for model {}", persona.model)))?;
    registry.start(&child_id, store.clone(), provider, tunnels, &persona.model);
    store
        .append_message(&child_id, Role::User, &Block::Text { text: instructions })
        .await
        .map_err(store_error)?;
    registry.wake(&child_id);
    info!(
        session_id = %child_id,
        parent_id = %parent_log_id,
        persona = %persona_log_name,
        "child session spawned"
    );
    Ok(child_id)
}

/// A store write failure inside a spawn is an internal error: the caller can
/// only report it.
fn store_error(error: StoreError) -> SpawnError {
    SpawnError::Internal(anyhow::Error::new(error))
}

/// Queues a command for the node and waits for its result, delivered in the
/// node's next poll. The timeout and the error texts mirror the clone and
/// dev request path in api.rs.
async fn enqueue_and_await(
    commands: &CommandQueue,
    node: &str,
    command: NodeCommand,
) -> Result<SessionInfo, SpawnError> {
    let (reply, reply_rx) = oneshot::channel();
    commands.enqueue(node, command, Some(reply));
    let result = match tokio::time::timeout(SPAWN_TIMEOUT, reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            return Err(SpawnError::Failed(format!(
                "node {node} dropped the command reply"
            )));
        }
        Err(_) => return Err(SpawnError::Failed(format!("node {node} is unreachable"))),
    };
    match result {
        CommandResult::Session { session, .. } => Ok(session),
        CommandResult::Error { message, .. } => Err(SpawnError::Failed(format!(
            "node {node} rejected the request: {message}"
        ))),
        _ => Err(SpawnError::Failed(format!(
            "node {node} answered start with a non-session result"
        ))),
    }
}
