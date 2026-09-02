//! Reproduction for: concurrent streams over one session tunnel stall after
//! roughly 800 KiB of a large response. Mirrors the reported deployment: a
//! control plane, a node that dials out over one tunnel, and two requests
//! over logical connections on the same tunnel at the same time.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::Request as HttpRequest;
use axum::http::StatusCode;
use bosun_common::tunnel::Tunnel;
use bosun_control::api::AppState;
use bosun_control::api::router;
use bosun_control::commands::CommandQueue;
use bosun_control::loops::AgentRegistry;
use bosun_control::registry::NodeRegistry;
use bosun_control::tunnel::TunnelRegistry;
use bosun_store::store::Store;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

const ASSET_BODY_LEN: usize = 2_738_956;
const ROOT_BODY: &[u8] = b"root page";

/// Serves a request head based on the path: `/assets/index.js` returns a body
/// the size of a large web asset bundle, everything else a tiny body.
async fn backend() -> SocketAddr {
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
                let is_asset = head.contains("/assets/index.js");
                if is_asset {
                    let header =
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {ASSET_BODY_LEN}\r\n\r\n");
                    let _ = stream.write_all(header.as_bytes()).await;
                    let body = vec![b'x'; ASSET_BODY_LEN];
                    let _ = stream.write_all(&body).await;
                } else {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
                        ROOT_BODY.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(ROOT_BODY).await;
                }
            });
        }
    });
    addr
}

/// A control plane with a session tunnel registry. Returns the store too, so
/// a test can register the tunnel's session up front.
async fn control_plane() -> (SocketAddr, Store, Arc<TunnelRegistry>) {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let store = Store::open(&dir.path().join("sessions.db")).unwrap();
    let tunnels = Arc::new(TunnelRegistry::new());
    let state = Arc::new(AppState {
        registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
        commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
        tunnels: tunnels.clone(),
        store: store.clone(),
        loops: Arc::new(AgentRegistry::new(
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )),
        providers: HashMap::new(),
        personas: HashMap::new(),
        default_persona: None,
        skills_dir: None,
    });
    let app = router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, store, tunnels)
}

/// Registers the session in the store, as a clone/dev request would, so the
/// tunnel endpoint accepts its tunnel.
async fn register_session(store: &Store, session_id: &str) {
    use bosun_common::session::Permission;
    use bosun_common::session::Session;
    use bosun_common::session::SessionState;

    store
        .create_session(&Session {
            id: session_id.to_string(),
            node: "node-1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/work".into(),
            model: "mock-model".into(),
            persona: None,
            parent_id: None,
            owner_id: session_id.to_string(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            state: SessionState::WaitingForInput,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: None,
        })
        .await
        .unwrap();
}

/// The node side: opens the outbound tunnel and relays every logical
/// connection to `backend`, the way `bosun-node/src/tunnel.rs` does.
async fn node_tunnel(cp_addr: SocketAddr, session_id: &str, backend: SocketAddr) {
    let stream = TcpStream::connect(cp_addr).await.unwrap();
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
        .header("host", cp_addr.to_string())
        .header("connection", "upgrade")
        .header("upgrade", "bosun-tunnel")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let upgraded = hyper::upgrade::on(response).await.unwrap();

    let (tunnel, mut opens) = Tunnel::new(TokioIo::new(upgraded));
    tokio::spawn(async move {
        while let Some(event) = opens.recv().await {
            let tunnel = tunnel.clone();
            tokio::spawn(async move {
                let Ok(mut backend) = TcpStream::connect(backend).await else {
                    return;
                };
                let Some(mut logical) = tunnel.attach(event.conn_id, event.rx) else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut backend, &mut logical).await;
            });
        }
    });
}

/// Opens a logical connection on the session's tunnel and sends one GET over
/// HTTP/1.1, the way the control plane's tool transport does. Returns `None`
/// while the tunnel is not yet registered.
async fn tunnel_get(
    tunnels: &TunnelRegistry,
    session_id: &str,
    path: &str,
) -> Option<hyper::Response<hyper::body::Incoming>> {
    let stream = tunnels.open(session_id).await.ok()?;
    let (mut sender, conn) =
        http1::handshake::<_, http_body_util::Empty<bytes::Bytes>>(TokioIo::new(stream))
            .await
            .ok()?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let request = HttpRequest::builder()
        .method("GET")
        .uri(path)
        .header("host", "executor")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();
    sender.send_request(request).await.ok()
}

/// Waits until the tunnel is live, then fetches the root and the asset
/// concurrently over two logical connections on one tunnel.
#[tokio::test]
async fn concurrent_streams_over_one_tunnel_deliver_both_bodies() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let backend_addr = backend().await;
    let (cp_addr, store, tunnels) = control_plane().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    register_session(&store, &session_id).await;
    node_tunnel(cp_addr, &session_id, backend_addr).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel_get(&tunnels, &session_id, "/index.html").await {
            Some(response) if response.status().is_success() => break,
            _ => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "session route never became live"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    let root = tunnel_get(&tunnels, &session_id, "/index.html");
    let asset = tunnel_get(&tunnels, &session_id, "/assets/index.js");

    let (root, asset) = tokio::join!(root, asset);
    let root = root.expect("root request failed");
    let asset = asset.expect("asset request failed");
    assert_eq!(root.status(), StatusCode::OK);
    assert_eq!(asset.status(), StatusCode::OK);

    let root_len = tokio::time::timeout(Duration::from_secs(10), body_bytes(root))
        .await
        .expect("root body stalled");
    assert_eq!(root_len, ROOT_BODY, "root body mismatch");

    let asset_len = tokio::time::timeout(Duration::from_secs(10), body_bytes(asset))
        .await
        .expect("asset download stalled");
    assert_eq!(asset_len.len(), ASSET_BODY_LEN, "asset body truncated");
}

async fn body_bytes(response: hyper::Response<hyper::body::Incoming>) -> Vec<u8> {
    use http_body_util::BodyExt;
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}
