//! Stub MCP servers shared by this crate's tests.
//!
//! A stub is a local axum server that records every request it serves and
//! answers it from the test's own closure, so a test can assert both what the
//! client sent and what it made of the answer.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bosun_common::mcp::McpTool;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;

use crate::mcp_client::LEGACY_SSE_VERSION;
use crate::mcp_client::MODERN_PROTOCOL_VERSION;
use crate::mcp_client::McpClient;
use crate::mcp_client::PARAM_HEADER_PREFIX;

/// One request the stub served, as the client sent it.
#[derive(Clone)]
pub(crate) struct StubRequest {
    pub(crate) body: Value,
    pub(crate) method: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) accept: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) authorization: Option<String>,
    pub(crate) session_id: Option<String>,
    /// The `Mcp-Param-*` headers the client mirrored from the arguments.
    pub(crate) params: Vec<(String, String)>,
}

impl StubRequest {
    /// The JSON-RPC method of the request body.
    pub(crate) fn message(&self) -> String {
        self.body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    pub(crate) fn id(&self) -> u64 {
        self.body
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or_default()
    }

    /// One key of the request's params, or null when it carries none.
    pub(crate) fn param(&self, key: &str) -> Value {
        self.body["params"].get(key).cloned().unwrap_or(Value::Null)
    }

    /// One key of the request's `_meta`, or null when it carries none.
    pub(crate) fn meta(&self, key: &str) -> Value {
        self.body["params"]["_meta"]
            .get(key)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// One `Mcp-Param-*` header the client mirrored, if it sent it.
    pub(crate) fn mirrored(&self, suffix: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(name, _)| {
                name.get(PARAM_HEADER_PREFIX.len()..)
                    .is_some_and(|rest| rest.eq_ignore_ascii_case(suffix))
            })
            .map(|(_, value)| value.as_str())
    }
}

/// One canned reply. The defaults describe a plain JSON answer.
pub(crate) struct StubReply {
    pub(crate) status: u16,
    pub(crate) content_type: &'static str,
    pub(crate) body: String,
    /// Echoed as the `Mcp-Session-Id` response header.
    pub(crate) session_id: Option<&'static str>,
    /// Echoed as the `WWW-Authenticate` response header, for a stub that
    /// challenges for more scopes.
    pub(crate) www_authenticate: Option<String>,
    /// Leaves the body open after `body`, so an event stream does not end
    /// until the client cancels.
    pub(crate) keep_open: bool,
}

impl StubReply {
    pub(crate) fn with_session_id(mut self, session_id: &'static str) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub(crate) fn with_www_authenticate(mut self, header: &str) -> Self {
        self.www_authenticate = Some(header.to_string());
        self
    }
}

/// A 200 reply with a JSON body.
pub(crate) fn json_ok(body: Value) -> StubReply {
    StubReply {
        status: 200,
        content_type: "application/json",
        body: body.to_string(),
        session_id: None,
        www_authenticate: None,
        keep_open: false,
    }
}

/// A reply with an explicit status and a plain-text body.
pub(crate) fn text_status(status: u16, body: &str) -> StubReply {
    StubReply {
        status,
        content_type: "text/plain",
        body: body.to_string(),
        session_id: None,
        www_authenticate: None,
        keep_open: false,
    }
}

/// A 200 reply whose body is an event stream built from JSON-RPC frames.
pub(crate) fn sse_ok(frames: &[Value], keep_open: bool) -> StubReply {
    StubReply {
        status: 200,
        content_type: "text/event-stream",
        body: frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect(),
        session_id: None,
        www_authenticate: None,
        keep_open,
    }
}

/// The empty-body answer a stub gives a notification.
pub(crate) fn accepted() -> StubReply {
    StubReply {
        status: 202,
        content_type: "application/json",
        body: String::new(),
        session_id: None,
        www_authenticate: None,
        keep_open: false,
    }
}

/// A JSON-RPC result message for `id`.
pub(crate) fn result_message(id: u64, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error message for `id`.
pub(crate) fn error_message(id: u64, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A JSON-RPC notification, which carries no id.
pub(crate) fn notification(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": method })
}

/// The answer a modern stub gives `server/discover`.
pub(crate) fn discover_ok(request: &StubRequest) -> StubReply {
    json_ok(result_message(
        request.id(),
        json!({ "supportedVersions": [MODERN_PROTOCOL_VERSION], "capabilities": {} }),
    ))
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// The response builder one canned reply describes, with its session id and
/// its `WWW-Authenticate` challenge when it carries them.
fn reply_builder(reply: &StubReply) -> axum::http::response::Builder {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(reply.status).unwrap())
        .header(header::CONTENT_TYPE, reply.content_type);
    if let Some(session_id) = reply.session_id {
        builder = builder.header("mcp-session-id", session_id);
    }
    if let Some(challenge) = &reply.www_authenticate {
        builder = builder.header(header::WWW_AUTHENTICATE, challenge);
    }
    builder
}

fn record(headers: &HeaderMap, body: &str) -> StubRequest {
    StubRequest {
        body: serde_json::from_str(body).unwrap_or(Value::Null),
        method: header_string(headers, "mcp-method"),
        version: header_string(headers, "mcp-protocol-version"),
        name: header_string(headers, "mcp-name"),
        accept: header_string(headers, "accept"),
        content_type: header_string(headers, "content-type"),
        authorization: header_string(headers, "authorization"),
        session_id: header_string(headers, "mcp-session-id"),
        params: headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.as_str();
                let prefix = PARAM_HEADER_PREFIX.to_ascii_lowercase();
                name.starts_with(&prefix).then(|| {
                    (
                        name.to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
            })
            .collect(),
    }
}

/// A local stub serving the Streamable HTTP endpoint at `/mcp`: every POST
/// is logged and answered by `reply`. Returns the endpoint URL.
pub(crate) async fn stub_endpoint(
    reply: impl Fn(&StubRequest) -> StubReply + Send + Sync + 'static,
    log: Arc<Mutex<Vec<StubRequest>>>,
) -> String {
    #[derive(Clone)]
    struct Stub {
        reply: Arc<dyn Fn(&StubRequest) -> StubReply + Send + Sync>,
        log: Arc<Mutex<Vec<StubRequest>>>,
    }

    async fn handle(State(stub): State<Stub>, headers: HeaderMap, body: String) -> Response {
        let request = record(&headers, &body);
        stub.log.lock().unwrap().push(request.clone());
        let reply = (stub.reply)(&request);
        let builder = reply_builder(&reply);
        let body = match reply.keep_open {
            true => Body::from_stream(
                stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(reply.body))])
                    .chain(stream::pending::<Result<Bytes, Infallible>>()),
            ),
            false => Body::from(reply.body),
        };
        builder.body(body).unwrap()
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/mcp", post(handle)).with_state(Stub {
        reply: Arc::new(reply),
        log,
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

/// A stub for the deprecated HTTP+SSE transport: the GET stream answers
/// with the `endpoint` event, the POST endpoint pushes each reply onto
/// that stream, and anything POSTed to the MCP URL is refused so the
/// client falls back here. `handshake_only` ends the event stream once the
/// `initialize` answer has been sent, which is what a server does when the
/// client's session is over. Returns the server URL and how many event
/// streams are open.
pub(crate) async fn sse_stub(
    log: Arc<Mutex<Vec<StubRequest>>>,
    handshake_only: bool,
) -> (String, Arc<AtomicUsize>) {
    /// Counts one event stream for as long as its response body lives, so a
    /// test can see when the client let the stream go.
    struct OpenStream(Arc<AtomicUsize>);

    impl Drop for OpenStream {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[derive(Clone)]
    struct Stub {
        log: Arc<Mutex<Vec<StubRequest>>>,
        base: String,
        stream: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Bytes>>>>,
        open: Arc<AtomicUsize>,
        handshake_only: bool,
    }

    async fn refused() -> impl IntoResponse {
        (StatusCode::BAD_REQUEST, "no messages here")
    }

    async fn stream_handler(State(stub): State<Stub>) -> Response {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        let _ = sender.send(Bytes::from(format!(
            "event: endpoint\ndata: {}/messages\n\n",
            stub.base
        )));
        *stub.stream.lock().unwrap() = Some(sender);
        stub.open.fetch_add(1, Ordering::SeqCst);
        let body = stream::unfold(
            (receiver, OpenStream(stub.open.clone())),
            |(mut receiver, guard)| async move {
                receiver
                    .recv()
                    .await
                    .map(|chunk| (Ok::<Bytes, Infallible>(chunk), (receiver, guard)))
            },
        );
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(body))
            .unwrap()
    }

    async fn message_handler(
        State(stub): State<Stub>,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let request = record(&headers, &body);
        stub.log.lock().unwrap().push(request.clone());
        if request.message() == "initialize" {
            let reply = result_message(
                request.id(),
                json!({
                    "protocolVersion": LEGACY_SSE_VERSION,
                    "capabilities": {},
                    "serverInfo": { "name": "old" },
                }),
            );
            let mut stream = stub.stream.lock().unwrap();
            if let Some(sender) = stream.as_ref() {
                let _ = sender.send(Bytes::from(format!("data: {reply}\n\n")));
            }
            if stub.handshake_only {
                // Dropping the sender ends the GET stream.
                *stream = None;
            }
        }
        StatusCode::ACCEPTED.into_response()
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let open = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/mcp", get(stream_handler).post(refused))
        .route("/messages", post(message_handler))
        .with_state(Stub {
            log,
            base,
            stream: Arc::new(Mutex::new(None)),
            open: open.clone(),
            handshake_only,
        });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), open)
}

/// A stub for a server behind OAuth: a GET of the server URL answers the 401
/// challenge that names the protected resource metadata, the metadata
/// documents point at a token endpoint on the same origin, and the MCP
/// endpoint answers `reply` only for a request whose bearer token is
/// `accepted`. A refresh returns the token `accepted` holds at the time.
/// Returns the server URL and how many refreshes the token endpoint served.
pub(crate) async fn oauth_stub(
    reply: impl Fn(&StubRequest) -> StubReply + Send + Sync + 'static,
    log: Arc<Mutex<Vec<StubRequest>>>,
    accepted: Arc<Mutex<String>>,
) -> (String, Arc<AtomicUsize>) {
    #[derive(Clone)]
    struct Stub {
        reply: Arc<dyn Fn(&StubRequest) -> StubReply + Send + Sync>,
        log: Arc<Mutex<Vec<StubRequest>>>,
        issuer: String,
        accepted: Arc<Mutex<String>>,
        refreshes: Arc<AtomicUsize>,
    }

    async fn challenge(State(stub): State<Stub>) -> Response {
        let metadata = format!("{}/.well-known/oauth-protected-resource/mcp", stub.issuer);
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                format!("Bearer resource_metadata=\"{metadata}\""),
            )],
        )
            .into_response()
    }

    async fn resource_metadata(State(stub): State<Stub>) -> Response {
        axum::Json(json!({
            "authorization_servers": [stub.issuer],
            "scopes_supported": ["read", "write"],
        }))
        .into_response()
    }

    async fn authorization_server_metadata(State(stub): State<Stub>) -> Response {
        axum::Json(json!({
            "issuer": stub.issuer,
            "authorization_endpoint": format!("{}/authorize", stub.issuer),
            "token_endpoint": format!("{}/token", stub.issuer),
            "code_challenge_methods_supported": ["S256"],
        }))
        .into_response()
    }

    async fn token(State(stub): State<Stub>) -> Response {
        stub.refreshes.fetch_add(1, Ordering::SeqCst);
        let access_token = stub.accepted.lock().unwrap().clone();
        axum::Json(json!({
            "access_token": access_token,
            "refresh_token": "refresh",
            "expires_in": 3600,
        }))
        .into_response()
    }

    async fn handle(State(stub): State<Stub>, headers: HeaderMap, body: String) -> Response {
        let request = record(&headers, &body);
        stub.log.lock().unwrap().push(request.clone());
        // The server reads its accepted token per request, so a token it has
        // replaced is refused even on a live connection.
        let accepted = stub.accepted.lock().unwrap().clone();
        if request.authorization.as_deref() != Some(&format!("Bearer {accepted}")) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let reply = (stub.reply)(&request);
        let builder = reply_builder(&reply);
        builder.body(Body::from(reply.body)).unwrap()
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/mcp", get(challenge).post(handle))
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/token", post(token))
        .with_state(Stub {
            reply: Arc::new(reply),
            log,
            issuer: format!("http://{addr}"),
            accepted,
            refreshes: refreshes.clone(),
        });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), refreshes)
}

/// A client pointed at a stub. The timeout makes a stub that never
/// answers fail the test instead of hanging it.
pub(crate) fn client_at(url: &str, bearer_token: Option<String>) -> McpClient {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    McpClient::new(client, url, bearer_token)
}

pub(crate) fn empty_log() -> Arc<Mutex<Vec<StubRequest>>> {
    Arc::new(Mutex::new(Vec::new()))
}

/// A tool with no parameters, for a call that needs no mirrored header.
pub(crate) fn plain_tool(name: &str) -> McpTool {
    McpTool {
        name: name.to_string(),
        description: String::new(),
        schema: json!({ "type": "object", "properties": {} }),
    }
}
