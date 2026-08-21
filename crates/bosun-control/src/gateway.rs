use std::sync::Arc;

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
use tracing::debug;
use tracing::instrument;
use tracing::warn;

use crate::api::AppState;
use crate::tunnel::TunnelError;
use crate::tunnel::TunnelRegistry;

/// Routes opencode client requests to session tunnels on one control-plane
/// port. The path prefix `/session/<id>` selects the target; the prefix is
/// stripped before the request is forwarded, so the node's opencode server
/// sees plain opencode paths. Requests and responses are streamed as bytes
/// after the routing decision; WebSocket upgrades are bridged as raw streams.
#[instrument(skip(state, req), fields(path = %req.uri().path()))]
pub async fn route(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    match forward(&state.tunnels, req).await {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error.display_chain(), "session proxy request failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

async fn forward(tunnels: &TunnelRegistry, req: Request<Body>) -> Result<Response, anyhow::Error> {
    let (mut parts, body) = req.into_parts();
    let Some((session_id, rest)) = split_session_path(parts.uri.path()) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let stream = match tunnels.open(session_id).await {
        Ok(stream) => stream,
        Err(TunnelError::NoTunnel { .. }) => {
            return Ok((StatusCode::NOT_FOUND, format!("no session {session_id}")).into_response());
        }
        Err(TunnelError::TunnelClosed { .. }) => {
            return Ok(StatusCode::BAD_GATEWAY.into_response());
        }
    };
    debug!(session_id = %session_id, "session request routed");

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
        header::HeaderValue::from_str(session_id)
            .context("session id is not a valid host header")?,
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

    let (mut sender, conn) = http1::handshake::<_, Body>(TokioIo::new(stream))
        .await
        .context("failed to handshake with the session tunnel")?;
    tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            debug!(error = %error, "session tunnel connection ended with an error");
        }
    });

    let response = sender
        .send_request(upstream)
        .await
        .context("failed to send request to the session tunnel")?;

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
                    debug!(error = %error, "session websocket upgrade failed");
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
            "session tunnel did not upgrade the connection"
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
    use super::*;

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
        assert_eq!(split_session_path("/poll"), None);
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
}
