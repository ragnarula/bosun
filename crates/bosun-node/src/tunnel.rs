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
use tower_service::Service as _;
use tracing::debug;
use tracing::warn;

const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// Keeps one outbound tunnel to the control plane open for a session. On any
/// failure the connection is re-established after a short delay; the session
/// itself keeps running on the node.
pub async fn run_session_tunnel(
    cp_url: String,
    session_id: String,
    executor_port: u16,
    tls_config: Option<Arc<ClientConfig>>,
) {
    loop {
        match connect_tunnel(&cp_url, &session_id, tls_config.clone()).await {
            Ok(stream) => {
                let (tunnel, mut opens) = Tunnel::new(stream);
                loop {
                    tokio::select! {
                        event = opens.recv() => {
                            let Some(event) = event else { break };
                            let tunnel = tunnel.clone();
                            tokio::spawn(relay_connection(event, executor_port, tunnel));
                        }
                        _ = tunnel.closed() => break,
                    }
                }
            }
            Err(error) => {
                warn!(
                    session_id = %session_id,
                    error = %error.display_chain(),
                    "session tunnel failed; reconnecting"
                );
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Relays one logical connection to the local executor process.
async fn relay_connection(event: OpenEvent, executor_port: u16, tunnel: Tunnel) {
    let mut local = match TcpStream::connect(("127.0.0.1", executor_port)).await {
        Ok(stream) => stream,
        Err(error) => {
            debug!(
                conn_id = event.conn_id,
                error = %error,
                "failed to dial the local executor"
            );
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
    session_id: &str,
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

    let uri: Uri = format!(
        "{}/tunnel/session/{session_id}",
        cp_url.trim_end_matches('/')
    )
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
        .uri(format!("/tunnel/session/{session_id}"))
        .header(header::HOST, authority.as_str())
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "bosun-tunnel")
        .body(Empty::<Bytes>::new())
        .context("failed to build the tunnel request")?;
    let response = sender
        .send_request(request)
        .await
        .context("failed to request the session tunnel")?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        anyhow::bail!(
            "the control plane refused the session tunnel: {}",
            response.status()
        );
    }
    let upgraded = hyper::upgrade::on(response)
        .await
        .context("the control plane accepted the upgrade but hyper provided none")?;
    Ok(TokioIo::new(upgraded))
}
