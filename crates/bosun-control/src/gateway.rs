use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Context;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use axum::response::Response;
use bosun_common::error::ErrorExt;
use hyper::client::conn::http1;
use hyper::upgrade::OnUpgrade;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tracing::debug;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use crate::api::AppState;

/// Routes opencode client requests to node forwarders on one control-plane
/// port. The path prefix `/session/<id>` selects the target; the prefix is
/// stripped before the request is forwarded, so the forwarder sees plain
/// opencode paths. Requests and responses are streamed as bytes after the
/// routing decision; WebSocket upgrades are bridged as raw streams.
pub struct Gateway {
    targets: RwLock<HashMap<String, String>>,
}

impl Gateway {
    pub fn new() -> Self {
        Self {
            targets: RwLock::new(HashMap::new()),
        }
    }

    /// Records or updates the forwarder that serves a session. Called on
    /// heartbeat and at spawn.
    pub fn ensure(&self, session_id: &str, forwarder_addr: &str) {
        let mut targets = self.targets.write().unwrap();
        if targets.get(session_id).map(String::as_str) == Some(forwarder_addr) {
            return;
        }
        targets.insert(session_id.to_string(), forwarder_addr.to_string());
        info!(session_id = %session_id, "session proxy target recorded");
    }

    pub fn remove(&self, session_id: &str) {
        self.targets.write().unwrap().remove(session_id);
    }

    fn target(&self, session_id: &str) -> Option<String> {
        self.targets.read().unwrap().get(session_id).cloned()
    }
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

#[instrument(skip(state, req), fields(path = %req.uri().path()))]
pub async fn route(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    match forward(&state.gateway, req).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error.display_chain(), "session proxy request failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

async fn forward(gateway: &Gateway, req: Request<Body>) -> Result<Response, anyhow::Error> {
    let (mut parts, body) = req.into_parts();
    let Some((session_id, rest)) = split_session_path(parts.uri.path()) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(target) = gateway.target(session_id) else {
        return Ok((StatusCode::NOT_FOUND, format!("no session {session_id}")).into_response());
    };
    debug!(session_id = %session_id, target = %target, "session request routed");
    let target_addr: SocketAddr = target.parse().with_context(|| {
        format!("forwarder address {target} for session {session_id} is invalid")
    })?;

    let upgrade = wants_upgrade(&parts);
    let incoming_upgrade = if upgrade {
        parts.extensions.remove::<OnUpgrade>()
    } else {
        None
    };

    let mut upstream = Request::builder()
        .method(parts.method.clone())
        .uri(upstream_uri(rest, parts.uri.query())?)
        .body(body)
        .context("failed to build the upstream request")?;
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        if name == header::HOST {
            continue;
        }
        upstream.headers_mut().append(name.clone(), value.clone());
    }
    upstream.headers_mut().insert(
        header::HOST,
        header::HeaderValue::from_str(&target)
            .with_context(|| format!("forwarder address {target} is not a valid host header"))?,
    );
    if upgrade {
        upstream.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
        upstream.headers_mut().insert(
            header::UPGRADE,
            header::HeaderValue::from_static("websocket"),
        );
    }

    let stream = TcpStream::connect(target_addr)
        .await
        .with_context(|| format!("failed to connect to forwarder {target_addr}"))?;
    let (mut sender, conn) = http1::handshake::<_, Body>(TokioIo::new(stream))
        .await
        .context("failed to handshake with the forwarder")?;
    tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            debug!(error = %error, "forwarder connection ended with an error");
        }
    });

    let response = sender
        .send_request(upstream)
        .await
        .context("failed to send request to the forwarder")?;

    if upgrade && response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let (mut parts, _) = response.into_parts();
        let upstream_upgrade = parts
            .extensions
            .remove::<OnUpgrade>()
            .context("upstream 101 response carried no upgrade")?;
        let Some(incoming_upgrade) = incoming_upgrade else {
            anyhow::bail!("client requested an upgrade but hyper provided none");
        };
        let mut headers = header::HeaderMap::new();
        for (name, value) in parts.headers.iter() {
            if name == header::CONTENT_LENGTH || name == header::TRANSFER_ENCODING {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        tokio::spawn(async move {
            match upstream_upgrade.await {
                Ok(upstream) => bridge(incoming_upgrade, upstream).await,
                Err(error) => {
                    debug!(error = %error, "forwarder websocket upgrade failed");
                }
            }
        });
        let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        if let Some(headers_map) = builder.headers_mut() {
            *headers_map = headers;
        }
        return builder
            .body(Body::empty())
            .context("failed to build the 101 response");
    }
    if upgrade {
        debug!(
            status = %response.status(),
            "forwarder did not upgrade the connection"
        );
    }

    let (mut parts, body) = response.into_parts();
    let mut headers = header::HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    parts.headers = headers;
    Ok(Response::from_parts(parts, Body::new(body)))
}

async fn bridge(incoming: OnUpgrade, upstream: Upgraded) {
    let client = match incoming.await {
        Ok(client) => client,
        Err(error) => {
            debug!(error = %error, "client websocket upgrade failed");
            return;
        }
    };
    let mut client = TokioIo::new(client);
    let mut upstream = TokioIo::new(upstream);
    if let Err(error) = copy_bidirectional(&mut client, &mut upstream).await {
        debug!(error = %error, "websocket bridge closed with an error");
    }
}

fn split_session_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/session/")?;
    let (id, rest) = rest.split_once('/').unwrap_or((rest, ""));
    if id.is_empty() {
        return None;
    }
    Some((id, rest))
}

fn upstream_uri(rest: &str, query: Option<&str>) -> Result<Uri, anyhow::Error> {
    let path = if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/{rest}")
    };
    let mut uri = path;
    if let Some(query) = query {
        uri.push('?');
        uri.push_str(query);
    }
    uri.parse()
        .context("failed to build the upstream request target")
}

fn wants_upgrade(parts: &Parts) -> bool {
    let connection = parts
        .headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let upgrade = parts
        .headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        && upgrade.eq_ignore_ascii_case("websocket")
}

fn is_hop_by_hop(name: &header::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use axum::routing::get;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;

    use super::*;
    use crate::registry::NodeRegistry;

    #[test]
    fn split_session_path_handles_bare_and_nested_paths() {
        assert_eq!(split_session_path("/session/s1"), Some(("s1", "")));
        assert_eq!(
            split_session_path("/session/s1/global/health"),
            Some(("s1", "global/health"))
        );
        assert_eq!(
            split_session_path("/session/s1/pty/1/connect"),
            Some(("s1", "pty/1/connect"))
        );
        assert_eq!(split_session_path("/session/s1/"), Some(("s1", "")));
        assert_eq!(split_session_path("/session/"), None);
        assert_eq!(split_session_path("/heartbeat"), None);
        assert_eq!(split_session_path("/session"), None);
    }

    #[test]
    fn upstream_uri_builds_an_origin_form_path_with_query() {
        assert_eq!(upstream_uri("", None).unwrap().to_string(), "/");
        assert_eq!(
            upstream_uri("global/health", None).unwrap().to_string(),
            "/global/health"
        );
        assert_eq!(
            upstream_uri("file/content", Some("path=src/main.rs"))
                .unwrap()
                .to_string(),
            "/file/content?path=src/main.rs"
        );
    }

    #[test]
    fn wants_upgrade_detects_a_websocket_request() {
        let mut parts = Request::new(Body::empty()).into_parts().0;
        parts.headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
        parts.headers.insert(
            header::UPGRADE,
            header::HeaderValue::from_static("websocket"),
        );
        assert!(wants_upgrade(&parts));

        let mut plain = Request::new(Body::empty()).into_parts().0;
        plain.headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("keep-alive"),
        );
        assert!(!wants_upgrade(&plain));
    }

    async fn test_server(gateway: Gateway) -> SocketAddr {
        let state = Arc::new(AppState {
            registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
            client: reqwest::Client::new(),
            template_path: PathBuf::new(),
            gateway: Arc::new(gateway),
        });
        let app = crate::api::router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn stub_backend() -> SocketAddr {
        let app = axum::Router::new().route("/global/health", get(|| async { "ok" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn routes_by_session_and_strips_the_prefix() {
        let backend = stub_backend().await;
        let gateway = Gateway::new();
        gateway.ensure("s1", &backend.to_string());
        let addr = test_server(gateway).await;

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/session/s1/global/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn unknown_session_returns_not_found() {
        let addr = test_server(Gateway::new()).await;
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/session/ghost/global/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_session_path_returns_not_found() {
        let addr = test_server(Gateway::new()).await;
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

        let gateway = Gateway::new();
        gateway.ensure("s1", &backend_addr.to_string());
        let addr = test_server(gateway).await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /session/s1/global/health HTTP/1.1\r\nHost: test\r\n\r\n")
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

        let gateway = Gateway::new();
        gateway.ensure("s1", &backend_addr.to_string());
        let addr = test_server(gateway).await;

        let url = format!("ws://{addr}/session/s1/pty/1/connect");
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
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
