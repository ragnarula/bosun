//! Spawning real child sessions from an agent loop's `spawn` tool. The child
//! is a full session: the control plane asks a node to start its own executor
//! in the parent's working copy, or in a node and directory the caller named,
//! creates the child's session row, and starts its own loop. The parent's turn
//! gets the child's id back and continues; the child runs concurrently and
//! reports when it completes.

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
        node,
        dir,
    } = request;
    let parent_log_id = parent.id.clone();
    let persona_log_name = persona_name.clone();
    let placement = placement(
        nodes,
        &parent,
        node.as_deref(),
        dir.as_deref(),
        SystemTime::now(),
    )?;

    let child_id = uuid::Uuid::new_v4().to_string();
    // The node confines a named directory to its own browse roots exactly like
    // dev, so an out-of-roots request is refused there and reaches the parent
    // as a tool error; a command with no directory asks the node to pick one
    // under its work directory. A clone-session parent lives under the node's
    // work_dir, so a root has to cover it for the parent's own copy to be a
    // legal placement.
    let command = NodeCommand::Start {
        id: commands.next_id(),
        session_id: child_id.clone(),
        dir: placement.dir.clone(),
        permission: persona.permission,
    };
    let node_session = enqueue_and_await(commands, &placement.node, command).await?;

    let dir = node_session
        .dir
        .map(|dir| dir.display().to_string())
        .ok_or_else(|| {
            SpawnError::Failed(format!(
                "node {} did not report a directory for the child session",
                placement.node
            ))
        })?;
    // The child keeps the parent's repository only when it runs in the
    // parent's own directory; one placed elsewhere works in a directory that
    // holds no such clone.
    let in_parent_dir = dir == parent.dir;
    let child = Session {
        id: child_id.clone(),
        node: placement.node.clone(),
        repo_url: if in_parent_dir {
            parent.repo_url.clone()
        } else {
            None
        },
        git_ref: if in_parent_dir {
            parent.git_ref.clone()
        } else {
            None
        },
        dir,
        model: persona.model.clone(),
        persona: Some(persona_name),
        parent_id: Some(parent.id),
        owner_id: parent.owner_id,
        permission: persona.permission,
        allowed_tools: persona.allowed_tools.clone(),
        mcp_servers: "".into(),
        state: SessionState::Creating,
        interrupt_cause: None,
        created_at_secs: bosun_common::time::unix_secs(SystemTime::now()),
        prompt: Some(instructions.clone()),
        summary: None,
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

/// Where a spawn puts the child: the node it starts on, and the directory the
/// caller named. `None` for the directory asks the node to choose one under
/// its own work directory.
#[derive(Debug)]
struct Placement {
    node: String,
    dir: Option<PathBuf>,
}

/// Resolves the placement the tool asked for. With no node the child keeps the
/// parent's place, unchanged: the parent's node and the parent's working copy.
/// A named node must be registered and up, and a directory needs a node to
/// confine it to that node's browse roots.
fn placement(
    nodes: &NodeRegistry,
    parent: &Session,
    node: Option<&str>,
    dir: Option<&str>,
    now: SystemTime,
) -> Result<Placement, SpawnError> {
    let Some(named) = node else {
        if dir.is_some() {
            return Err(SpawnError::Failed(
                "dir needs node: without a node the child starts in the parent's directory"
                    .to_string(),
            ));
        }
        if nodes.node(&parent.node, now).is_none() {
            return Err(SpawnError::Failed(format!(
                "node {} is not up",
                parent.node
            )));
        }
        return Ok(Placement {
            node: parent.node.clone(),
            dir: Some(PathBuf::from(&parent.dir)),
        });
    };
    if nodes.node(named, now).is_none() {
        return Err(SpawnError::Failed(
            if nodes.list(now).iter().any(|health| health.name == named) {
                format!("node {named} is not up")
            } else {
                format!("unknown node {named}")
            },
        ));
    }
    Ok(Placement {
        node: named.to_string(),
        dir: dir.map(PathBuf::from),
    })
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

#[cfg(test)]
mod tests {
    use bosun_common::session::Permission;
    use bosun_common::types::UpdateStatus;

    use super::*;

    /// A parent session running on `node`, working in `dir`.
    fn parent(node: &str, dir: &str) -> Session {
        Session {
            id: "parent-1".into(),
            node: node.into(),
            repo_url: Some("https://example.com/repo".into()),
            git_ref: Some("main".into()),
            dir: dir.into(),
            model: "test-model".into(),
            persona: Some("coder".into()),
            parent_id: None,
            owner_id: "parent-1".into(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            mcp_servers: "".into(),
            state: SessionState::Creating,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: None,
            summary: None,
        }
    }

    /// A registry holding one record per node, last heard from at `seen`.
    fn nodes(seen: &[(&str, SystemTime)]) -> NodeRegistry {
        let registry = NodeRegistry::new(Duration::from_secs(10));
        for (name, at) in seen {
            registry.upsert(name, "0.9.40", UpdateStatus::UpToDate, *at);
        }
        registry
    }

    #[test]
    fn a_spawn_with_no_node_keeps_the_parents_place() {
        let now = SystemTime::now();
        let registry = nodes(&[("n1", now)]);
        let placement = placement(&registry, &parent("n1", "/work/repo"), None, None, now)
            .expect("a spawn with no node keeps today's place");
        assert_eq!(placement.node, "n1");
        assert_eq!(
            placement.dir,
            Some(PathBuf::from("/work/repo")),
            "the child runs in the parent's working copy"
        );
    }

    #[test]
    fn a_named_node_takes_the_child_with_the_directory_it_was_given() {
        let now = SystemTime::now();
        let registry = nodes(&[("n1", now), ("n2", now)]);
        let placement = placement(
            &registry,
            &parent("n1", "/work/repo"),
            Some("n2"),
            Some("/work/other"),
            now,
        )
        .expect("a node that is up may be named");
        assert_eq!(placement.node, "n2");
        assert_eq!(placement.dir, Some(PathBuf::from("/work/other")));
    }

    #[test]
    fn a_named_node_with_no_directory_leaves_the_choice_to_that_node() {
        let now = SystemTime::now();
        let registry = nodes(&[("n1", now), ("n2", now)]);
        let placement = placement(
            &registry,
            &parent("n1", "/work/repo"),
            Some("n2"),
            None,
            now,
        )
        .unwrap();
        assert_eq!(placement.node, "n2");
        assert_eq!(
            placement.dir, None,
            "no directory is the node's own placement to make, under its work directory"
        );
    }

    #[test]
    fn an_unknown_node_is_refused_by_name() {
        let now = SystemTime::now();
        let registry = nodes(&[("n1", now)]);
        let error = placement(
            &registry,
            &parent("n1", "/work/repo"),
            Some("ghost"),
            None,
            now,
        )
        .expect_err("a node the registry has never seen cannot host a child");
        assert_eq!(error.to_string(), "unknown node ghost");
    }

    #[test]
    fn a_node_that_is_not_up_is_refused() {
        let now = SystemTime::now();
        let stale = now - Duration::from_secs(60);
        let registry = nodes(&[("n1", now), ("n2", stale)]);
        let error = placement(
            &registry,
            &parent("n1", "/work/repo"),
            Some("n2"),
            None,
            now,
        )
        .expect_err("a node that stopped reporting cannot host a child");
        assert_eq!(error.to_string(), "node n2 is not up");
    }

    #[test]
    fn a_parent_whose_own_node_is_not_up_is_refused() {
        let now = SystemTime::now();
        let stale = now - Duration::from_secs(60);
        let registry = nodes(&[("n1", stale)]);
        let error = placement(&registry, &parent("n1", "/work/repo"), None, None, now)
            .expect_err("the unchanged path still needs the parent's node up");
        assert_eq!(error.to_string(), "node n1 is not up");
    }

    #[test]
    fn a_directory_without_a_node_is_refused() {
        let now = SystemTime::now();
        let registry = nodes(&[("n1", now)]);
        let error = placement(
            &registry,
            &parent("n1", "/work/repo"),
            None,
            Some("/work/other"),
            now,
        )
        .expect_err("a directory needs the node whose browse roots confine it");
        assert_eq!(
            error.to_string(),
            "dir needs node: without a node the child starts in the parent's directory"
        );
    }
}
