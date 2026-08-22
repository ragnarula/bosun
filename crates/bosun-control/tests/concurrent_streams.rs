//! Reproduction for: concurrent streams over one session tunnel stall after
//! roughly 800 KiB of a large response. Mirrors the reported deployment: a
//! control plane, a node that dials out over one tunnel, and a client that
//! fetches two URLs through the same tunnel at the same time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::Request as HttpRequest;
use axum::http::StatusCode;
use bosun_common::tunnel::Tunnel;
use bosun_control::api::AppState;
use bosun_control::api::router;
use bosun_control::commands::CommandQueue;
use bosun_control::registry::NodeRegistry;
use bosun_control::tunnel::TunnelRegistry;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

const ASSET_BODY_LEN: usize = 2_738_956;
const ROOT_BODY: &[u8] = b"root page";

/// Serves a request head based on the path: `/assets/index.js` returns a body
/// the size of the reported opencode bundle, everything else a tiny body.
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

/// A control plane whose gateway routes to a session tunnel.
async fn control_plane() -> SocketAddr {
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
    addr
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

/// Waits until a session route answers, then fetches the root and the asset
/// concurrently through the gateway, the way `curl --parallel` does.
#[tokio::test]
async fn concurrent_streams_over_one_tunnel_deliver_both_bodies() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let backend_addr = backend().await;
    let cp_addr = control_plane().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    node_tunnel(cp_addr, &session_id, backend_addr).await;

    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client
            .get(format!("http://{cp_addr}/session/{session_id}/"))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => break,
            _ => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "session route never became live"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    let root = client
        .get(format!("http://{cp_addr}/session/{session_id}/"))
        .send();
    let asset = client
        .get(format!(
            "http://{cp_addr}/session/{session_id}/assets/index.js"
        ))
        .send();

    let (root, asset) = tokio::join!(root, asset);
    let root = root
        .expect("root request failed")
        .error_for_status()
        .unwrap();
    let asset = asset
        .expect("asset request failed")
        .error_for_status()
        .unwrap();

    let (root_len, _) = tokio::join!(root.bytes(), async { ROOT_BODY.to_vec() });
    let root_len = root_len.expect("root body incomplete");
    assert_eq!(root_len.as_ref(), ROOT_BODY, "root body mismatch");

    let asset_len = tokio::time::timeout(Duration::from_secs(10), asset.bytes())
        .await
        .expect("asset download stalled")
        .expect("asset body error");
    assert_eq!(asset_len.len(), ASSET_BODY_LEN, "asset body truncated");
}

/// The reported client speaks HTTP/2 (`curl --parallel --http2`), so its two
/// streams share one HTTP/2 connection to the control plane. The gateway
/// still opens one logical tunnel connection per request; both run over the
/// same session tunnel at once.
#[tokio::test]
async fn concurrent_http2_streams_over_one_tunnel_deliver_both_bodies() {
    let backend_addr = backend().await;
    let cp_addr = control_plane().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    node_tunnel(cp_addr, &session_id, backend_addr).await;

    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    let client: Client<_, http_body_util::Empty<bytes::Bytes>> =
        Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build_http();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client
            .get(
                format!("http://{cp_addr}/session/{session_id}/")
                    .parse()
                    .unwrap(),
            )
            .await
        {
            Ok(response) if response.status().is_success() => break,
            _ => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "session route never became live"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    let root = client.get(
        format!("http://{cp_addr}/session/{session_id}/")
            .parse()
            .unwrap(),
    );
    let asset = client.get(
        format!("http://{cp_addr}/session/{session_id}/assets/index.js")
            .parse()
            .unwrap(),
    );

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
