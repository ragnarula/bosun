use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::tool::ToolMsg;
use bosun_common::tool::ToolOp;
use bosun_common::tool::read_tool_frame;
use bosun_common::tool::write_tool_frame;
use bosun_common::tunnel::LogicalStream;
use bosun_common::tunnel::OpenEvent;
use bosun_common::tunnel::Tunnel;
use bosun_executor::CallOutcome;
use bosun_executor::ExecutorError;
use bosun_executor::ExecutorState;
use bosun_executor::ShellEvent;
use bosun_executor::ShellStream;
use bosun_executor::run_call;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::Empty;
use hyper::client::conn::http1;
use hyper::header;
use hyper::http::StatusCode;
use hyper::http::Uri;
use hyper::upgrade::Upgraded;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tower_service::Service as _;
use tracing::debug;
use tracing::error;
use tracing::warn;

use crate::manager::NodeManager;

const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// The next suffix for a spilled file's name. The suffix keeps two spills in
/// the same session from overwriting each other even when a caller reuses a
/// run id, so the file a `Spilled` frame names always holds that frame's
/// content.
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Keeps the node's one outbound tunnel to the control plane open. On any
/// failure the connection is re-established after a short delay; the node's
/// sessions keep their executors, so a reconnect restores every session's
/// tool calls at once. Runs until the node exits, independent of how many
/// sessions the node hosts.
pub async fn run_node_tunnel(
    cp_url: String,
    node_name: String,
    manager: Arc<NodeManager>,
    tls_config: Option<Arc<ClientConfig>>,
) {
    loop {
        match connect_tunnel(&cp_url, &node_name, tls_config.clone()).await {
            Ok(stream) => {
                let (tunnel, opens) = Tunnel::new(stream);
                relay_tunnel(tunnel, opens, manager.clone()).await;
            }
            Err(error) => {
                warn!(
                    node = %node_name,
                    error = %error.display_chain(),
                    "node tunnel failed; reconnecting"
                );
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Relays every logical connection the control plane opens on the node's
/// tunnel until the tunnel dies. Each opened connection names a session, so
/// the relay dispatches it to that session's in-process executor.
async fn relay_tunnel(
    tunnel: Tunnel,
    mut opens: mpsc::UnboundedReceiver<OpenEvent>,
    manager: Arc<NodeManager>,
) {
    loop {
        tokio::select! {
            event = opens.recv() => {
                let Some(event) = event else { break };
                let tunnel = tunnel.clone();
                let manager = manager.clone();
                tokio::spawn(relay_connection(event, manager, tunnel));
            }
            _ = tunnel.closed() => break,
        }
    }
}

/// Relays one logical connection to the executor of the session it names.
/// Attaching and dropping the logical stream sends a close frame, so a tool
/// call aimed at a session this node does not run fails instead of hanging.
async fn relay_connection(event: OpenEvent, manager: Arc<NodeManager>, tunnel: Tunnel) {
    let Some(state) = manager.executor(&event.session_id) else {
        debug!(
            conn_id = event.conn_id,
            session_id = %event.session_id,
            "no executor for the session; closing the connection"
        );
        let _ = tunnel.attach(event.conn_id, event.rx);
        return;
    };
    let Some(mut logical) = tunnel.attach(event.conn_id, event.rx) else {
        return;
    };
    let op = match read_tool_frame::<_, ToolOp>(&mut logical).await {
        Ok(Some(op)) => op,
        Ok(None) => {
            debug!(
                conn_id = event.conn_id,
                session_id = %event.session_id,
                "the control plane closed the connection without an operation"
            );
            return;
        }
        Err(error) => {
            debug!(
                conn_id = event.conn_id,
                session_id = %event.session_id,
                error = %error,
                "failed to read the operation frame; closing the connection"
            );
            return;
        }
    };
    match op {
        ToolOp::Cancel { run_id } => {
            state.cancel(&run_id).await;
            if let Err(error) = write_tool_frame(&mut logical, &ToolMsg::Ack).await {
                debug!(
                    conn_id = event.conn_id,
                    session_id = %event.session_id,
                    run_id = %run_id,
                    error = %error,
                    "failed to write the cancel ack; closing the connection"
                );
            }
        }
        ToolOp::SetPermission { permission } => {
            state.set_permission(permission).await;
            if let Err(error) = write_tool_frame(&mut logical, &ToolMsg::Ack).await {
                debug!(
                    conn_id = event.conn_id,
                    session_id = %event.session_id,
                    error = %error,
                    "failed to write the permission ack; closing the connection"
                );
            }
        }
        ToolOp::Call { run_id, tool, args } => {
            relay_call(logical, &state, &run_id, &tool, &args).await;
        }
    }
}

/// Dispatches one tool call and writes its response frames back over the
/// connection. A JSON result larger than the spill limits is written to a
/// file in the session's working copy and replaced by a `Spilled` frame
/// naming the file and a short preview, so the model's context stays bounded
/// and the full content stays readable through `file_read`. A shell run
/// streams its output live up to the same byte budget, then spills the full
/// file at the end (see [`relay_shell_stream`]).
async fn relay_call(
    logical: LogicalStream,
    state: &Arc<ExecutorState>,
    run_id: &str,
    tool: &str,
    args: &Value,
) {
    let outcome = run_call(state, run_id, tool, args).await;
    let mut logical = logical;
    match outcome {
        Ok(CallOutcome::Result { content }) => {
            // A JSON result spills on its serialized text: the model reads the
            // file instead of a value the size of a novel in its context.
            let text = match &content {
                Value::String(text) => text.clone(),
                other => serde_json::to_string(other).expect("a result value serializes"),
            };
            let frame = if bosun_common::tool::should_spill(&text) {
                match spill_to_file(state, run_id, tool, &text).await {
                    Ok(file) => ToolMsg::Spilled {
                        file,
                        preview: preview(&text),
                        exit_code: None,
                    },
                    Err(error) => {
                        warn!(
                            run_id = %run_id,
                            tool = %tool,
                            error = %error,
                            "failed to spill a large tool result"
                        );
                        ToolMsg::Result { content }
                    }
                }
            } else {
                ToolMsg::Result { content }
            };
            if let Err(error) = write_tool_frame(&mut logical, &frame).await {
                debug!(
                    run_id = %run_id,
                    tool = %tool,
                    error = %error,
                    "failed to write the result frame; closing the connection"
                );
            }
        }
        Ok(CallOutcome::Shell(stream)) => {
            relay_shell_stream(logical, state, run_id, tool, stream).await;
        }
        Err(error) => {
            match &error {
                ExecutorError::Tool(bosun_executor::tools::ToolError::Internal(internal)) => {
                    error!(
                        error = %internal.display_chain(),
                        tool = %tool,
                        run_id = %run_id,
                        "tool call failed with an internal error"
                    );
                }
                _ => warn!(error = %error, tool = %tool, run_id = %run_id, "tool call failed"),
            }
            if let Err(write_error) = write_tool_frame(
                &mut logical,
                &ToolMsg::Error {
                    message: error.to_string(),
                },
            )
            .await
            {
                debug!(
                    run_id = %run_id,
                    tool = %tool,
                    error = %write_error,
                    "failed to write the error frame; closing the connection"
                );
            }
        }
    }
}

/// Forwards a shell run's streamed events as frames until it ends with a done
/// code. The model's context stays bounded: output is forwarded live only up
/// to the spill byte budget, and whatever streams past it is still
/// accumulated. When the run ends and the accumulated output exceeds the
/// spill limits, the full output is written to a file in the session's
/// working copy and a `Spilled` frame is sent instead of `Done`; the model
/// reads the file with `file_read` when it needs what the live stream cut
/// off. While streaming, the control plane's end of the connection is
/// watched: when it closes, the stream is dropped, whose guard kills the
/// shell's process group and deregisters the run.
async fn relay_shell_stream(
    logical: LogicalStream,
    state: &Arc<ExecutorState>,
    run_id: &str,
    tool: &str,
    mut stream: ShellStream,
) {
    let (mut reader, mut writer) = tokio::io::split(logical);
    let mut buf = [0u8; 1024];
    // The full streamed output, kept so a run that exceeds the spill limits
    // can be written to a file at the end.
    let mut output = String::new();
    // The streamed bytes forwarded live to the control plane. Past the spill
    // byte budget nothing more is forwarded, so the model's context stays
    // bounded even though the run keeps producing output. The run still ends
    // as a spill (any output over the budget is at least this big), so the
    // full output stays reachable through `file_read`.
    let mut forwarded = 0usize;
    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                match event {
                    ShellEvent::Out(text) => {
                        output.push_str(&text);
                        let budget = bosun_common::tool::SPILL_BYTE_LIMIT;
                        if forwarded < budget {
                            let chunk = text
                                .chars()
                                .take(budget - forwarded)
                                .collect::<String>();
                            forwarded += chunk.len();
                            if write_tool_frame(&mut writer, &ToolMsg::Event { text: chunk })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    ShellEvent::Done(exit_code) => {
                        // A small shell run ends as today. A large one spills:
                        // the file holds the full output, the preview names it.
                        if bosun_common::tool::should_spill(&output) {
                            match spill_to_file(state, run_id, tool, &output).await {
                                Ok(file) => {
                                    let _ = write_tool_frame(
                                        &mut writer,
                                        &ToolMsg::Spilled {
                                            file,
                                            preview: preview(&output),
                                            exit_code: Some(exit_code),
                                        },
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    warn!(
                                        run_id = %run_id,
                                        tool = %tool,
                                        error = %error,
                                        "failed to spill a large shell output"
                                    );
                                    let _ = write_tool_frame(
                                        &mut writer,
                                        &ToolMsg::Done { exit_code },
                                    )
                                    .await;
                                }
                            }
                        } else {
                            if write_tool_frame(&mut writer, &ToolMsg::Done { exit_code }).await.is_err() {
                                break;
                            }
                        }
                        break;
                    }
                }
            }
            read = reader.read(&mut buf) => {
                match read {
                    // EOF means the control plane dropped its end.
                    Ok(0) => break,
                    // Unexpected bytes are discarded; the operation protocol
                    // is one request per connection, so nothing else arrives.
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

/// Writes `content` to `<session_dir>/tool-output/<run_id>-<tool>-<seq>.txt`
/// and returns the path relative to the working copy that `file_read`
/// resolves. A result spilled here stays in the session's working copy, so
/// the model can read the whole thing on demand without it leaving the node.
/// The per-node sequence suffix makes the name unique per spill even when a
/// caller reuses a run id, so an earlier spill is never overwritten and the
/// file a `Spilled` frame names always holds that frame's content.
async fn spill_to_file(
    state: &Arc<ExecutorState>,
    run_id: &str,
    tool: &str,
    content: &str,
) -> anyhow::Result<String> {
    let dir = state.session_dir.join("tool-output");
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;
    // A tool name like `file/read` and a run id from the wire must not become
    // nested paths in the spilled file's name.
    let tool_slug = tool.replace('/', "-").replace("..", "-");
    let run_slug = run_id.replace(['/', '\\'], "-").replace("..", "-");
    let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = format!("{run_slug}-{tool_slug}-{seq}.txt");
    let path = dir.join(&file_name);
    tokio::fs::write(&path, content)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(format!("tool-output/{file_name}"))
}

/// The leading portion of a spilled result kept inline as the preview. The
/// preview is capped by bytes, not characters: its purpose is to bound how
/// much of the result sits in the model's context.
fn preview(text: &str) -> String {
    let mut out = String::new();
    let mut bytes = 0usize;
    for character in text.chars() {
        let character_bytes = character.len_utf8();
        if bytes + character_bytes > bosun_common::tool::SPILLED_PREVIEW_BYTES {
            break;
        }
        bytes += character_bytes;
        out.push(character);
    }
    if text.len() > bosun_common::tool::SPILLED_PREVIEW_BYTES {
        out.push_str("\n… (spilled; read the file for the full output)");
    }
    out
}

async fn connect_tunnel(
    cp_url: &str,
    node_name: &str,
    tls_config: Option<Arc<ClientConfig>>,
) -> anyhow::Result<TokioIo<Upgraded>> {
    let mut connector = match tls_config {
        Some(config) => HttpsConnectorBuilder::new()
            .with_tls_config((*config).clone())
            .https_or_http()
            .enable_http1()
            .build(),
        None => HttpsConnectorBuilder::new()
            .with_platform_verifier()
            .https_or_http()
            .enable_http1()
            .build(),
    };

    let uri: Uri = format!("{}/tunnel/node/{node_name}", cp_url.trim_end_matches('/'))
        .parse()
        .context("cp_url is not a valid URL")?;
    let authority = uri
        .authority()
        .context("cp_url must include a host and port")?
        .to_string();
    let stream = connector.call(uri).await.map_err(|error| {
        anyhow::anyhow!("failed to connect to the control plane at {authority}: {error}")
    })?;

    let (mut sender, conn) = http1::handshake::<_, Empty<Bytes>>(stream)
        .await
        .context("failed to handshake with the control plane")?;
    tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            debug!(error = %error, "tunnel connection ended");
        }
    });

    let request = hyper::Request::builder()
        .method("GET")
        .uri(format!("/tunnel/node/{node_name}"))
        .header(header::HOST, authority.as_str())
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "bosun-tunnel")
        .body(Empty::<Bytes>::new())
        .context("failed to build the tunnel request")?;
    let response = sender
        .send_request(request)
        .await
        .context("failed to request the node tunnel")?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        anyhow::bail!(
            "the control plane refused the node tunnel: {}",
            response.status()
        );
    }
    let upgraded = hyper::upgrade::on(response)
        .await
        .context("the control plane accepted the upgrade but hyper provided none")?;
    Ok(TokioIo::new(upgraded))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path as AxumPath;
    use axum::extract::State;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::get;
    use bosun_common::session::Permission;
    use bosun_common::tool::ToolMsg;
    use bosun_common::tool::ToolOp;
    use bosun_common::tool::read_tool_frame;
    use bosun_common::tool::write_tool_frame;
    use bosun_common::tunnel::LogicalStream;
    use bosun_common::tunnel::Tunnel;
    use bosun_common::types::NodeStartRequest;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;

    /// A manager running one in-process session per id, each with its own
    /// directory under `root`.
    async fn manager_with(root: &Path, sessions: &[&str]) -> Arc<NodeManager> {
        let manager = Arc::new(NodeManager::new(
            root.to_path_buf(),
            vec![root.to_path_buf()],
            "http://127.0.0.1:1".into(),
            None,
        ));
        for session_id in sessions {
            let dir = root.join(session_id);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            manager
                .start(&NodeStartRequest {
                    session_id: (*session_id).into(),
                    dir,
                    permission: Permission::ReadWrite,
                })
                .await
                .expect("the session should start");
        }
        manager
    }

    /// A duplex pair of tunnels with the node side's opens exposed.
    fn tunnel_pair() -> (Tunnel, Tunnel, mpsc::UnboundedReceiver<OpenEvent>) {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _cp_opens) = Tunnel::new(cp_side);
        let (node_tunnel, opens) = Tunnel::new(node_side);
        (cp_tunnel, node_tunnel, opens)
    }

    async fn wait_until<F>(what: &str, mut condition: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if condition() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Sends one Call operation over a fresh connection and returns the
    /// terminal frame the node answers with.
    async fn typed_call(
        tunnel: &Tunnel,
        session_id: &str,
        run_id: &str,
        tool: &str,
        args: Value,
    ) -> ToolMsg {
        let mut conn = tunnel
            .open(session_id)
            .await
            .expect("the node tunnel is up");
        write_tool_frame(
            &mut conn,
            &ToolOp::Call {
                run_id: run_id.into(),
                tool: tool.into(),
                args,
            },
        )
        .await
        .unwrap();
        read_reply(&mut conn).await
    }

    /// Reads frames until a terminal one arrives.
    async fn read_reply(conn: &mut LogicalStream) -> ToolMsg {
        loop {
            let message =
                tokio::time::timeout(Duration::from_secs(10), read_tool_frame::<_, ToolMsg>(conn))
                    .await
                    .expect("the node never answered")
                    .expect("read failed")
                    .expect("the node closed the connection without a reply");
            match &message {
                ToolMsg::Ack
                | ToolMsg::Error { .. }
                | ToolMsg::Result { .. }
                | ToolMsg::Done { .. }
                | ToolMsg::Spilled { .. } => {
                    return message;
                }
                ToolMsg::Event { .. } => {}
            }
        }
    }

    /// The one node tunnel carries logical connections addressed to different
    /// sessions, and the relay dispatches each to that session's own
    /// executor, so two sessions' tool calls run concurrently over the same
    /// tunnel.
    #[tokio::test]
    async fn one_node_tunnel_dispatches_each_session_to_its_own_executor() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1", "s2"]).await;
        tokio::fs::write(root.path().join("s1/marker.txt"), "SRV1")
            .await
            .unwrap();
        tokio::fs::write(root.path().join("s2/marker.txt"), "SRV2")
            .await
            .unwrap();
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        // Each connection reached its own session's executor.
        let from_s1 = typed_call(
            &cp_tunnel,
            "s1",
            "run-1",
            "file_read",
            json!({ "path": "marker.txt" }),
        )
        .await;
        let ToolMsg::Result { content } = from_s1 else {
            panic!("s1's call must return a result: {from_s1:?}");
        };
        assert_eq!(content["content"], "SRV1");

        let from_s2 = typed_call(
            &cp_tunnel,
            "s2",
            "run-2",
            "file_read",
            json!({ "path": "marker.txt" }),
        )
        .await;
        let ToolMsg::Result { content } = from_s2 else {
            panic!("s2's call must return a result: {from_s2:?}");
        };
        assert_eq!(content["content"], "SRV2");

        // The session-to-executor direction flows too, and a write to one
        // session's working copy never reaches the other session's.
        let wrote = typed_call(
            &cp_tunnel,
            "s1",
            "run-3",
            "file_write",
            json!({ "path": "mine.txt", "content": "one" }),
        )
        .await;
        assert!(matches!(wrote, ToolMsg::Result { .. }));
        let read_back = typed_call(
            &cp_tunnel,
            "s1",
            "run-4",
            "file_read",
            json!({ "path": "mine.txt" }),
        )
        .await;
        let ToolMsg::Result { content } = read_back else {
            panic!("s1's read must return a result");
        };
        assert_eq!(content["content"], "one");
        assert!(
            !root.path().join("s2/mine.txt").exists(),
            "s2's working copy must not see s1's write"
        );

        relay.abort();
    }

    /// A logical connection aimed at a session this node does not run is
    /// closed instead of hanging, so the tool call fails rather than
    /// stalling.
    #[tokio::test]
    async fn an_open_for_an_unknown_session_is_closed() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let mut conn = cp_tunnel
            .open("ghost")
            .await
            .expect("the node tunnel is up");
        let mut buf = [0u8; 4];
        let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf))
            .await
            .expect("the unknown-session connection hung")
            .expect("read failed");
        assert_eq!(n, 0, "the peer closes the connection");

        relay.abort();
    }

    /// A refused tool call comes back as an error frame on the same
    /// connection.
    #[tokio::test]
    async fn a_failed_tool_call_answers_with_an_error_frame() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let reply = typed_call(
            &cp_tunnel,
            "s1",
            "run-1",
            "file_read",
            json!({ "path": "absent.txt" }),
        )
        .await;
        let ToolMsg::Error { message } = reply else {
            panic!("a missing file must answer with an error frame: {reply:?}");
        };
        assert!(message.contains("absent.txt"), "the error names the file");

        relay.abort();
    }

    /// A shell call streams event frames over the relay and ends with a done
    /// frame carrying the exit code.
    #[tokio::test]
    async fn a_shell_call_streams_events_and_ends_with_done() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let mut conn = cp_tunnel.open("s1").await.expect("open for s1");
        write_tool_frame(
            &mut conn,
            &ToolOp::Call {
                run_id: "run-1".into(),
                tool: "shell".into(),
                args: json!({ "command": "echo hello" }),
            },
        )
        .await
        .unwrap();

        let mut events = Vec::new();
        let mut exit_code = None;
        while exit_code.is_none() {
            let message = tokio::time::timeout(
                Duration::from_secs(10),
                read_tool_frame::<_, ToolMsg>(&mut conn),
            )
            .await
            .expect("the shell stream hung")
            .expect("read failed")
            .expect("the connection closed before the done frame");
            match message {
                ToolMsg::Event { text } => events.push(text),
                ToolMsg::Done { exit_code: code } => exit_code = Some(code),
                other => panic!("unexpected frame in a shell stream: {other:?}"),
            }
        }
        assert_eq!(exit_code, Some(0));
        assert!(
            events.iter().any(|text| text.contains("hello")),
            "the streamed output must carry the shell's output: {events:?}"
        );

        relay.abort();
    }

    /// A shell run whose output exceeds the spill limits ends with a Spilled
    /// frame instead of Done: the full output lands in a file under the
    /// session's working copy, and the frame names it with a short preview.
    #[tokio::test]
    async fn a_large_shell_run_spills_its_output_to_a_file() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let mut conn = cp_tunnel.open("s1").await.expect("open for s1");
        // Each line is padded well past the spill byte budget, so the run
        // spills on bytes as well as on line count: the live frames stop at
        // the byte budget while the spilled file holds the whole output.
        let command = format!(
            "for i in $(seq 1 {}); do echo line-$i-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA; done",
            bosun_common::tool::SPILL_LINE_LIMIT + 10
        );
        write_tool_frame(
            &mut conn,
            &ToolOp::Call {
                run_id: "run-big".into(),
                tool: "shell".into(),
                args: json!({ "command": command }),
            },
        )
        .await
        .unwrap();

        let mut events = Vec::new();
        let mut terminal = None;
        while terminal.is_none() {
            let message = tokio::time::timeout(
                Duration::from_secs(10),
                read_tool_frame::<_, ToolMsg>(&mut conn),
            )
            .await
            .expect("the shell stream hung")
            .expect("read failed")
            .expect("the connection closed before the terminal frame");
            match message {
                ToolMsg::Event { text } => events.push(text),
                ToolMsg::Spilled {
                    file,
                    preview,
                    exit_code,
                } => {
                    assert_eq!(exit_code, Some(0));
                    assert!(file.starts_with("tool-output/"), "{file}");
                    assert!(
                        preview.len() <= bosun_common::tool::SPILLED_PREVIEW_BYTES + 64, /* the ellipsis suffix */
                        "preview stays short"
                    );
                    terminal = Some((file, preview));
                }
                other => panic!("unexpected frame in a spilled shell stream: {other:?}"),
            }
        }
        let (file, preview) = terminal.unwrap();
        assert!(
            preview.contains("line-1"),
            "the preview starts with the output's head: {preview}"
        );
        let full = tokio::fs::read_to_string(root.path().join("s1").join(&file))
            .await
            .expect("the spilled file exists under the session's working copy");
        assert!(full.contains("line-1"));
        assert!(
            full.contains("line-2001"),
            "the file holds the whole output"
        );
        // The live frames stop at the spill byte budget: only the head
        // reaches the control plane, so the model's context stays bounded
        // while the run streams. The spilled file holds what the live stream
        // cut off.
        let forwarded: usize = events.iter().map(String::len).sum();
        assert!(
            forwarded <= bosun_common::tool::SPILL_BYTE_LIMIT + 64,
            "live forwarding stops at the spill budget"
        );
        assert!(
            forwarded < full.len(),
            "live forwarding is capped below the file"
        );

        relay.abort();
    }

    /// An oversized `file_read` is refused rather than spilled: the executor
    /// caps each read to the inline budget, and a read over it comes back as
    /// an error instead of a copy of the file written into the working copy.
    #[tokio::test]
    async fn a_large_file_read_is_refused_instead_of_spilled() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::create_dir_all(root.join("s1")).await.unwrap();
        let big = "x".repeat(bosun_common::tool::SPILL_BYTE_LIMIT + 1);
        tokio::fs::write(root.join("s1/big.txt"), &big)
            .await
            .unwrap();

        let manager = manager_with(root, &["s1"]).await;
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let message = typed_call(
            &cp_tunnel,
            "s1",
            "run-big",
            "file_read",
            json!({ "path": "big.txt" }),
        )
        .await;
        let ToolMsg::Error { message: _ } = message else {
            panic!("a large file read must be refused: {message:?}");
        };

        // Nothing was spilled: no file was written to the working copy.
        assert!(
            !root.join("s1/tool-output").exists(),
            "a refused read must not write a spilled file"
        );

        // A ranged read still answers inline as a bounded result.
        tokio::fs::write(root.join("s1/lines.txt"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        let message = typed_call(
            &cp_tunnel,
            "s1",
            "run-window",
            "file_read",
            json!({ "path": "lines.txt", "offset": 2, "limit": 1 }),
        )
        .await;
        let ToolMsg::Result { content } = message else {
            panic!("a ranged read must return a result: {message:?}");
        };
        assert_eq!(content["content"], "two\n");

        relay.abort();
    }

    /// Dropping the control plane's end of a streaming shell connection kills
    /// the shell's process group and empties the running map, exactly like an
    /// aborted client under the old HTTP transport.
    #[tokio::test]
    async fn dropping_the_connection_kills_the_shell() {
        #[cfg(unix)]
        {
            let root = tempdir().unwrap();
            let manager = manager_with(root.path(), &["s1"]).await;
            let executor = manager.executor("s1").expect("the session's executor");
            let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
            let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

            let mut conn = cp_tunnel.open("s1").await.expect("open for s1");
            let pid = {
                write_tool_frame(
                    &mut conn,
                    &ToolOp::Call {
                        run_id: "run-1".into(),
                        tool: "shell".into(),
                        args: json!({ "command": "sleep 64" }),
                    },
                )
                .await
                .unwrap();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    // The run is registered before the shell is up, with pid 0;
                    // wait for the owner task to publish the real pid.
                    let pid = executor
                        .running
                        .read()
                        .await
                        .get("run-1")
                        .map(|shell| shell.pid)
                        .unwrap_or(0);
                    if pid > 0 {
                        break pid;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        panic!("the shell never published its pid");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };

            // Dropping the connection closes it; the relay's stream guard
            // must kill the shell and empty the running map.
            drop(conn);

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let empty = executor.running.read().await.is_empty();
                let alive = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if empty && !alive {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell survived the dropped connection");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            relay.abort();
        }
    }

    /// A cancel operation on its own connection kills the streaming shell
    /// named by the run id.
    #[tokio::test]
    async fn a_cancel_operation_kills_the_streaming_shell() {
        #[cfg(unix)]
        {
            let root = tempdir().unwrap();
            let manager = manager_with(root.path(), &["s1"]).await;
            let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
            let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

            let mut stream_conn = cp_tunnel.open("s1").await.expect("open for s1");
            write_tool_frame(
                &mut stream_conn,
                &ToolOp::Call {
                    run_id: "run-cancel".into(),
                    tool: "shell".into(),
                    args: json!({ "command": "sleep 65" }),
                },
            )
            .await
            .unwrap();

            let mut cancel_conn = cp_tunnel.open("s1").await.expect("open for s1");
            write_tool_frame(
                &mut cancel_conn,
                &ToolOp::Cancel {
                    run_id: "run-cancel".into(),
                },
            )
            .await
            .unwrap();
            let ack = read_reply(&mut cancel_conn).await;
            assert!(
                matches!(ack, ToolMsg::Ack),
                "cancel answers with ack: {ack:?}"
            );

            // The streaming connection ends with a killed-run done code.
            let done = read_reply(&mut stream_conn).await;
            assert!(matches!(done, ToolMsg::Done { exit_code: -1 }));

            relay.abort();
        }
    }

    /// A read-only permission update reaches the session's executor and gates
    /// the next dispatches.
    #[tokio::test]
    async fn a_set_permission_operation_gates_the_session() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        let (cp_tunnel, node_tunnel, opens) = tunnel_pair();
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let mut conn = cp_tunnel.open("s1").await.expect("open for s1");
        write_tool_frame(
            &mut conn,
            &ToolOp::SetPermission {
                permission: Permission::ReadOnly,
            },
        )
        .await
        .unwrap();
        let ack = read_reply(&mut conn).await;
        assert!(
            matches!(ack, ToolMsg::Ack),
            "set_permission answers with ack: {ack:?}"
        );

        let reply = typed_call(
            &cp_tunnel,
            "s1",
            "run-2",
            "shell",
            json!({ "command": "echo hi" }),
        )
        .await;
        let ToolMsg::Error { message } = reply else {
            panic!("read-only must refuse shell through the relay: {reply:?}");
        };
        assert!(
            message.contains("read-write"),
            "the refusal names the permission"
        );

        relay.abort();
    }

    /// What the control-plane stub does to its first tunneled connection, to
    /// provoke a reconnect.
    #[derive(Clone, Copy)]
    enum FirstTunnelAction {
        /// Drop the connection like a network failure.
        Drop,
        /// Write a frame with an unknown type: a protocol violation.
        Violate,
    }

    #[derive(Clone)]
    struct FakeCpState {
        connections: Arc<AtomicUsize>,
        latest: Arc<Mutex<Option<Tunnel>>>,
        first: FirstTunnelAction,
    }

    /// A control plane that upgrades one tunnel per node connection, kills
    /// the first one, and keeps the newest surviving one for the test to open
    /// logical connections on.
    async fn fake_control_plane(first: FirstTunnelAction) -> (FakeCpState, std::net::SocketAddr) {
        async fn serve_tunnel(
            State(state): State<FakeCpState>,
            AxumPath(_node): AxumPath<String>,
            mut req: Request<Body>,
        ) -> Response {
            if !wants_tunnel_upgrade(&req) {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let Some(upgrade) = req.extensions_mut().remove::<hyper::upgrade::OnUpgrade>() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            tokio::spawn(async move {
                let Ok(stream) = upgrade.await else {
                    return;
                };
                let index = state.connections.fetch_add(1, Ordering::SeqCst);
                let stream = TokioIo::new(stream);
                if index == 0 {
                    match state.first {
                        FirstTunnelAction::Drop => drop(stream),
                        FirstTunnelAction::Violate => {
                            let mut stream = stream;
                            // A header with an unknown type byte.
                            let _ = stream
                                .write_all(&[0x7f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                                .await;
                            drop(stream);
                        }
                    }
                    return;
                }
                let (tunnel, _opens) = Tunnel::new(stream);
                *state.latest.lock().unwrap() = Some(tunnel);
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(axum::http::header::CONNECTION, "upgrade")
                .header(axum::http::header::UPGRADE, "bosun-tunnel")
                .body(Body::empty())
                .unwrap_or_else(|error| {
                    warn!(error = %error, "failed to build the 101 response");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })
        }

        let state = FakeCpState {
            connections: Arc::new(AtomicUsize::new(0)),
            latest: Arc::new(Mutex::new(None)),
            first,
        };
        let app = Router::new()
            .route("/tunnel/node/{node}", get(serve_tunnel))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (state, addr)
    }

    fn wants_tunnel_upgrade(req: &Request<Body>) -> bool {
        let connection = req
            .headers()
            .get(axum::http::header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let upgrade = req
            .headers()
            .get(axum::http::header::UPGRADE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        connection
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            && upgrade.eq_ignore_ascii_case("bosun-tunnel")
    }

    /// A dropped tunnel does not touch the sessions' executors: the node
    /// reconnects its one tunnel and tool calls reach the sessions again over
    /// it.
    #[tokio::test]
    async fn a_dropped_tunnel_reconnects_and_restores_the_node_sessions() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        tokio::fs::write(root.path().join("s1/marker.txt"), "SRV1")
            .await
            .unwrap();
        let (state, addr) = fake_control_plane(FirstTunnelAction::Drop).await;
        let base = format!("http://127.0.0.1:{}", addr.port());
        let task = tokio::spawn(run_node_tunnel(base, "node-1".into(), manager, None));

        wait_until("the node to reconnect after the drop", || {
            state.connections.load(Ordering::SeqCst) >= 2 && state.latest.lock().unwrap().is_some()
        })
        .await;
        let tunnel = state
            .latest
            .lock()
            .unwrap()
            .clone()
            .expect("the reconnected tunnel");

        let reply = typed_call(
            &tunnel,
            "s1",
            "run-1",
            "file_read",
            json!({ "path": "marker.txt" }),
        )
        .await;
        let ToolMsg::Result { content } = reply else {
            panic!("the call after the reconnect must return a result");
        };
        assert_eq!(content["content"], "SRV1");

        task.abort();
    }

    /// A protocol violation tears the node tunnel down and the node
    /// reconnects, restoring every session at once.
    #[tokio::test]
    async fn a_protocol_violation_tears_the_tunnel_down_and_the_node_reconnects() {
        let root = tempdir().unwrap();
        let manager = manager_with(root.path(), &["s1"]).await;
        tokio::fs::write(root.path().join("s1/marker.txt"), "SRV1")
            .await
            .unwrap();
        let (state, addr) = fake_control_plane(FirstTunnelAction::Violate).await;
        let base = format!("http://127.0.0.1:{}", addr.port());
        let task = tokio::spawn(run_node_tunnel(base, "node-1".into(), manager, None));

        wait_until("the node to reconnect after the violation", || {
            state.connections.load(Ordering::SeqCst) >= 2 && state.latest.lock().unwrap().is_some()
        })
        .await;
        let tunnel = state
            .latest
            .lock()
            .unwrap()
            .clone()
            .expect("the reconnected tunnel");

        let reply = typed_call(
            &tunnel,
            "s1",
            "run-1",
            "file_read",
            json!({ "path": "marker.txt" }),
        )
        .await;
        let ToolMsg::Result { content } = reply else {
            panic!("the call after the reconnect must return a result");
        };
        assert_eq!(content["content"], "SRV1");

        task.abort();
    }
}
