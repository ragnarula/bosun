use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bosun_common::error::ErrorExt;
use bosun_common::tunnel::Tunnel;
use bosun_common::types::CloneRequest;
use bosun_common::types::CommandResult;
use bosun_common::types::DevRequest;
use bosun_common::types::DirListing;
use bosun_common::types::NodeCommand;
use bosun_common::types::PollRequest;
use bosun_common::types::PollResponse;
use bosun_common::types::SessionInfo;
use bosun_common::types::StopRequest;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use crate::commands::CommandQueue;
use crate::gateway::route as gateway_route;
use crate::registry::NodeHealth;
use crate::registry::NodeRegistry;
use crate::registry::SessionHealth;
use crate::tunnel::TunnelRegistry;

const SPAWN_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("node {node} is not up")]
    NodeNotUp { node: String },

    #[error("node {node} rejected the request: {detail}")]
    NodeRejected { node: String, detail: String },

    #[error("node {node} is unreachable")]
    NodeUnreachable { node: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, text) = match &self {
            ApiError::NodeNotUp { .. } => (StatusCode::BAD_REQUEST, Some(self.to_string())),
            ApiError::NodeRejected { .. } | ApiError::NodeUnreachable { .. } => {
                (StatusCode::BAD_GATEWAY, Some(self.to_string()))
            }
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
        };

        tracing::error!("error: {}", self.display_chain());

        match text {
            Some(text) => (status, text).into_response(),
            None => status.into_response(),
        }
    }
}

pub struct AppState {
    pub registry: Arc<NodeRegistry>,
    pub commands: Arc<CommandQueue>,
    pub tunnels: Arc<TunnelRegistry>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/poll", post(poll))
        .route("/nodes", get(nodes))
        .route("/sessions", get(sessions))
        .route("/clone", post(clone))
        .route("/dev", post(dev))
        .route("/nodes/{name}/dirs", get(dirs))
        .route("/stop", post(stop))
        .route("/tunnel/session/{id}", get(tunnel))
        .fallback(gateway_route)
        .with_state(state)
}

/// The node's one outbound control request: it reports its heartbeat payload,
/// delivers the previous command's result, and takes the next command.
#[instrument(skip(state))]
async fn poll(
    State(state): State<Arc<AppState>>,
    Json(poll): Json<PollRequest>,
) -> Json<PollResponse> {
    state
        .registry
        .upsert(&poll.node_name, &poll.sessions, SystemTime::now());
    if let Some(result) = poll.result {
        state.commands.report(&poll.node_name, result);
    }
    let command = state.commands.next(&poll.node_name).await;
    Json(PollResponse { command })
}

#[instrument(skip(state))]
async fn nodes(State(state): State<Arc<AppState>>) -> Json<Vec<NodeHealth>> {
    Json(state.registry.list(SystemTime::now()))
}

#[instrument(skip(state))]
async fn sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionHealth>> {
    Json(state.registry.sessions(SystemTime::now()))
}

#[instrument(skip(state))]
async fn clone(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CloneRequest>,
) -> Result<Json<SessionHealth>, ApiError> {
    if state.registry.node(&req.node, SystemTime::now()).is_none() {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let command = NodeCommand::Clone {
        id: state.commands.next_id(),
        session_id: session_id.clone(),
        repo_url: req.repo_url.clone(),
        git_ref: req.git_ref.clone(),
    };
    let session = enqueue_and_await(&state, &req.node, command)
        .await
        .and_then(|result| match result {
            CommandResult::Session { session, .. } => Ok(session),
            CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                node: req.node.clone(),
                detail: message,
            }),
            _ => Err(ApiError::Internal(anyhow::anyhow!(
                "node answered clone with a non-session result"
            ))),
        })?;

    state.registry.add_session(&req.node, session.clone());
    info!(session_id = %session.id, node = %req.node, "session cloned");
    Ok(Json(to_health(req.node, session)))
}

#[instrument(skip(state))]
async fn dev(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DevRequest>,
) -> Result<Json<SessionHealth>, ApiError> {
    if state.registry.node(&req.node, SystemTime::now()).is_none() {
        return Err(ApiError::NodeNotUp {
            node: req.node.clone(),
        });
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let command = NodeCommand::Dev {
        id: state.commands.next_id(),
        session_id: session_id.clone(),
        dir: req.dir.clone(),
    };
    let session = enqueue_and_await(&state, &req.node, command)
        .await
        .and_then(|result| match result {
            CommandResult::Session { session, .. } => Ok(session),
            CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                node: req.node.clone(),
                detail: message,
            }),
            _ => Err(ApiError::Internal(anyhow::anyhow!(
                "node answered dev with a non-session result"
            ))),
        })?;

    state.registry.add_session(&req.node, session.clone());
    info!(session_id = %session.id, node = %req.node, dir = %req.dir.display(), "dev session started");
    Ok(Json(to_health(req.node, session)))
}

#[derive(Debug, Deserialize)]
struct DirsQuery {
    path: Option<PathBuf>,
}

#[instrument(skip(state))]
async fn dirs(
    State(state): State<Arc<AppState>>,
    AxumPath(node): AxumPath<String>,
    Query(query): Query<DirsQuery>,
) -> Result<Json<DirListing>, ApiError> {
    if state.registry.node(&node, SystemTime::now()).is_none() {
        return Err(ApiError::NodeNotUp { node });
    }

    let command = NodeCommand::Dirs {
        id: state.commands.next_id(),
        path: query.path,
    };
    let listing =
        enqueue_and_await(&state, &node, command)
            .await
            .and_then(|result| match result {
                CommandResult::Dirs { listing, .. } => Ok(listing),
                CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                    node,
                    detail: message,
                }),
                _ => Err(ApiError::Internal(anyhow::anyhow!(
                    "node answered dirs with a non-listing result"
                ))),
            })?;
    Ok(Json(listing))
}

#[instrument(skip(state))]
async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now();
    let Some((node, _)) = state.registry.session(&req.session_id, now) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    if state.registry.node(&node, now).is_none() {
        return Ok(StatusCode::NO_CONTENT);
    }

    let command = NodeCommand::Stop {
        id: state.commands.next_id(),
        session_id: req.session_id.clone(),
    };
    let node_for_result = node.clone();
    enqueue_and_await(&state, &node, command)
        .await
        .and_then(|result| match result {
            CommandResult::Stop { .. } => Ok(()),
            CommandResult::Error { message, .. } => Err(ApiError::NodeRejected {
                node: node_for_result,
                detail: message,
            }),
            _ => Err(ApiError::Internal(anyhow::anyhow!(
                "node answered stop with a non-stop result"
            ))),
        })?;

    state.tunnels.unregister(&req.session_id);
    state.registry.remove_session(&node, &req.session_id);
    info!(session_id = %req.session_id, node = %node, "session stopped");
    Ok(StatusCode::NO_CONTENT)
}

/// Enqueues a command for the node and waits for its result, delivered in the
/// node's next poll.
async fn enqueue_and_await(
    state: &Arc<AppState>,
    node: &str,
    command: NodeCommand,
) -> Result<CommandResult, ApiError> {
    let (reply, reply_rx) = oneshot::channel();
    state.commands.enqueue(node, command, reply);
    match tokio::time::timeout(Duration::from_secs(SPAWN_TIMEOUT_SECS), reply_rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(ApiError::Internal(anyhow::anyhow!(
            "node {node} dropped the command reply"
        ))),
        Err(_) => Err(ApiError::NodeUnreachable {
            node: node.to_string(),
        }),
    }
}

/// Accepts a node's outbound tunnel for a session. The node keeps the
/// connection; the gateway opens logical connections on it per client.
async fn tunnel(
    State(state): State<Arc<AppState>>,
    AxumPath(session_id): AxumPath<String>,
    mut req: Request<Body>,
) -> Response {
    if !wants_tunnel_upgrade(&req) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let tunnels = state.tunnels.clone();
    tokio::spawn(async move {
        let stream = match upgrade.await {
            Ok(stream) => stream,
            Err(error) => {
                warn!(session_id = %session_id, error = %error, "session tunnel upgrade failed");
                return;
            }
        };
        let (tunnel, _opens) = Tunnel::new(TokioIo::new(stream));
        tunnels.register(&session_id, tunnel.clone());
        tunnel.closed().await;
        tunnels.unregister(&session_id);
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "bosun-tunnel")
        .body(Body::empty())
        .unwrap_or_else(|error| {
            warn!(error = %error, "failed to build the 101 response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

fn wants_tunnel_upgrade(req: &Request<Body>) -> bool {
    let connection = req
        .headers()
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        && upgrade.eq_ignore_ascii_case("bosun-tunnel")
}

fn to_health(node: String, session: SessionInfo) -> SessionHealth {
    SessionHealth {
        id: session.id,
        node,
        repo_url: session.repo_url,
        git_ref: session.git_ref,
        dir: session.dir,
        status: session.status,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;

    use super::*;

    #[test]
    fn wants_tunnel_upgrade_accepts_the_bosun_tunnel_protocol() {
        let req = HttpRequest::builder()
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "bosun-tunnel")
            .body(Body::empty())
            .unwrap();
        assert!(wants_tunnel_upgrade(&req));

        let plain = HttpRequest::builder().body(Body::empty()).unwrap();
        assert!(!wants_tunnel_upgrade(&plain));

        let ws = HttpRequest::builder()
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .body(Body::empty())
            .unwrap();
        assert!(!wants_tunnel_upgrade(&ws));
    }

    /// Boots the full API with a fake node: one side of an in-memory tunnel
    /// registered for `session_id`, whose relay dials `backend`.
    async fn test_server(backend: SocketAddr) -> (SocketAddr, String) {
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

        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(tunnels),
        });
        let app = router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, session_id)
    }

    async fn stub_backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    use tokio::io::AsyncWriteExt;
                    let mut buf = [0u8; 4096];
                    let mut read = 0;
                    while let Ok(n) = stream.read(&mut buf[read..]).await {
                        if n == 0 {
                            break;
                        }
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn tunnel_route_carries_the_upgrade() {
        let backend_addr = stub_backend().await;
        let session_id = uuid::Uuid::new_v4().to_string();

        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
        });
        let app = router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            http1::handshake::<_, http_body_util::Empty<bytes::Bytes>>(TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let request = HttpRequest::builder()
            .method("GET")
            .uri(format!("/tunnel/session/{session_id}"))
            .header(header::HOST, addr.to_string())
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "bosun-tunnel")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        // The test owns the node side of the upgraded connection: relay every
        // opened logical connection to the stub backend.
        let upgraded = hyper::upgrade::on(response).await.unwrap();
        let (node_tunnel, mut opens) = Tunnel::new(TokioIo::new(upgraded));
        tokio::spawn(async move {
            while let Some(event) = opens.recv().await {
                let tunnel = node_tunnel.clone();
                tokio::spawn(async move {
                    let mut backend = TcpStream::connect(backend_addr).await.unwrap();
                    let mut logical = tunnel.attach(event.conn_id, event.rx).unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut backend, &mut logical).await;
                });
            }
        });

        // The control plane registers the tunnel after the 101; retry until the
        // session is reachable through the gateway.
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match client
                .get(format!("http://{addr}/global/health"))
                .header("host", format!("{session_id}.bosun.on.21cs.biz"))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => break,
                _ => {
                    if tokio::time::Instant::now() >= deadline {
                        panic!("session never became reachable through the tunnel");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    /// Answers with the request line so a test can see exactly what the
    /// gateway forwarded to the backend.
    async fn request_line_backend() -> SocketAddr {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut read = 0;
                    while read < buf.len() {
                        match stream.read(&mut buf[read..]).await {
                            Ok(0) => return,
                            Ok(n) => read += n,
                            Err(_) => return,
                        }
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..read]);
                    let request_line = head.lines().next().unwrap_or("");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                        request_line.len(),
                        request_line
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn session_route_serves_through_the_tunnel() {
        let backend = stub_backend().await;
        let (addr, session_id) = test_server(backend).await;
        let client = reqwest::Client::new();
        let text = client
            .get(format!("http://{addr}/global/health"))
            .header("host", format!("{session_id}.bosun.on.21cs.biz"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(text, "ok");
    }

    /// opencode's own API uses `/session` and `/session/<id>` paths. The
    /// gateway must forward them unchanged rather than treating the path as a
    /// route prefix.
    #[tokio::test]
    async fn session_api_paths_are_forwarded_unchanged() {
        let backend_addr = request_line_backend().await;
        let (addr, session_id) = test_server(backend_addr).await;
        let client = reqwest::Client::new();
        let text = client
            .get(format!("http://{addr}/session/other-session/global/health"))
            .header("host", format!("{session_id}.bosun.on.21cs.biz"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(text, "GET /session/other-session/global/health HTTP/1.1");
    }

    #[tokio::test]
    async fn subdomain_host_routes_to_the_session() {
        let backend = stub_backend().await;
        let (addr, session_id) = test_server(backend).await;
        let client = reqwest::Client::new();
        let text = client
            .get(format!("http://{addr}/assets/index-abc.js"))
            .header("host", format!("{session_id}.bosun.on.21cs.biz"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(text, "ok");
    }

    #[tokio::test]
    async fn apex_host_does_not_route_root_paths() {
        let (addr, _) = test_server(stub_backend().await).await;
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/assets/index-abc.js"))
            .header("host", "bosun.on.21cs.biz")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn session_host_without_a_tunnel_returns_not_found() {
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
            tunnels: Arc::new(TunnelRegistry::new()),
        });
        let app = router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/global/health"))
            .header("host", "ghost.bosun.on.21cs.biz")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_session_host_returns_not_found() {
        let (addr, _) = test_server(stub_backend().await).await;
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn streams_the_response_body_through() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = backend.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
                      data: one\n\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            stream.write_all(b"data: two\n\n").await.unwrap();
        });

        let (addr, session_id) = test_server(backend_addr).await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                format!(
                    "GET /global/health HTTP/1.1\r\nHost: {session_id}.bosun.on.21cs.biz\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut seen = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = client.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&seen);
            if text.contains("data: one") && text.contains("data: two") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("data: one"), "first event missing");
        assert!(text.contains("data: two"), "second event missing");
    }

    #[tokio::test]
    async fn tunnels_websocket_upgrades() {
        use futures_util::SinkExt;
        use futures_util::StreamExt;

        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = backend.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                ws.send(message).await.unwrap();
            }
        });

        let (addr, session_id) = test_server(backend_addr).await;

        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{addr}/pty/1/connect")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            header::HOST,
            format!("{session_id}.bosun.on.21cs.biz").parse().unwrap(),
        );
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text("ping".into()))
            .await
            .unwrap();
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(
            message,
            tokio_tungstenite::tungstenite::Message::Text("ping".into())
        );
    }
}
