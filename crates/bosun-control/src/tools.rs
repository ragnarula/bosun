//! Tool calls over the node tunnel. Each call resolves the session's node
//! from the store, opens a fresh logical connection on that node's tunnel
//! addressed with the session id, and sends one typed operation frame the
//! node relay dispatches to the session's in-process executor.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use bosun_agent::agent_loop::ToolError;
use bosun_agent::agent_loop::ToolExecutor;
use bosun_agent::agent_loop::ToolOutcome;
use bosun_common::session::Permission;
use bosun_common::tool::ToolDelta;
use bosun_common::tool::ToolMsg;
use bosun_common::tool::ToolOp;
use bosun_common::tool::read_tool_frame;
use bosun_common::tool::write_tool_frame;
use bosun_common::tunnel::LogicalStream;
use bosun_store::store::Store;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

use crate::tunnel::TunnelError;
use crate::tunnel::TunnelRegistry;

/// Dispatches tool calls to the session's executor on its node. The session
/// row names its node; a logical connection opened on that node's tunnel
/// carries the session id, and the node relay dispatches the operation to the
/// session's in-process executor.
pub struct TunnelToolExecutor {
    pub tunnels: Arc<TunnelRegistry>,
    pub store: Store,
}

impl ToolExecutor for TunnelToolExecutor {
    fn call(
        &self,
        session_id: String,
        run_id: String,
        name: String,
        args: Value,
        delta: mpsc::UnboundedSender<ToolDelta>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolError>> + Send>> {
        let tunnels = self.tunnels.clone();
        let store = self.store.clone();
        Box::pin(async move {
            let outcome =
                call_tool(&tunnels, &store, &session_id, &run_id, &name, &args, &delta).await?;
            Ok(outcome)
        })
    }

    fn cancel(
        &self,
        session_id: String,
        run_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send>> {
        let tunnels = self.tunnels.clone();
        let store = self.store.clone();
        Box::pin(async move {
            if let Err(error) = cancel_tool(&tunnels, &store, &session_id, &run_id).await {
                debug!(
                    msg = "tool cancel failed; the tool dies with the tunnel",
                    session_id = %session_id,
                    run_id = %run_id,
                    error = %error
                );
            }
            Ok(())
        })
    }
}

async fn call_tool(
    tunnels: &TunnelRegistry,
    store: &Store,
    session_id: &str,
    run_id: &str,
    name: &str,
    args: &Value,
    delta: &mpsc::UnboundedSender<ToolDelta>,
) -> anyhow::Result<ToolOutcome> {
    debug!(
        msg = "dispatching tool call to the executor",
        session_id = %session_id,
        tool = %name,
        run_id = %run_id
    );
    let node = resolve_session_node(store, session_id).await?;
    let mut conn = open_connection(tunnels, &node, session_id).await?;
    write_tool_frame(
        &mut conn,
        &ToolOp::Call {
            run_id: run_id.to_string(),
            tool: name.to_string(),
            args: args.clone(),
        },
    )
    .await
    .with_context(|| format!("failed to send the {name} tool call"))?;

    let mut output = String::new();
    let mut is_shell = false;
    loop {
        let message = read_tool_frame::<_, ToolMsg>(&mut conn)
            .await
            .with_context(|| format!("failed to read the {name} tool response"))?
            .with_context(|| format!("the node closed the connection while {name} was running"))?;
        match message {
            ToolMsg::Error { message } => {
                warn!(
                    msg = "executor refused the tool call",
                    session_id = %session_id,
                    tool = %name,
                    run_id = %run_id,
                    error = %message
                );
                return Ok(ToolOutcome {
                    content: json!({ "error": message }),
                    is_error: true,
                });
            }
            ToolMsg::Result { content } => {
                return Ok(ToolOutcome {
                    content,
                    is_error: false,
                });
            }
            ToolMsg::Event { text } => {
                is_shell = true;
                output.push_str(&text);
                let _ = delta.send(ToolDelta { text });
            }
            ToolMsg::Done { exit_code } => {
                if !is_shell {
                    warn!(
                        msg = "executor sent a done frame outside a shell run",
                        session_id = %session_id,
                        tool = %name
                    );
                }
                return Ok(ToolOutcome {
                    content: json!({ "output": output, "exit_code": exit_code }),
                    is_error: exit_code != 0,
                });
            }
            ToolMsg::Ack => {
                warn!(
                    msg = "executor answered a tool call with an ack",
                    session_id = %session_id,
                    tool = %name
                );
            }
        }
    }
}

async fn cancel_tool(
    tunnels: &TunnelRegistry,
    store: &Store,
    session_id: &str,
    run_id: &str,
) -> anyhow::Result<()> {
    let node = resolve_session_node(store, session_id).await?;
    let mut conn = open_connection(tunnels, &node, session_id).await?;
    write_tool_frame(
        &mut conn,
        &ToolOp::Cancel {
            run_id: run_id.to_string(),
        },
    )
    .await
    .context("failed to send the cancel request")?;
    // The node answers with an ack once the run is told to die. An error or a
    // lost connection is left to the caller's best-effort handling.
    let _ = read_tool_frame::<_, ToolMsg>(&mut conn)
        .await
        .context("failed to read the cancel response")?;
    Ok(())
}

/// Forwards a permission change to the session's executor. The caller treats
/// transport errors as best-effort: the loop's tool schema gates the
/// permission too.
pub async fn set_executor_permission(
    tunnels: &TunnelRegistry,
    node: &str,
    session_id: &str,
    permission: Permission,
) -> anyhow::Result<()> {
    let mut conn = open_connection(tunnels, node, session_id).await?;
    write_tool_frame(&mut conn, &ToolOp::SetPermission { permission })
        .await
        .context("failed to send the permission request")?;
    let message = read_tool_frame::<_, ToolMsg>(&mut conn)
        .await
        .context("failed to read the permission response")?
        .context("the node closed the connection while applying the permission")?;
    match message {
        ToolMsg::Ack => Ok(()),
        ToolMsg::Error { message } => {
            anyhow::bail!("executor refused the permission change: {message}")
        }
        other => anyhow::bail!("executor answered the permission change with {other:?}"),
    }
}

/// Resolves the node a session runs on from the store. The session row is the
/// control plane's record of where the session's executor lives.
async fn resolve_session_node(store: &Store, session_id: &str) -> anyhow::Result<String> {
    let session = store
        .get_session(session_id)
        .await
        .with_context(|| format!("failed to look up session {session_id}"))?
        .with_context(|| format!("session {session_id} was not found"))?;
    Ok(session.node)
}

/// Opens a logical connection on the session's node tunnel.
async fn open_connection(
    tunnels: &TunnelRegistry,
    node: &str,
    session_id: &str,
) -> anyhow::Result<LogicalStream> {
    match tunnels.open(node, session_id).await {
        Ok(stream) => Ok(stream),
        Err(TunnelError::NoTunnel { .. }) | Err(TunnelError::TunnelClosed { .. }) => {
            debug!(
                msg = "the session's node has no live tunnel",
                session_id = %session_id,
                node = %node
            );
            anyhow::bail!("session {session_id} has no live tunnel");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bosun_common::session::Session;
    use bosun_common::session::SessionState;
    use bosun_common::tunnel::Tunnel;
    use bosun_store::store::Store;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    use super::*;

    /// A node side that speaks the typed tool protocol. It reads each opened
    /// connection's operation and answers with canned frames, recording
    /// cancels and permission changes like the real relay would dispatch
    /// them.
    async fn stub_node() -> (
        TunnelToolExecutor,
        String,
        String,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<Vec<Permission>>>,
    ) {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _) = Tunnel::new(cp_side);
        let (node_tunnel, mut opens) = Tunnel::new(node_side);

        let session_id = uuid::Uuid::new_v4().to_string();
        let node = "node-1".to_string();
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&Session {
                id: session_id.clone(),
                node: node.clone(),
                repo_url: None,
                git_ref: None,
                dir: "/work".into(),
                model: "mock".into(),
                persona: None,
                parent_id: None,
                owner_id: session_id.clone(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                state: SessionState::WaitingForInput,
                interrupt_cause: None,
                created_at_secs: 1_700_000_000,
                prompt: None,
            })
            .await
            .unwrap();
        let tunnels = TunnelRegistry::new();
        tunnels.register(&node, cp_tunnel);

        let cancels: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let permissions: Arc<Mutex<Vec<Permission>>> = Arc::new(Mutex::new(Vec::new()));
        let cancels_for_task = cancels.clone();
        let permissions_for_task = permissions.clone();
        tokio::spawn(async move {
            while let Some(event) = opens.recv().await {
                let tunnel = node_tunnel.clone();
                let cancels = cancels_for_task.clone();
                let permissions = permissions_for_task.clone();
                tokio::spawn(async move {
                    let Some(mut conn) = tunnel.attach(event.conn_id, event.rx) else {
                        return;
                    };
                    let op = match read_tool_frame::<_, ToolOp>(&mut conn).await {
                        Ok(Some(op)) => op,
                        _ => return,
                    };
                    let reply = match op {
                        ToolOp::Call { tool, args, .. } => match tool.as_str() {
                            "echo" => Some(ToolMsg::Result {
                                content: json!({ "echo": "hi" }),
                            }),
                            "forbidden" => Some(ToolMsg::Error {
                                message: "read-only tool refused".into(),
                            }),
                            "shell" => {
                                let exit_code = args["exit_code"].as_i64().unwrap_or(0) as i32;
                                let _ = write_tool_frame(
                                    &mut conn,
                                    &ToolMsg::Event {
                                        text: "building...".into(),
                                    },
                                )
                                .await;
                                let _ = write_tool_frame(
                                    &mut conn,
                                    &ToolMsg::Event {
                                        text: "finished".into(),
                                    },
                                )
                                .await;
                                Some(ToolMsg::Done { exit_code })
                            }
                            other => Some(ToolMsg::Error {
                                message: format!("unknown tool {other}"),
                            }),
                        },
                        ToolOp::Cancel { run_id } => {
                            cancels.lock().unwrap().push(run_id);
                            Some(ToolMsg::Ack)
                        }
                        ToolOp::SetPermission { permission } => {
                            permissions.lock().unwrap().push(permission);
                            Some(ToolMsg::Ack)
                        }
                    };
                    if let Some(reply) = reply {
                        let _ = write_tool_frame(&mut conn, &reply).await;
                    }
                });
            }
        });

        (
            TunnelToolExecutor {
                tunnels: Arc::new(tunnels),
                store,
            },
            session_id,
            node,
            cancels,
            permissions,
        )
    }

    #[tokio::test]
    async fn call_dispatches_json_tools() {
        let (executor, session_id, _node, _cancels, _permissions) = stub_node().await;
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();

        let outcome = executor
            .call(
                session_id,
                "run-1".into(),
                "echo".into(),
                json!({ "text": "hi" }),
                delta_tx,
            )
            .await
            .unwrap();

        assert_eq!(outcome.content, json!({ "echo": "hi" }));
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn call_streams_shell_output() {
        let (executor, session_id, _node, _cancels, _permissions) = stub_node().await;
        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel();

        let outcome = executor
            .call(
                session_id,
                "run-2".into(),
                "shell".into(),
                json!({ "command": "cargo build" }),
                delta_tx,
            )
            .await
            .unwrap();

        let mut deltas = Vec::new();
        while let Ok(delta) = delta_rx.try_recv() {
            deltas.push(delta.text);
        }
        assert_eq!(deltas, ["building...", "finished"]);
        assert_eq!(
            outcome.content,
            json!({ "output": "building...finished", "exit_code": 0 })
        );
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn call_reports_an_error_reply_as_a_tool_error_outcome() {
        let (executor, session_id, _node, _cancels, _permissions) = stub_node().await;
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();

        let outcome = executor
            .call(
                session_id,
                "run-3".into(),
                "forbidden".into(),
                json!({}),
                delta_tx,
            )
            .await
            .unwrap();

        assert!(outcome.is_error);
        assert_eq!(
            outcome.content,
            json!({ "error": "read-only tool refused" })
        );
    }

    #[tokio::test]
    async fn a_nonzero_exit_code_marks_the_outcome_as_error() {
        let (executor, session_id, _node, _cancels, _permissions) = stub_node().await;
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();

        let outcome = executor
            .call(
                session_id,
                "run-4".into(),
                "shell".into(),
                json!({ "command": "exit 3", "exit_code": 3 }),
                delta_tx,
            )
            .await
            .unwrap();

        assert!(outcome.is_error);
        assert_eq!(
            outcome.content,
            json!({ "output": "building...finished", "exit_code": 3 })
        );
    }

    #[tokio::test]
    async fn cancel_sends_a_cancel_operation() {
        let (executor, session_id, _node, cancels, _permissions) = stub_node().await;

        executor.cancel(session_id, "run-9".into()).await.unwrap();

        assert!(
            cancels.lock().unwrap().iter().any(|id| id == "run-9"),
            "the executor never received the cancel operation"
        );
    }

    #[tokio::test]
    async fn set_executor_permission_sends_the_permission_operation() {
        let (executor, session_id, node, _cancels, permissions) = stub_node().await;

        set_executor_permission(&executor.tunnels, &node, &session_id, Permission::ReadOnly)
            .await
            .unwrap();

        assert_eq!(
            *permissions.lock().unwrap(),
            vec![Permission::ReadOnly],
            "the executor never received the permission operation"
        );
    }

    #[tokio::test]
    async fn set_executor_permission_fails_on_an_error_reply() {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _) = Tunnel::new(cp_side);
        let (node_tunnel, mut opens) = Tunnel::new(node_side);
        let tunnels = TunnelRegistry::new();
        tunnels.register("node-1", cp_tunnel);
        tokio::spawn(async move {
            while let Some(event) = opens.recv().await {
                let tunnel = node_tunnel.clone();
                tokio::spawn(async move {
                    let Some(mut conn) = tunnel.attach(event.conn_id, event.rx) else {
                        return;
                    };
                    let _ = read_tool_frame::<_, ToolOp>(&mut conn).await;
                    let _ = write_tool_frame(
                        &mut conn,
                        &ToolMsg::Error {
                            message: "no such session".into(),
                        },
                    )
                    .await;
                });
            }
        });

        let error = set_executor_permission(&tunnels, "node-1", "ghost", Permission::ReadOnly)
            .await
            .expect_err("an error reply must fail the permission change");
        assert!(
            error.to_string().contains("no such session"),
            "the error names the executor's reason: {error}"
        );
    }

    #[tokio::test]
    async fn call_reports_a_missing_node_tunnel_as_no_live_tunnel() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        store
            .create_session(&Session {
                id: session_id.clone(),
                node: "node-down".into(),
                repo_url: None,
                git_ref: None,
                dir: "/work".into(),
                model: "mock".into(),
                persona: None,
                parent_id: None,
                owner_id: session_id.clone(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                state: SessionState::WaitingForInput,
                interrupt_cause: None,
                created_at_secs: 1_700_000_000,
                prompt: None,
            })
            .await
            .unwrap();
        let executor = TunnelToolExecutor {
            tunnels: Arc::new(TunnelRegistry::new()),
            store,
        };
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();

        let error = match executor
            .call(
                session_id.clone(),
                "run-5".into(),
                "echo".into(),
                json!({}),
                delta_tx,
            )
            .await
        {
            Err(bosun_agent::agent_loop::ToolError::Internal(error)) => error,
            Ok(_) => panic!("no tunnel means the call fails"),
        };

        let chain = format!("{error:?}");
        // No error variant distinguishes no-live-tunnel, so the message text is the only handle.
        assert!(
            chain.contains(&format!("session {session_id} has no live tunnel")),
            "the error chain must name the session like a node that is down: {chain}"
        );
    }
}
