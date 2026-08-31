//! Tool calls over the session tunnel. Each call opens a fresh logical
//! connection on the session's tunnel and speaks HTTP/1.1 to the node's
//! executor, which the node relay dials on loopback.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use bosun_agent::agent_loop::ToolError;
use bosun_agent::agent_loop::ToolExecutor;
use bosun_agent::agent_loop::ToolOutcome;
use bosun_agent::sse::sse_stream;
use bosun_common::session::Permission;
use bosun_common::tool::ToolDelta;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::Limited;
use hyper::Request;
use hyper::client::conn::http1;
use hyper::header;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

use crate::tunnel::TunnelError;
use crate::tunnel::TunnelRegistry;

/// The executor origin. Hyper writes only the path into the tunnel, so the
/// authority never reaches the node.
const EXECUTOR_BASE: &str = "http://executor";
/// Cap on the body read for a non-2xx response, which is only logged.
const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// Cap on a JSON tool result body.
const JSON_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Dispatches tool calls to the node's executor over the session tunnel.
pub struct TunnelToolExecutor {
    pub tunnels: Arc<TunnelRegistry>,
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
        Box::pin(async move {
            let outcome = call_tool(&tunnels, &session_id, &run_id, &name, &args, &delta).await?;
            Ok(outcome)
        })
    }

    fn cancel(
        &self,
        session_id: String,
        run_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send>> {
        let tunnels = self.tunnels.clone();
        Box::pin(async move {
            if let Err(error) = cancel_tool(&tunnels, &session_id, &run_id).await {
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
    let mut sender = open_connection(tunnels, session_id).await?;
    let body = serde_json::to_vec(&json!({ "run_id": run_id, "args": args }))
        .context("failed to serialize the tool request")?;
    let response = post_json(&mut sender, &format!("/tool/{name}"), body.into(), "tool").await?;

    let status = response.status();
    if !status.is_success() {
        let body = read_bounded_body(response.into_body(), ERROR_BODY_LIMIT).await?;
        let text = String::from_utf8_lossy(&body).into_owned();
        warn!(
            msg = "executor refused the tool call",
            session_id = %session_id,
            tool = %name,
            run_id = %run_id,
            status = %status
        );
        return Ok(ToolOutcome {
            content: json!({ "error": text }),
            is_error: true,
        });
    }

    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.contains("text/event-stream"));

    if is_sse {
        stream_outcome(response.into_body(), session_id, run_id, name, delta).await
    } else {
        json_outcome(response.into_body(), name).await
    }
}

async fn cancel_tool(
    tunnels: &TunnelRegistry,
    session_id: &str,
    run_id: &str,
) -> anyhow::Result<()> {
    let mut sender = open_connection(tunnels, session_id).await?;
    let _ = post_json(
        &mut sender,
        &format!("/tool/{run_id}/cancel"),
        Bytes::new(),
        "cancel",
    )
    .await?;
    Ok(())
}

/// Forwards a permission change to the node's executor. The caller treats
/// transport errors as best-effort: the loop's tool schema gates the
/// permission too.
pub async fn set_executor_permission(
    tunnels: &TunnelRegistry,
    session_id: &str,
    permission: Permission,
) -> anyhow::Result<()> {
    let mut sender = open_connection(tunnels, session_id).await?;
    let body = serde_json::to_vec(&json!({ "permission": permission }))
        .context("failed to serialize the permission request")?;
    let response = post_json(&mut sender, "/permission", body.into(), "permission").await?;
    if !response.status().is_success() {
        let body = read_bounded_body(response.into_body(), ERROR_BODY_LIMIT).await?;
        anyhow::bail!(
            "executor refused the permission change: {}",
            String::from_utf8_lossy(&body)
        );
    }
    Ok(())
}

/// Sends a JSON POST to the executor over the session tunnel.
async fn post_json(
    sender: &mut http1::SendRequest<Full<Bytes>>,
    path: &str,
    body: Bytes,
    what: &str,
) -> anyhow::Result<hyper::Response<hyper::body::Incoming>> {
    let uri: hyper::Uri = format!("{EXECUTOR_BASE}{path}")
        .parse()
        .with_context(|| format!("failed to build the {what} request URI"))?;
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(body))
        .with_context(|| format!("failed to build the {what} request"))?;
    sender
        .send_request(request)
        .await
        .with_context(|| format!("failed to send the {what} request"))
}

/// Opens a logical connection on the session's tunnel and handshakes HTTP/1.1
/// over it, so the node relay can forward the request to the executor.
async fn open_connection(
    tunnels: &TunnelRegistry,
    session_id: &str,
) -> anyhow::Result<http1::SendRequest<Full<Bytes>>> {
    let stream = match tunnels.open(session_id).await {
        Ok(stream) => stream,
        Err(TunnelError::NoTunnel { .. }) | Err(TunnelError::TunnelClosed { .. }) => {
            anyhow::bail!("session {session_id} has no live tunnel");
        }
    };
    let (sender, conn) = http1::handshake(TokioIo::new(stream))
        .await
        .context("failed to handshake with the session tunnel")?;
    tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            debug!(error = %error, "tool tunnel connection ended with an error");
        }
    });
    Ok(sender)
}

/// Reads the whole body, failing once it exceeds `limit`.
async fn read_bounded_body(body: hyper::body::Incoming, limit: usize) -> anyhow::Result<Bytes> {
    let collected = Limited::new(body, limit)
        .collect()
        .await
        .map_err(|error| {
            if error.is::<http_body_util::LengthLimitError>() {
                anyhow::anyhow!("executor response body exceeded the {limit} byte limit")
            } else {
                anyhow::anyhow!("{error}")
            }
        })
        .context("failed to read the executor response body")?;
    Ok(collected.to_bytes())
}

/// Consumes a streaming shell response: `out` events forward deltas, `done`
/// carries the exit code, and the accumulated output is the tool result.
async fn stream_outcome(
    body: hyper::body::Incoming,
    session_id: &str,
    run_id: &str,
    name: &str,
    delta: &mpsc::UnboundedSender<ToolDelta>,
) -> anyhow::Result<ToolOutcome> {
    let mut events = Box::pin(sse_stream(Box::pin(body.into_data_stream())));
    let mut output = String::new();
    let mut exit_code = 0;
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => match event.event.as_deref() {
                Some("out") => {
                    output.push_str(&event.data);
                    let _ = delta.send(ToolDelta { text: event.data });
                }
                Some("done") => {
                    exit_code = event.data.parse::<i32>().unwrap_or(0);
                }
                _ => {}
            },
            Err(error) => {
                warn!(
                    msg = "tool stream failed",
                    session_id = %session_id,
                    tool = %name,
                    run_id = %run_id,
                    error = %error
                );
                return Err(anyhow::Error::from(error)
                    .context(format!("failed to stream tool {name} output")));
            }
        }
    }
    Ok(ToolOutcome {
        content: json!({ "output": output, "exit_code": exit_code }),
        is_error: exit_code != 0,
    })
}

/// Parses a non-streaming JSON tool result.
async fn json_outcome(body: hyper::body::Incoming, name: &str) -> anyhow::Result<ToolOutcome> {
    let bytes = read_bounded_body(body, JSON_BODY_LIMIT).await?;
    let content: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("executor returned non-JSON content for tool {name}"))?;
    Ok(ToolOutcome {
        content,
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::post;
    use bosun_common::tunnel::Tunnel;
    use futures_util::stream;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;

    use super::*;

    /// A stub executor implementing the tool protocol over plain TCP, with the
    /// session tunnel relayed to it like a node would.
    async fn stub_executor() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let cancels: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/tool/shell", post(shell_tool))
            .route("/tool/echo", post(echo_tool))
            .route("/tool/forbidden", post(forbidden_tool))
            .route("/tool/{run_id}/cancel", post(handle_cancel))
            .route("/permission", post(handle_permission))
            .with_state(cancels.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, cancels)
    }

    async fn shell_tool(body: Bytes) -> Response {
        let exit_code = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|body| body["args"]["exit_code"].as_i64())
            .unwrap_or(0);
        let done = format!("event: done\ndata: {exit_code}\n\n");
        let body = Body::from_stream(stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from_static(
                b"event: out\ndata: building...\n\n",
            )),
            Ok::<_, std::convert::Infallible>(Bytes::from_static(
                b"event: out\ndata: finished\n\n",
            )),
            Ok::<_, std::convert::Infallible>(Bytes::from(done)),
        ]));
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
    }

    async fn echo_tool() -> Json<Value> {
        Json(json!({ "echo": "hi" }))
    }

    async fn forbidden_tool() -> Response {
        (StatusCode::FORBIDDEN, "read-only tool refused").into_response()
    }

    async fn handle_cancel(
        State(cancels): State<Arc<Mutex<Vec<String>>>>,
        Path(run_id): Path<String>,
    ) -> Json<Value> {
        cancels
            .lock()
            .unwrap()
            .push(format!("/tool/{run_id}/cancel"));
        Json(json!({}))
    }

    async fn handle_permission(
        State(seen): State<Arc<Mutex<Vec<String>>>>,
        body: axum::body::Bytes,
    ) -> Json<Value> {
        seen.lock()
            .unwrap()
            .push(String::from_utf8_lossy(&body).into_owned());
        Json(json!({}))
    }

    /// Registers a real `TunnelRegistry` for a fresh session whose node side
    /// relays every opened logical connection to the stub executor.
    async fn test_executor(backend: SocketAddr) -> (TunnelToolExecutor, String) {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _) = Tunnel::new(cp_side);
        let (node_tunnel, mut opens) = Tunnel::new(node_side);

        let session_id = uuid::Uuid::new_v4().to_string();
        let tunnels = TunnelRegistry::new();
        tunnels.register(&session_id, cp_tunnel);

        tokio::spawn(async move {
            while let Some(event) = opens.recv().await {
                let tunnel = node_tunnel.clone();
                tokio::spawn(async move {
                    let mut backend = TcpStream::connect(backend).await.unwrap();
                    let mut logical = tunnel.attach(event.conn_id, event.rx).unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut backend, &mut logical).await;
                });
            }
        });

        (
            TunnelToolExecutor {
                tunnels: Arc::new(tunnels),
            },
            session_id,
        )
    }

    #[tokio::test]
    async fn call_dispatches_json_tools() {
        let (backend, _) = stub_executor().await;
        let (executor, session_id) = test_executor(backend).await;
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
        let (backend, _) = stub_executor().await;
        let (executor, session_id) = test_executor(backend).await;
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
    async fn call_reports_non_2xx_as_tool_error_outcome() {
        let (backend, _) = stub_executor().await;
        let (executor, session_id) = test_executor(backend).await;
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
        let (backend, _) = stub_executor().await;
        let (executor, session_id) = test_executor(backend).await;
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
    async fn cancel_posts_to_the_run_id_route() {
        let (backend, cancels) = stub_executor().await;
        let (executor, session_id) = test_executor(backend).await;

        executor.cancel(session_id, "run-9".into()).await.unwrap();

        assert!(
            cancels
                .lock()
                .unwrap()
                .iter()
                .any(|path| path == "/tool/run-9/cancel"),
            "the executor never received the cancel request"
        );
    }

    #[tokio::test]
    async fn set_executor_permission_posts_the_snake_case_permission() {
        let (backend, seen) = stub_executor().await;
        let (executor, session_id) = test_executor(backend).await;

        set_executor_permission(&executor.tunnels, &session_id, Permission::ReadOnly)
            .await
            .unwrap();

        let bodies = seen.lock().unwrap();
        assert!(
            bodies
                .iter()
                .any(|body| body == r#"{"permission":"read_only"}"#),
            "the executor never received the permission body: {bodies:?}"
        );
    }
}
