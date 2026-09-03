use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::tunnel::OpenEvent;
use bosun_common::tunnel::Tunnel;
use bytes::Bytes;
use http_body_util::Empty;
use hyper::client::conn::http1;
use hyper::header;
use hyper::http::StatusCode;
use hyper::http::Uri;
use hyper::upgrade::Upgraded;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tower_service::Service as _;
use tracing::debug;
use tracing::warn;

use crate::manager::NodeManager;

const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// Keeps the node's one outbound tunnel to the control plane open. On any
/// failure the connection is re-established after a short delay; the node's
/// executors keep running, so a reconnect restores every session's tool
/// calls at once. Runs until the node exits, independent of how many sessions
/// the node hosts.
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
/// the relay dials that session's executor port.
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
    let Some(executor_port) = manager.executor_port(&event.session_id) else {
        debug!(
            conn_id = event.conn_id,
            session_id = %event.session_id,
            "no executor for the session; closing the connection"
        );
        let _ = tunnel.attach(event.conn_id, event.rx);
        return;
    };
    let mut local = match TcpStream::connect(("127.0.0.1", executor_port)).await {
        Ok(stream) => stream,
        Err(error) => {
            debug!(
                conn_id = event.conn_id,
                session_id = %event.session_id,
                error = %error,
                "failed to dial the session's executor; closing the connection"
            );
            let _ = tunnel.attach(event.conn_id, event.rx);
            return;
        }
    };
    let Some(mut logical) = tunnel.attach(event.conn_id, event.rx) else {
        return;
    };
    if let Err(error) = copy_bidirectional(&mut local, &mut logical).await {
        debug!(conn_id = event.conn_id, error = %error, "tunnel relay closed with an error");
    }
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
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path;
    use axum::extract::State;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::get;
    use tempfile::tempdir;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;

    /// A manager holding the given sessions with no real executors, so the
    /// relay dials the stub executors the tests bind instead.
    fn manager_with(sessions: &[(&str, u16)]) -> Arc<NodeManager> {
        let work = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            work.path().to_path_buf(),
            vec![],
            "http://127.0.0.1:1".into(),
            None,
        ));
        for (id, port) in sessions {
            manager.add_session_for_test(id, *port);
        }
        manager
    }

    /// A stub executor: answers every connection with `marker`, which names
    /// it, and records everything the connection sends. Returns its address
    /// and the recorded payloads.
    async fn stub_executor(marker: &'static [u8]) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let received_for_task = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let received = received_for_task.clone();
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.into_split();
                    let _ = write.write_all(marker).await;
                    let mut buf = vec![0u8; 1024];
                    loop {
                        match read.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => received.lock().unwrap().push(buf[..n].to_vec()),
                        }
                    }
                });
            }
        });
        (addr, received)
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

    /// The one node tunnel carries logical connections addressed to different
    /// sessions, and the relay dials each session's own executor, so two
    /// sessions' tool calls run concurrently over the same tunnel.
    #[tokio::test]
    async fn one_node_tunnel_relays_each_session_to_its_own_executor() {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _cp_opens) = Tunnel::new(cp_side);
        let (node_tunnel, opens) = Tunnel::new(node_side);
        let (srv1_addr, srv1_seen) = stub_executor(b"SRV1").await;
        let (srv2_addr, srv2_seen) = stub_executor(b"SRV2").await;
        let manager = manager_with(&[("s1", srv1_addr.port()), ("s2", srv2_addr.port())]);
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let mut conn1 = cp_tunnel.open("s1").await.expect("open for s1");
        let mut conn2 = cp_tunnel.open("s2").await.expect("open for s2");

        // Each connection reached its own session's executor.
        let mut marker = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), conn1.read_exact(&mut marker))
            .await
            .expect("s1's executor never answered")
            .expect("s1's read failed");
        assert_eq!(&marker, b"SRV1");
        let mut marker = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), conn2.read_exact(&mut marker))
            .await
            .expect("s2's executor never answered")
            .expect("s2's read failed");
        assert_eq!(&marker, b"SRV2");

        // The session-to-executor direction flows too.
        conn1.write_all(b"ping-1").await.unwrap();
        conn2.write_all(b"ping-2").await.unwrap();
        wait_until("s1's executor to receive s1's payload", || {
            srv1_seen.lock().unwrap().iter().any(|b| b == b"ping-1")
        })
        .await;
        wait_until("s2's executor to receive s2's payload", || {
            srv2_seen.lock().unwrap().iter().any(|b| b == b"ping-2")
        })
        .await;
        assert!(
            !srv1_seen.lock().unwrap().iter().any(|b| b == b"ping-2"),
            "s1's executor must not receive s2's connection"
        );
        assert!(
            !srv2_seen.lock().unwrap().iter().any(|b| b == b"ping-1"),
            "s2's executor must not receive s1's connection"
        );

        relay.abort();
    }

    /// A logical connection aimed at a session this node does not run is
    /// closed instead of hanging, so the tool call fails rather than
    /// stalling.
    #[tokio::test]
    async fn an_open_for_an_unknown_session_is_closed() {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _cp_opens) = Tunnel::new(cp_side);
        let (node_tunnel, opens) = Tunnel::new(node_side);
        let (srv1_addr, _) = stub_executor(b"SRV1").await;
        let manager = manager_with(&[("s1", srv1_addr.port())]);
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

    /// A session whose executor port has no listener refuses the dial, and
    /// the relay closes the connection instead of hanging, so the tool call
    /// fails rather than stalling.
    #[tokio::test]
    async fn an_open_for_a_session_with_no_executor_listener_is_closed() {
        let (cp_side, node_side) = tokio::io::duplex(1 << 20);
        let (cp_tunnel, _cp_opens) = Tunnel::new(cp_side);
        let (node_tunnel, opens) = Tunnel::new(node_side);

        // Reserve a port, then release it so nothing answers a dial to it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);

        let manager = manager_with(&[("s1", dead_port)]);
        let relay = tokio::spawn(relay_tunnel(node_tunnel, opens, manager));

        let mut conn = cp_tunnel.open("s1").await.expect("the node tunnel is up");
        let mut buf = [0u8; 4];
        let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf))
            .await
            .expect("the dial-refused connection hung")
            .expect("read failed");
        assert_eq!(n, 0, "the peer closes the connection");

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
    async fn fake_control_plane(first: FirstTunnelAction) -> (FakeCpState, SocketAddr) {
        async fn serve_tunnel(
            State(state): State<FakeCpState>,
            Path(_node): Path<String>,
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

    /// A dropped tunnel does not touch the executors: the node reconnects
    /// its one tunnel and the sessions' tool calls reach their executors
    /// again over it.
    #[tokio::test]
    async fn a_dropped_tunnel_reconnects_and_restores_the_node_sessions() {
        let (srv1_addr, _seen) = stub_executor(b"SRV1").await;
        let manager = manager_with(&[("s1", srv1_addr.port())]);
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

        let mut conn = tunnel
            .open("s1")
            .await
            .expect("open on the reconnected tunnel");
        let mut marker = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), conn.read_exact(&mut marker))
            .await
            .expect("s1's executor never answered after the reconnect")
            .expect("read failed");
        assert_eq!(&marker, b"SRV1");

        task.abort();
    }

    /// A protocol violation tears the node tunnel down and the node
    /// reconnects, restoring every session at once.
    #[tokio::test]
    async fn a_protocol_violation_tears_the_tunnel_down_and_the_node_reconnects() {
        let (srv1_addr, _seen) = stub_executor(b"SRV1").await;
        let manager = manager_with(&[("s1", srv1_addr.port())]);
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

        let mut conn = tunnel
            .open("s1")
            .await
            .expect("open on the reconnected tunnel");
        let mut marker = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), conn.read_exact(&mut marker))
            .await
            .expect("s1's executor never answered after the reconnect")
            .expect("read failed");
        assert_eq!(&marker, b"SRV1");

        task.abort();
    }
}
