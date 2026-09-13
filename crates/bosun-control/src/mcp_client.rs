//! JSON-RPC client for one MCP server over the Streamable HTTP transport,
//! with the `initialize` handshake and the deprecated HTTP+SSE transport
//! behind it.
//!
//! One client speaks one server. It detects the server's protocol era at
//! connect and caches it: a modern server gets per-request `_meta` and the
//! 2026-07-28 revision, a legacy one gets the handshake and the revision it
//! answers with. The client is `Sync`, so listing tools, calling tools, and
//! listening for tool-list changes can run at the same time on one client.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bosun_agent::sse::SseError;
use bosun_agent::sse::SseEvent;
use bosun_agent::sse::sse_stream;
use bosun_common::error::ErrorExt;
use bosun_common::mcp::McpCallOutcome;
use bosun_common::mcp::McpTool;
use bosun_common::mcp::mcp_structured_content_text;
use bosun_common::mcp::mcp_tool_result_text;
use bosun_common::version::VERSION;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::error;
use tracing::instrument;
use tracing::warn;

use crate::mcp_oauth::auth_param;

/// The revision this client speaks when the server is modern.
pub(crate) const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// The latest legacy revision the client proposes in the `initialize`
/// handshake; the server answers with the revision it speaks.
const LEGACY_INITIALIZE_VERSION: &str = "2025-06-18";
/// The revision of the deprecated HTTP+SSE transport.
pub(crate) const LEGACY_SSE_VERSION: &str = "2024-11-05";
/// The client name the server sees in `clientInfo`.
const CLIENT_NAME: &str = "bosun";
/// The JSON-RPC code for a protocol version the server does not support.
const UNSUPPORTED_PROTOCOL_VERSION_CODE: i64 = -32022;
/// The JSON-RPC code for a client capability the server requires.
const MISSING_CAPABILITY_CODE: i64 = -32021;
/// The JSON-RPC code for a request whose headers disagree with its body.
const HEADER_MISMATCH_CODE: i64 = -32020;
/// The schema annotation that mirrors a tool parameter into a request
/// header.
const HEADER_ANNOTATION: &str = "x-mcp-header";
/// The prefix of a mirrored parameter header.
pub(crate) const PARAM_HEADER_PREFIX: &str = "Mcp-Param-";
/// The marker around a header value that is Base64-encoded, per the spec's
/// value-encoding rule.
const BASE64_SENTINEL_PREFIX: &str = "=?base64?";
const BASE64_SENTINEL_SUFFIX: &str = "?=";
/// The largest integer a mirrored parameter may hold, per the spec: the safe
/// range of an IEEE754 double.
const MAX_SAFE_INTEGER: i64 = (1i64 << 53) - 1;
/// The most characters of an error body kept in an error's detail, so a
/// server cannot make a detail grow without bound.
const MAX_DETAIL_CHARS: usize = 200;
/// The most `tools/list` pages one listing follows, so a server that keeps
/// returning a cursor cannot make the call run forever.
const MAX_TOOL_PAGES: usize = 100;
/// How long one request may take before the client gives up on it. A tool
/// call can run for minutes, so the bound is generous; it exists so a server
/// that stalls after the TCP handshake fails the request instead of holding
/// the server's task and a session's call. It is applied per request rather
/// than to the client's `reqwest::Client`, because a client-wide timeout
/// would also cut the `subscriptions/listen` stream, which stays open for the
/// connection's lifetime. A listing that follows several pages spends this
/// bound on each page, and [`MAX_TOOL_PAGES`] limits how many there are.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Errors a caller can act on, plus a catch-all for internal failures. No
/// variant carries a token or another secret.
#[derive(Debug, Error)]
pub enum McpClientError {
    /// The server rejected the credentials. The caller refreshes or
    /// re-authorizes; this module never does.
    #[error("the MCP server rejected the client's credentials")]
    Unauthorized,

    /// The server answered with an `insufficient_scope` challenge: the token
    /// is valid but carries too few scopes. The caller re-authorizes with the
    /// union of the granted scopes and `scope`; this module never does.
    #[error("the MCP server requires more OAuth scopes than the token carries")]
    InsufficientScope {
        /// The `scope` auth-param of the challenge, when it named one.
        scope: Option<String>,
    },

    /// The server speaks the modern revision but supports none of the
    /// versions the client offered.
    #[error("the MCP server supports none of the offered protocol versions: {supported:?}")]
    UnsupportedVersion { supported: Vec<String> },

    /// The server requires a client capability this client does not send.
    #[error("the MCP server requires client capabilities the client does not send: {required:?}")]
    MissingCapability { required: Vec<String> },

    #[error("the MCP server returned HTTP {status}: {detail}")]
    HttpStatus { status: u16, detail: String },

    #[error("the MCP server returned JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },

    /// A legacy request carried an `Mcp-Session-Id` and the server answered
    /// 404: the session expired. The caller reconnects this server.
    #[error("the MCP server's session expired")]
    SessionExpired,

    #[error("the MCP server broke the protocol: {detail}")]
    Protocol { detail: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// The protocol era one server speaks, detected at connect and cached for the
/// connection's lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEra {
    /// The current revision, with per-request `_meta`.
    Modern { version: String },

    /// A legacy revision over the Streamable HTTP transport, reached through
    /// the `initialize` handshake.
    Legacy { version: String },

    /// A revision before 2025-06-18, over the deprecated HTTP+SSE transport.
    LegacySse { version: String },
}

impl ServerEra {
    /// The revision this connection uses.
    pub fn version(&self) -> &str {
        match self {
            ServerEra::Modern { version }
            | ServerEra::Legacy { version }
            | ServerEra::LegacySse { version } => version,
        }
    }
}

/// One server's tools and how long the server lets the client cache them.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolList {
    pub tools: Vec<McpTool>,
    /// The `ttlMs` of the last page that carried one.
    pub ttl_ms: Option<u64>,
}

/// The deprecated HTTP+SSE transport's state: the POST URL from the
/// `endpoint` event, the waiters for responses that arrive on the open GET
/// stream, and whether that stream's reader is still running.
#[derive(Clone)]
struct SseEndpoint {
    post_url: String,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// Set by the reader as it stops. Once it is set, no response can arrive
    /// on the stream any more, so a request fails instead of waiting.
    closed: Arc<AtomicBool>,
}

/// One registered waiter on the deprecated transport's event stream. The
/// entry goes away with the request that registered it, so a wait that times
/// out or is cancelled cannot leave it behind for the connection's lifetime.
struct SseWaiter {
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    id: u64,
}

impl Drop for SseWaiter {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

/// A JSON-RPC client for one MCP server. The URL, the token, and the reqwest
/// client are constructor parameters, so a test can point the client at a
/// local stub. The token, when set, travels only in an `Authorization`
/// header and is never logged.
pub struct McpClient {
    client: reqwest::Client,
    url: String,
    bearer_token: Option<String>,
    era: Option<ServerEra>,
    /// The `Mcp-Session-Id` a legacy server returned, echoed on later
    /// requests.
    session_id: Option<String>,
    sse: Option<SseEndpoint>,
    /// The task reading the deprecated transport's event stream. Aborted
    /// before the endpoint is replaced or cleared, so a repeated connect
    /// does not leave an open connection behind.
    sse_task: Option<tokio::task::JoinHandle<()>>,
    /// Request ids only have to be unique within the connection; the counter
    /// is atomic so calls and the listen stream can share one client.
    next_id: AtomicU64,
}

impl McpClient {
    pub fn new(client: reqwest::Client, url: &str, bearer_token: Option<String>) -> Self {
        Self {
            client,
            url: url.to_string(),
            bearer_token,
            era: None,
            session_id: None,
            sse: None,
            sse_task: None,
            next_id: AtomicU64::new(1),
        }
    }

    /// Probes the server and caches the era it speaks: modern when
    /// `server/discover` answers, else the `initialize` handshake, else the
    /// deprecated HTTP+SSE transport. The whole detection is bounded by
    /// [`REQUEST_TIMEOUT`], so a server that stops answering fails the
    /// connect instead of holding its caller.
    #[instrument(skip_all)]
    pub async fn connect(&mut self) -> Result<ServerEra, McpClientError> {
        // A connect re-detects the server from scratch: nothing the previous
        // detection left behind describes it any more.
        self.era = None;
        self.session_id = None;
        self.clear_sse();
        let era = match bounded(self.detect_era()).await {
            Ok(era) => era,
            Err(error) => {
                // A half-finished handshake leaves no transport behind.
                self.session_id = None;
                self.clear_sse();
                return Err(error);
            }
        };
        self.era = Some(era.clone());
        Ok(era)
    }

    /// Drops the deprecated transport's endpoint and stops its reader task,
    /// so the GET connection does not outlive the connection that opened it.
    fn clear_sse(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
        self.sse = None;
    }

    /// The era [`McpClient::connect`] detected, or None before the first
    /// connect.
    pub fn era(&self) -> Option<&ServerEra> {
        self.era.as_ref()
    }

    /// Lists every tool the server advertises, following `nextCursor` until
    /// the server stops returning one.
    #[instrument(skip_all)]
    pub async fn tools_list(&self) -> Result<McpToolList, McpClientError> {
        let mut tools = Vec::new();
        let mut ttl_ms = None;
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            if pages == MAX_TOOL_PAGES {
                return Err(McpClientError::Protocol {
                    detail: format!("the server returned more than {MAX_TOOL_PAGES} tool pages"),
                });
            }
            pages += 1;
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, None, &[], None).await?;
            let page: ToolsListResult =
                serde_json::from_value(result).map_err(|error| McpClientError::Protocol {
                    detail: format!("the tools/list result was malformed: {error}"),
                })?;
            for tool in page.tools {
                // A tool whose `x-mcp-header` annotations break the spec's
                // constraints is excluded, so one bad definition cannot stop
                // the other tools from working.
                if let Err(reason) = header_specs(&tool.input_schema) {
                    warn!(
                        tool = %tool.name,
                        reason = %reason,
                        "excluding an MCP tool with invalid x-mcp-header annotations"
                    );
                    continue;
                }
                tools.push(McpTool {
                    name: tool.name,
                    description: tool.description.unwrap_or_default(),
                    schema: tool.input_schema,
                });
            }
            if let Some(ttl) = page.ttl_ms {
                // A negative TTL means immediately stale, per the spec's
                // caching rule.
                ttl_ms = Some(ttl.max(0) as u64);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(McpToolList { tools, ttl_ms })
    }

    /// Calls one tool. Progress notifications the server sends on the
    /// response are forwarded to `progress` when the caller passes one.
    #[instrument(skip_all, fields(tool = %tool.name))]
    pub async fn tools_call(
        &self,
        tool: &McpTool,
        arguments: Value,
        progress: Option<&UnboundedSender<Value>>,
    ) -> Result<McpCallOutcome, McpClientError> {
        // A parameter the tool annotates travels in its own header as well as
        // in the body.
        let headers = mirrored_headers(&tool.schema, &arguments)?;
        let params = json!({ "name": tool.name, "arguments": arguments });
        let result = match self
            .request("tools/call", params, Some(&tool.name), &headers, progress)
            .await
        {
            Ok(result) => result,
            // The server answered the call with a JSON-RPC error, which is
            // the call's outcome rather than a transport failure. The errors
            // a caller acts on stay errors.
            Err(McpClientError::JsonRpc { message, .. }) => {
                return Ok(McpCallOutcome {
                    text: message,
                    is_error: true,
                });
            }
            Err(error) => return Err(error),
        };

        // A result that is not `complete` carries work the client cannot
        // serve: the capabilities it sends are empty, so no input request can
        // be answered.
        if let Some(result_type) = result.get("resultType").and_then(Value::as_str)
            && result_type != "complete"
        {
            return Err(McpClientError::Protocol {
                detail: format!("the server answered tools/call with resultType {result_type}"),
            });
        }

        let mut text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|content| mcp_tool_result_text(content))
            .unwrap_or_default();
        if let Some(structured) = result
            .get("structuredContent")
            .filter(|value| !value.is_null())
        {
            let structured = mcp_structured_content_text(structured);
            text = match text.is_empty() {
                true => structured,
                false => format!("{text}\n{structured}"),
            };
        }
        Ok(McpCallOutcome {
            text,
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Opens the server's `subscriptions/listen` stream and calls
    /// `on_tools_changed` for every `notifications/tools/list_changed` the
    /// server sends. Runs until the stream ends. The opening request is
    /// bounded by [`REQUEST_TIMEOUT`]; the stream it opens is not. The caller
    /// cancels by dropping this future, which closes the response stream:
    /// HTTP has no cancellation message.
    #[instrument(skip_all)]
    pub async fn subscriptions_listen<F>(
        &self,
        mut on_tools_changed: F,
    ) -> Result<(), McpClientError>
    where
        F: FnMut(),
    {
        if self.sse.is_some() {
            // The listen stream is a modern method, and the deprecated
            // transport has no request stream to carry it.
            return Err(McpClientError::Protocol {
                detail: "the deprecated transport has no subscriptions stream".to_string(),
            });
        }
        let params = self.request_params(
            json!({ "notifications": { "toolsListChanged": true } }),
            None,
        );
        let body = request_body(self.next_id(), "subscriptions/listen", params);
        // Opening the stream is a request like any other, so it carries the
        // same bound; a server that never answers must not leave the listener
        // waiting for the connection's lifetime.
        let response = bounded(async {
            let response = self
                .post(
                    "subscriptions/listen",
                    self.protocol_version(),
                    None,
                    &[],
                    &body,
                )
                .await?;
            if !response.status().is_success() {
                return Err(response_error(response).await);
            }
            if !is_event_stream(&response) {
                let body = response
                    .bytes()
                    .await
                    .map_err(|error| McpClientError::Internal(error.into()))?;
                return Err(match jsonrpc_error(&body) {
                    Some(error) => map_jsonrpc_error(error),
                    None => McpClientError::Protocol {
                        detail: "subscriptions/listen did not answer with an event stream"
                            .to_string(),
                    },
                });
            }
            Ok(response)
        })
        .await?;

        let mut events = Box::pin(sse_stream(response.bytes_stream()));
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| McpClientError::Internal(error.into()))?;
            let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            if message.get("method").and_then(Value::as_str)
                == Some("notifications/tools/list_changed")
            {
                on_tools_changed();
            }
        }
        Ok(())
    }

    /// The era probe: `server/discover` with the current revision, then with
    /// the server's own supported version on a `-32022` answer, then the two
    /// legacy fallbacks.
    async fn detect_era(&mut self) -> Result<ServerEra, McpClientError> {
        let version = match self.probe_modern(MODERN_PROTOCOL_VERSION).await? {
            Probe::Modern => {
                return Ok(ServerEra::Modern {
                    version: MODERN_PROTOCOL_VERSION.to_string(),
                });
            }
            Probe::Legacy => return self.connect_legacy().await,
            Probe::UnsupportedVersion { supported } => mutually_supported_version(&supported),
        };
        match self.probe_modern(&version).await? {
            Probe::Modern => Ok(ServerEra::Modern { version }),
            Probe::Legacy => self.connect_legacy().await,
            Probe::UnsupportedVersion { supported } => {
                Err(McpClientError::UnsupportedVersion { supported })
            }
        }
    }

    /// One `server/discover` attempt with `version`, which travels in both
    /// the request header and `_meta`. A modern server answers it, or refuses
    /// it over the protocol version or a client capability; anything else
    /// means it has no `server/discover` and the legacy handshake comes next.
    async fn probe_modern(&self, version: &str) -> Result<Probe, McpClientError> {
        let id = self.next_id();
        // The era is not known yet, so the probe carries the modern `_meta`
        // for the version it is asking about.
        let params = json!({ "_meta": client_meta(version) });
        let body = request_body(id, "server/discover", params);
        let response = self
            .post("server/discover", version, None, &[], &body)
            .await?;
        let status = response.status();
        if status.is_success() {
            return match self.read_response(response, id, None).await {
                Ok(_) => Ok(Probe::Modern),
                Err(McpClientError::UnsupportedVersion { supported }) => {
                    Ok(Probe::UnsupportedVersion { supported })
                }
                // A recognized modern error keeps the server modern: the
                // client does not fall back to the handshake.
                Err(error @ McpClientError::JsonRpc { code, .. }) if is_modern_error(code) => {
                    Err(error)
                }
                Err(error @ McpClientError::MissingCapability { .. }) => Err(error),
                // The modern revision defines an error for the version and the
                // client capabilities; any other JSON-RPC error, a body that is
                // not JSON at all, and a refused request with no JSON-RPC error
                // mean the server has no `server/discover`: it is a legacy one.
                Err(McpClientError::JsonRpc { .. }) | Err(McpClientError::Protocol { .. }) => {
                    Ok(Probe::Legacy)
                }
                Err(error) => Err(error),
            };
        }

        let headers = response.headers().clone();
        let body = response
            .bytes()
            .await
            .map_err(|error| McpClientError::Internal(error.into()))?;
        // A refused request can be a challenge for scopes the token does not
        // carry, which `status_error` turns into the error a caller acts on.
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(status_error(status, &headers, &body));
        }
        if let Some(error) = jsonrpc_error(&body) {
            if error.code == UNSUPPORTED_PROTOCOL_VERSION_CODE {
                return Ok(Probe::UnsupportedVersion {
                    supported: string_list(error.data.as_ref(), "supported"),
                });
            }
            if is_modern_error(error.code) {
                return Err(map_jsonrpc_error(error));
            }
            // A legacy Streamable HTTP server does not have `server/discover`
            // and answers an unknown method: 404 with JSON-RPC code -32601.
            return Ok(Probe::Legacy);
        }
        // A refused POST with no body at all is a server without the modern
        // endpoint.
        if matches!(status.as_u16(), 400 | 404 | 405) {
            return Ok(Probe::Legacy);
        }
        Err(http_status_error(status, &body))
    }

    /// The legacy handshake over the Streamable HTTP transport: `initialize`
    /// with the latest legacy revision the client speaks, then the
    /// `initialized` notification. A server that refuses the POST with 400,
    /// 404, or 405 and no version or capability error is on the deprecated
    /// HTTP+SSE transport instead.
    async fn connect_legacy(&mut self) -> Result<ServerEra, McpClientError> {
        let id = self.next_id();
        let body = request_body(id, "initialize", initialize_params());
        let response = self
            .post("initialize", LEGACY_INITIALIZE_VERSION, None, &[], &body)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| McpClientError::Internal(error.into()))?;
            // A server that refuses the handshake with 400, 404, or 405 and no
            // version or capability error is not a Streamable HTTP server at
            // all: the deprecated transport comes next.
            let modern = jsonrpc_error(&bytes).is_some_and(|error| is_modern_error(error.code));
            if matches!(status.as_u16(), 400 | 404 | 405) && !modern {
                return self.connect_legacy_sse().await;
            }
            return Err(status_error(status, &headers, &bytes));
        }

        self.session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let result = self.read_response(response, id, None).await?;
        let version = negotiated_version(&result, LEGACY_INITIALIZE_VERSION);
        self.notify("notifications/initialized", &version).await?;
        Ok(ServerEra::Legacy { version })
    }

    /// The deprecated HTTP+SSE transport: GET the server URL, read the
    /// `endpoint` event for the POST URL, and handshake on the GET stream,
    /// which stays open for the connection's lifetime.
    async fn connect_legacy_sse(&mut self) -> Result<ServerEra, McpClientError> {
        let response = self
            .authorize(
                self.client
                    .get(&self.url)
                    .header("accept", "text/event-stream"),
            )
            .send()
            .await
            .map_err(|error| McpClientError::Internal(error.into()))?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }

        let mut events = Box::pin(sse_stream(response.bytes_stream()));
        let endpoint = events
            .next()
            .await
            .ok_or_else(|| McpClientError::Protocol {
                detail: "the server closed the event stream before the endpoint event".to_string(),
            })?
            .map_err(|error| McpClientError::Internal(error.into()))?;
        if endpoint.event.as_deref() != Some("endpoint") {
            return Err(McpClientError::Protocol {
                detail: format!(
                    "the first event was not the endpoint event: {:?}",
                    endpoint.event
                ),
            });
        }
        let post_url = self.resolve_endpoint(&endpoint.data)?;
        // A previous endpoint's reader must not outlive this connection.
        self.clear_sse();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        self.sse = Some(SseEndpoint {
            post_url,
            pending: pending.clone(),
            closed: closed.clone(),
        });
        self.sse_task = Some(tokio::spawn(read_event_stream(events, pending, closed)));

        let id = self.next_id();
        let body = request_body(id, "initialize", initialize_params());
        let result = self.request_sse(id, &body).await?;
        let version = negotiated_version(&result, LEGACY_SSE_VERSION);
        self.notify("notifications/initialized", &version).await?;
        Ok(ServerEra::LegacySse { version })
    }

    /// Sends one JSON-RPC request over whichever transport the connection
    /// uses, and returns its `result`. The request and the wait for its
    /// response are bounded by [`REQUEST_TIMEOUT`], so a server that stops
    /// answering fails the list or the call rather than holding its caller.
    async fn request(
        &self,
        method: &str,
        params: Value,
        tool_name: Option<&str>,
        headers: &[(String, String)],
        progress: Option<&UnboundedSender<Value>>,
    ) -> Result<Value, McpClientError> {
        // The progress token is the request id: unique per request and known
        // to the caller's channel. The deprecated transport carries responses
        // on a separate stream that the client reads for responses only, so a
        // token there would never deliver anything.
        let id = self.next_id();
        let progress_token = progress.filter(|_| self.sse.is_none()).map(|_| id);
        let params = self.request_params(params, progress_token);
        let body = request_body(id, method, params);
        if self.sse.is_some() {
            return self.request_sse(id, &body).await;
        }

        bounded(async {
            let response = self
                .post(method, self.protocol_version(), tool_name, headers, &body)
                .await?;
            if !response.status().is_success() {
                let status = response.status();
                // A legacy session that expired answers the next request with
                // 404. The caller reconnects rather than holding a dead
                // session.
                if status == reqwest::StatusCode::NOT_FOUND && self.session_id.is_some() {
                    return Err(McpClientError::SessionExpired);
                }
                return Err(response_error(response).await);
            }
            self.read_response(response, id, progress).await
        })
        .await
    }

    /// Sends one request over the deprecated transport and waits for the
    /// response the event stream carries for its id. A request that arrives
    /// after the reader has stopped fails at once, because the reader cannot
    /// deliver a response any more, and the wait is bounded by
    /// [`REQUEST_TIMEOUT`] like every other request.
    async fn request_sse(&self, id: u64, body: &Value) -> Result<Value, McpClientError> {
        let Some(sse) = self.sse.clone() else {
            return Err(McpClientError::Protocol {
                detail: "the event stream endpoint is not known".to_string(),
            });
        };
        bounded(async {
            let (sender, receiver) = oneshot::channel();
            sse.pending.lock().unwrap().insert(id, sender);
            // The waiter is removed however this request ends, so a wait that
            // times out or is cancelled leaves nothing behind.
            let _waiter = SseWaiter {
                pending: sse.pending.clone(),
                id,
            };
            // The reader clears `pending` as it stops, so a waiter that is
            // registered after that clear is never answered. The flag is read
            // after the insert, which catches either order.
            if sse.closed.load(Ordering::SeqCst) {
                return Err(stream_closed_error());
            }
            let response = self.post_sse(&sse.post_url, body).await?;
            if !response.status().is_success() {
                return Err(response_error(response).await);
            }
            // The sender is dropped when the reader stops without answering,
            // which is the same failure as the flag.
            receiver.await.map_err(|_| stream_closed_error())
        })
        .await
    }

    /// POSTs one JSON-RPC notification, with `version` in the protocol header
    /// when the connection uses the Streamable HTTP transport. A notification
    /// carries no id and its answer carries no body.
    async fn notify(&self, method: &str, version: &str) -> Result<(), McpClientError> {
        let body = json!({ "jsonrpc": "2.0", "method": method });
        let response = match &self.sse {
            // The deprecated transport has no protocol version header.
            Some(sse) => self.post_sse(&sse.post_url, &body).await?,
            None => self.post(method, version, None, &[], &body).await?,
        };
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        Ok(())
    }

    /// POSTs one message over the Streamable HTTP transport. Every message is
    /// its own request: the method and the revision travel in headers, and
    /// the tool name too for a call.
    async fn post(
        &self,
        method: &str,
        version: &str,
        tool_name: Option<&str>,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<reqwest::Response, McpClientError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", version)
            .header("mcp-method", method);
        if let Some(tool_name) = tool_name {
            request = request.header("mcp-name", encode_header_value(tool_name));
        }
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("mcp-session-id", session_id);
        }
        self.authorize(request)
            .json(body)
            .send()
            .await
            .map_err(|error| McpClientError::Internal(error.into()))
    }

    /// POSTs one message to the deprecated transport's endpoint URL. The
    /// server answers an empty 2xx and sends any response on the open event
    /// stream.
    async fn post_sse(
        &self,
        post_url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, McpClientError> {
        self.authorize(
            self.client
                .post(post_url)
                .header("content-type", "application/json"),
        )
        .json(body)
        .send()
        .await
        .map_err(|error| McpClientError::Internal(error.into()))
    }

    /// Reads the answer to one request: the single JSON object of an
    /// `application/json` reply, or the event stream whose response with the
    /// matching id ends it. Messages that arrive first are forwarded to
    /// `progress` when the caller asked for them.
    async fn read_response(
        &self,
        response: reqwest::Response,
        id: u64,
        progress: Option<&UnboundedSender<Value>>,
    ) -> Result<Value, McpClientError> {
        if !is_event_stream(&response) {
            let body = response
                .bytes()
                .await
                .map_err(|error| McpClientError::Internal(error.into()))?;
            let message: Value =
                serde_json::from_slice(&body).map_err(|error| McpClientError::Protocol {
                    detail: format!("the server sent a body that is not JSON: {error}"),
                })?;
            return message_result(message);
        }

        let mut events = Box::pin(sse_stream(response.bytes_stream()));
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| McpClientError::Internal(error.into()))?;
            let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                debug!("ignoring an event that is not a JSON-RPC message");
                continue;
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message_result(message);
            }
            if let Some(progress) = progress {
                let _ = progress.send(message);
            }
        }
        Err(McpClientError::Protocol {
            detail: "the server closed the event stream before it answered".to_string(),
        })
    }

    /// The params of one request with `_meta` merged in: a modern request
    /// carries the revision, the client identity, and the client's
    /// capabilities, and any request carries a progress token when the caller
    /// asked for progress.
    fn request_params(&self, mut params: Value, progress_token: Option<u64>) -> Value {
        let mut meta = match &self.era {
            Some(ServerEra::Modern { version }) => client_meta(version),
            _ => json!({}),
        };
        if let Some(token) = progress_token {
            meta["progressToken"] = json!(token);
        }
        if !meta.as_object().is_some_and(|meta| meta.is_empty()) {
            params["_meta"] = meta;
        }
        params
    }

    /// The revision this connection uses: the detected era's, or the current
    /// revision before `connect` has run.
    fn protocol_version(&self) -> &str {
        match &self.era {
            Some(era) => era.version(),
            None => MODERN_PROTOCOL_VERSION,
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Adds the bearer token to a request when the caller configured one.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.bearer_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// The POST URL an `endpoint` event names, resolved against the server
    /// URL because the event may carry a path instead of a full URL.
    fn resolve_endpoint(&self, endpoint: &str) -> Result<String, McpClientError> {
        let base = reqwest::Url::parse(&self.url).map_err(|error| McpClientError::Protocol {
            detail: format!("the server URL is not a valid URL: {error}"),
        })?;
        let url = base
            .join(endpoint.trim())
            .map_err(|error| McpClientError::Protocol {
                detail: format!("the endpoint event carried an invalid URL: {error}"),
            })?;
        // The client sends its bearer token to this URL, so it must stay on
        // the server the operator configured.
        if url.origin() != base.origin() {
            return Err(McpClientError::Protocol {
                detail: format!(
                    "the endpoint event named another origin: {}",
                    url.origin().ascii_serialization()
                ),
            });
        }
        Ok(url.to_string())
    }
}

impl Drop for McpClient {
    /// Ends the deprecated transport's reader task. Replacing a connection
    /// drops its client, and the task holds the open GET stream, so leaving
    /// it running would leave one task and one connection behind per
    /// reconnect.
    fn drop(&mut self) {
        self.clear_sse();
    }
}

/// Runs one request future with the per-request bound, so a server that stops
/// answering fails the request instead of holding its caller.
async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, McpClientError>>,
) -> Result<T, McpClientError> {
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(result) => result,
        Err(_) => Err(McpClientError::Internal(anyhow::anyhow!(
            "the MCP server did not answer within {REQUEST_TIMEOUT:?}"
        ))),
    }
}

/// The error a request reports when the event stream it waits on has ended,
/// so no response can arrive.
fn stream_closed_error() -> McpClientError {
    McpClientError::Protocol {
        detail: "the server closed the event stream before it answered".to_string(),
    }
}

/// What one modern probe found.
enum Probe {
    /// The server answered `server/discover`.
    Modern,

    /// The server speaks the modern revision but not the version probed.
    UnsupportedVersion { supported: Vec<String> },

    /// The server answered in no modern way: a JSON-RPC error other than the
    /// ones the modern revision defines, a body that is not JSON, or 400,
    /// 404, or 405 with no JSON-RPC error.
    Legacy,
}

/// Reads a deprecated transport's event stream for the connection's lifetime
/// and hands each response to the waiter registered for its id. When the
/// stream ends, the waiters still registered are dropped, which fails their
/// requests instead of leaving them waiting for an answer that cannot arrive,
/// and the stream is marked closed, which fails a request that registers
/// afterwards.
#[instrument(skip_all)]
async fn read_event_stream<S>(
    mut events: S,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    closed: Arc<AtomicBool>,
) where
    S: futures_util::Stream<Item = Result<SseEvent, SseError>> + Send + Unpin + 'static,
{
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                error!(
                    error = %error.display_chain(),
                    "the MCP server's event stream failed"
                );
                break;
            }
        };
        let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let waiter = pending.lock().unwrap().remove(&id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(message);
        }
    }
    // The stream is the connection: the flag fails a request that arrives
    // after this point, and dropping the waiters fails every request still
    // waiting on it, instead of leaving either waiting forever.
    closed.store(true, Ordering::SeqCst);
    pending.lock().unwrap().clear();
}

fn request_body(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// The `initialize` params: the latest legacy revision the client speaks, no
/// capabilities, and the client identity.
fn initialize_params() -> Value {
    json!({
        "protocolVersion": LEGACY_INITIALIZE_VERSION,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": VERSION },
    })
}

/// The revision the server chose in an `initialize` result, or `fallback`
/// when the result omits it.
fn negotiated_version(result: &Value, fallback: &str) -> String {
    result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// The `_meta` every modern request carries.
fn client_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": { "name": CLIENT_NAME, "version": VERSION },
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// The version to retry the probe with: the current revision when the server
/// lists it, else the first version the server lists.
fn mutually_supported_version(supported: &[String]) -> String {
    if supported
        .iter()
        .any(|version| version == MODERN_PROTOCOL_VERSION)
    {
        return MODERN_PROTOCOL_VERSION.to_string();
    }
    supported
        .first()
        .cloned()
        .unwrap_or_else(|| MODERN_PROTOCOL_VERSION.to_string())
}

/// The JSON-RPC error a body carries, when it carries one. A 400, 404, or 405
/// body with one of these means the server speaks JSON-RPC, so it is not a
/// server on the deprecated HTTP+SSE transport.
fn jsonrpc_error(body: &[u8]) -> Option<JsonRpcError> {
    let message: Value = serde_json::from_slice(body).ok()?;
    message_error(&message)
}

/// The JSON-RPC error a message carries, when it parses as one.
fn message_error(message: &Value) -> Option<JsonRpcError> {
    serde_json::from_value(message.get("error")?.clone()).ok()
}

/// The `result` of a JSON-RPC message, or the error it carries.
fn message_result(message: Value) -> Result<Value, McpClientError> {
    if let Some(error) = message_error(&message) {
        return Err(map_jsonrpc_error(error));
    }
    match message.get("result") {
        Some(result) => Ok(result.clone()),
        None if message.get("error").is_some() => Err(McpClientError::Protocol {
            detail: "the server sent a malformed JSON-RPC error".to_string(),
        }),
        None => Err(McpClientError::Protocol {
            detail: "the server sent a message with neither a result nor an error".to_string(),
        }),
    }
}

/// Maps a JSON-RPC error onto the variant a caller acts on.
fn map_jsonrpc_error(error: JsonRpcError) -> McpClientError {
    match error.code {
        UNSUPPORTED_PROTOCOL_VERSION_CODE => McpClientError::UnsupportedVersion {
            supported: string_list(error.data.as_ref(), "supported"),
        },
        MISSING_CAPABILITY_CODE => McpClientError::MissingCapability {
            required: capability_names(error.data.as_ref()),
        },
        code => McpClientError::JsonRpc {
            code,
            message: error.message,
        },
    }
}

/// Whether a JSON-RPC code is one the modern revision defines for the
/// protocol version, the client capabilities, or a request whose headers
/// disagree with its body. A recognized modern error means the server speaks
/// that revision; every other answer identifies a legacy server.
fn is_modern_error(code: i64) -> bool {
    matches!(
        code,
        UNSUPPORTED_PROTOCOL_VERSION_CODE | MISSING_CAPABILITY_CODE | HEADER_MISMATCH_CODE
    )
}

/// The capability names a `MissingRequiredClientCapabilityError` requires:
/// the keys of its `requiredCapabilities` object.
fn capability_names(data: Option<&Value>) -> Vec<String> {
    data.and_then(|data| data.get("requiredCapabilities"))
        .and_then(Value::as_object)
        .map(|capabilities| capabilities.keys().cloned().collect())
        .unwrap_or_default()
}

/// The error a failed response carries: a challenge for scopes the token is
/// missing, unauthorized for a 401, else the JSON-RPC error the body carries,
/// else the HTTP status with the body's text.
async fn response_error(response: reqwest::Response) -> McpClientError {
    let status = response.status();
    // The challenge is in the headers, so they are read before the body
    // consumes the response.
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap_or_default();
    status_error(status, &headers, &body)
}

/// The error a failed status, headers, and body carry.
fn status_error(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> McpClientError {
    if let Some(scope) = insufficient_scope_challenge(status, headers) {
        return McpClientError::InsufficientScope { scope };
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return McpClientError::Unauthorized;
    }
    match jsonrpc_error(body) {
        Some(error) => map_jsonrpc_error(error),
        None => http_status_error(status, body),
    }
}

/// The `scope` of an `insufficient_scope` challenge in a failed response: a
/// 401 or 403 carrying `WWW-Authenticate: Bearer error="insufficient_scope"`.
/// The outer option is whether the challenge is there, so a refusal that
/// names no scope is still not an ordinary 401. Every challenge header is
/// read, because a server may send one per scheme.
fn insufficient_scope_challenge(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Option<Option<String>> {
    if !matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return None;
    }
    headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|challenge| {
            auth_param(challenge, "error")?
                .eq_ignore_ascii_case("insufficient_scope")
                .then(|| auth_param(challenge, "scope"))
        })
}

/// An HTTP failure with the leading characters of the body's text as its
/// detail.
fn http_status_error(status: reqwest::StatusCode, body: &[u8]) -> McpClientError {
    McpClientError::HttpStatus {
        status: status.as_u16(),
        detail: String::from_utf8_lossy(body)
            .chars()
            .take(MAX_DETAIL_CHARS)
            .collect(),
    }
}

/// The string entries under one key of a JSON-RPC error's `data`.
fn string_list(data: Option<&Value>, key: &str) -> Vec<String> {
    data.and_then(|data| data.get(key))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a response body is an event stream.
fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

/// The subschema keywords that break static reachability: an `x-mcp-header`
/// below one of them makes the tool invalid.
const NON_PROPERTY_SUBSCHEMAS: &[&str] = &[
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "patternProperties",
    "propertyNames",
    "dependentSchemas",
    "unevaluatedProperties",
    "unevaluatedItems",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];

/// The mirrored-parameter headers one tool's schema declares, as
/// `(property path, header name)` pairs, or the reason the tool is unusable.
/// A tool with an invalid annotation, or with no input schema at all, is
/// excluded from `tools/list`.
fn header_specs(schema: &Value) -> Result<Vec<(String, String)>, String> {
    if !schema.is_object() {
        return Err("the tool declares no input schema".to_string());
    }
    let mut specs = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    collect_headers(schema, "", true, &mut specs, &mut seen)?;
    Ok(specs)
}

/// Walks a schema collecting `x-mcp-header` annotations. `reachable` is false
/// once the walk has left the `properties` chain, where an annotation is
/// invalid.
fn collect_headers(
    node: &Value,
    path: &str,
    reachable: bool,
    specs: &mut Vec<(String, String)>,
    seen: &mut Vec<String>,
) -> Result<(), String> {
    let Some(object) = node.as_object() else {
        return Ok(());
    };
    if let Some(annotation) = object.get(HEADER_ANNOTATION) {
        if !reachable {
            return Err(format!(
                "{HEADER_ANNOTATION} is not statically reachable through properties"
            ));
        }
        let name = annotation
            .as_str()
            .ok_or_else(|| format!("{HEADER_ANNOTATION} is not a string"))?;
        if name.is_empty() {
            return Err(format!("{HEADER_ANNOTATION} is empty"));
        }
        if !name.bytes().all(is_tchar) {
            return Err(format!(
                "{HEADER_ANNOTATION} {name} is not an HTTP header name"
            ));
        }
        if seen
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            return Err(format!("{HEADER_ANNOTATION} {name} is used more than once"));
        }
        match object.get("type").and_then(Value::as_str) {
            Some("string") | Some("integer") | Some("boolean") => {}
            Some(other) => {
                return Err(format!(
                    "{HEADER_ANNOTATION} {name} is on a {other} parameter; only string, integer, and boolean are allowed"
                ));
            }
            None => {
                return Err(format!(
                    "{HEADER_ANNOTATION} {name} is on a parameter with no type"
                ));
            }
        }
        if object.get("type").and_then(Value::as_str) == Some("integer") {
            check_integer_range(node, name)?;
        }
        seen.push(name.to_string());
        specs.push((path.to_string(), name.to_string()));
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            collect_headers(child, &child_path, true, specs, seen)?;
        }
    }
    for keyword in NON_PROPERTY_SUBSCHEMAS {
        match object.get(*keyword) {
            Some(Value::Array(items)) => {
                for item in items {
                    collect_headers(item, path, false, specs, seen)?;
                }
            }
            Some(child) => collect_headers(child, path, false, specs, seen)?,
            None => {}
        }
    }
    Ok(())
}

/// Whether a byte may appear in an HTTP field name, per RFC 9110's `tchar`.
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// A mirrored integer parameter's declared values must be inside the safe
/// integer range; an out-of-range one makes the tool invalid.
fn check_integer_range(property: &Value, name: &str) -> Result<(), String> {
    let mut values: Vec<&Value> = Vec::new();
    if let Some(default) = property.get("default") {
        values.push(default);
    }
    if let Some(constant) = property.get("const") {
        values.push(constant);
    }
    if let Some(entries) = property.get("enum").and_then(Value::as_array) {
        values.extend(entries.iter());
    }
    for value in values {
        if let Some(integer) = value.as_i64()
            && !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&integer)
        {
            return Err(format!(
                "{HEADER_ANNOTATION} {name} declares an integer outside the safe range"
            ));
        }
    }
    Ok(())
}

/// The `Mcp-Param-*` headers one call carries: each annotated parameter whose
/// value the arguments hold at its path, converted and encoded.
fn mirrored_headers(
    schema: &Value,
    arguments: &Value,
) -> Result<Vec<(String, String)>, McpClientError> {
    let specs =
        header_specs(schema).map_err(|reason| McpClientError::Protocol { detail: reason })?;
    let mut headers = Vec::new();
    for (path, name) in specs {
        let Some(value) = value_at_path(arguments, &path) else {
            continue;
        };
        let Some(value) = header_value(value) else {
            continue;
        };
        headers.push((format!("{PARAM_HEADER_PREFIX}{name}"), value));
    }
    Ok(headers)
}

/// The value at a dot-separated path of object keys, or None when any step is
/// absent.
fn value_at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    Some(current)
}

/// One mirrored parameter's header value: the value as text per its type,
/// then encoded so it is a safe header value.
fn header_value(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.to_string(),
            None => number.to_string(),
        },
        _ => return None,
    };
    Some(encode_header_value(&raw))
}

/// `raw` itself when it is a plain ASCII header value that does not look like
/// the Base64 sentinel, else the sentinel-wrapped Base64 of its UTF-8 bytes.
fn encode_header_value(raw: &str) -> String {
    let plain = !raw.is_empty()
        && !raw.starts_with(' ')
        && !raw.ends_with(' ')
        && raw
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) || matches!(byte, 0x20 | 0x09))
        && !(raw.starts_with(BASE64_SENTINEL_PREFIX) && raw.ends_with(BASE64_SENTINEL_SUFFIX));
    if plain {
        return raw.to_string();
    }
    format!(
        "{BASE64_SENTINEL_PREFIX}{}{BASE64_SENTINEL_SUFFIX}",
        crate::mcp_oauth::standard_base64(raw.as_bytes())
    )
}

/// One JSON-RPC error.
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

/// One tool in a `tools/list` result.
#[derive(Debug, Deserialize)]
struct ToolJson {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// A tool that declares no schema is skipped rather than failing the
    /// page, so one malformed definition cannot hide the other tools.
    #[serde(default, rename = "inputSchema")]
    input_schema: Value,
}

/// One page of a `tools/list` result.
#[derive(Debug, Deserialize)]
struct ToolsListResult {
    tools: Vec<ToolJson>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
    /// Signed because the spec allows a negative value, which means
    /// immediately stale.
    #[serde(default, rename = "ttlMs")]
    ttl_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use bosun_test_support::wait_for;

    use super::*;
    use crate::mcp_test_support::StubReply;
    use crate::mcp_test_support::accepted;
    use crate::mcp_test_support::client_at;
    use crate::mcp_test_support::discover_ok;
    use crate::mcp_test_support::empty_log;
    use crate::mcp_test_support::error_message;
    use crate::mcp_test_support::json_ok;
    use crate::mcp_test_support::notification;
    use crate::mcp_test_support::plain_tool;
    use crate::mcp_test_support::result_message;
    use crate::mcp_test_support::sse_ok;
    use crate::mcp_test_support::sse_stub;
    use crate::mcp_test_support::stub_endpoint;
    use crate::mcp_test_support::text_status;

    #[tokio::test]
    async fn the_modern_probe_selects_the_current_revision() {
        let log = empty_log();
        let url = stub_endpoint(discover_ok, log.clone()).await;
        let mut client = client_at(&url, None);

        let era = client.connect().await.unwrap();

        let modern = ServerEra::Modern {
            version: MODERN_PROTOCOL_VERSION.to_string(),
        };
        assert_eq!(era, modern);
        assert_eq!(client.era(), Some(&modern));

        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.message(), "server/discover");
        assert_eq!(request.method.as_deref(), Some("server/discover"));
        assert_eq!(request.version.as_deref(), Some(MODERN_PROTOCOL_VERSION));
        assert_eq!(
            request.accept.as_deref(),
            Some("application/json, text/event-stream")
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        assert_eq!(
            request.meta("io.modelcontextprotocol/protocolVersion"),
            json!(MODERN_PROTOCOL_VERSION)
        );
        let info = request.meta("io.modelcontextprotocol/clientInfo");
        assert_eq!(info["name"].as_str(), Some("bosun"));
        assert_eq!(info["version"].as_str(), Some(VERSION));
        assert_eq!(
            request.meta("io.modelcontextprotocol/clientCapabilities"),
            json!({})
        );
    }

    #[tokio::test]
    async fn an_unsupported_version_is_retried_with_the_servers_own_version() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" if request.version.as_deref() == Some("2025-11-25") => {
                    json_ok(result_message(
                        request.id(),
                        json!({ "supportedVersions": ["2025-11-25"], "capabilities": {} }),
                    ))
                }
                "server/discover" => json_ok(json!({
                    "jsonrpc": "2.0",
                    "id": request.id(),
                    "error": {
                        "code": UNSUPPORTED_PROTOCOL_VERSION_CODE,
                        "message": "unsupported protocol version",
                        "data": { "supported": ["2025-11-25"] },
                    },
                })),
                "tools/list" => json_ok(result_message(request.id(), json!({ "tools": [] }))),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);

        let era = client.connect().await.unwrap();

        assert_eq!(
            era,
            ServerEra::Modern {
                version: "2025-11-25".to_string()
            }
        );
        {
            let requests = log.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0].version.as_deref(),
                Some(MODERN_PROTOCOL_VERSION)
            );
            assert_eq!(
                requests[0].meta("io.modelcontextprotocol/protocolVersion"),
                json!(MODERN_PROTOCOL_VERSION)
            );
            assert_eq!(requests[1].version.as_deref(), Some("2025-11-25"));
            assert_eq!(
                requests[1].meta("io.modelcontextprotocol/protocolVersion"),
                json!("2025-11-25")
            );
        }

        // The version the server chose is the one the connection keeps using.
        assert!(client.tools_list().await.unwrap().tools.is_empty());
        let requests = log.lock().unwrap();
        assert_eq!(requests[2].message(), "tools/list");
        assert_eq!(requests[2].version.as_deref(), Some("2025-11-25"));
    }

    #[tokio::test]
    async fn a_plain_400_on_the_probe_falls_back_to_the_legacy_handshake() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => text_status(400, "unknown method"),
                "initialize" => json_ok(result_message(
                    request.id(),
                    json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": { "name": "legacy" },
                    }),
                ))
                .with_session_id("session-1"),
                "notifications/initialized" => accepted(),
                "tools/list" => json_ok(result_message(request.id(), json!({ "tools": [] }))),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);

        let era = client.connect().await.unwrap();

        // The revision comes from the initialize result, not from the client.
        assert_eq!(
            era,
            ServerEra::Legacy {
                version: "2025-03-26".to_string()
            }
        );
        {
            let requests = log.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1].message(), "initialize");
            assert_eq!(
                requests[1].param("protocolVersion"),
                json!(LEGACY_INITIALIZE_VERSION)
            );
            assert_eq!(
                requests[1].param("clientInfo")["name"].as_str(),
                Some("bosun")
            );
            assert_eq!(requests[1].param("capabilities"), json!({}));
            assert_eq!(requests[2].message(), "notifications/initialized");
            // The session id from the initialize answer is echoed afterwards,
            // and later requests carry the revision the server chose.
            assert_eq!(requests[2].session_id.as_deref(), Some("session-1"));
            assert_eq!(requests[2].version.as_deref(), Some("2025-03-26"));
        }

        assert!(client.tools_list().await.unwrap().tools.is_empty());
        let requests = log.lock().unwrap();
        assert_eq!(requests[3].session_id.as_deref(), Some("session-1"));
        assert_eq!(requests[3].version.as_deref(), Some("2025-03-26"));
    }

    #[tokio::test]
    async fn tools_list_follows_the_cursor_and_maps_every_tool() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => match request.param("cursor").as_str() {
                    None => json_ok(result_message(
                        request.id(),
                        json!({
                            "tools": [{
                                "name": "first",
                                "description": "does the first thing",
                                "inputSchema": { "type": "object" },
                            }],
                            "nextCursor": "page-2",
                            "ttlMs": 60000,
                        }),
                    )),
                    Some("page-2") => json_ok(result_message(
                        request.id(),
                        json!({
                            "tools": [{
                                "name": "second",
                                "inputSchema": { "type": "object", "properties": {} },
                            }],
                        }),
                    )),
                    Some(other) => panic!("unexpected cursor {other}"),
                },
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();

        let listed = client.tools_list().await.unwrap();

        assert_eq!(
            listed.tools,
            vec![
                McpTool {
                    name: "first".to_string(),
                    description: "does the first thing".to_string(),
                    schema: json!({ "type": "object" }),
                },
                McpTool {
                    name: "second".to_string(),
                    description: String::new(),
                    schema: json!({ "type": "object", "properties": {} }),
                },
            ]
        );
        assert_eq!(listed.ttl_ms, Some(60000));

        let requests = log.lock().unwrap();
        let cursors: Vec<Value> = requests
            .iter()
            .filter(|request| request.message() == "tools/list")
            .map(|request| request.param("cursor"))
            .collect();
        assert_eq!(cursors, vec![Value::Null, json!("page-2")]);
    }

    #[tokio::test]
    async fn tools_call_maps_the_content_and_the_servers_error() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/call" => match request.param("name").as_str() {
                    Some("read") => json_ok(result_message(
                        request.id(),
                        json!({
                            "content": [
                                { "type": "text", "text": "first" },
                                { "type": "image", "data": "x" },
                                { "type": "text", "text": "second" },
                            ],
                            "structuredContent": { "a": [1, 2] },
                            "isError": true,
                        }),
                    )),
                    Some("broken") => {
                        json_ok(error_message(request.id(), -32000, "the tool failed"))
                    }
                    other => panic!("unexpected tool {other:?}"),
                },
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();

        let outcome = client
            .tools_call(&plain_tool("read"), json!({ "path": "a" }), None)
            .await
            .unwrap();

        assert_eq!(outcome.text, "first\n[image]\nsecond\n{\"a\":[1,2]}");
        assert!(outcome.is_error);

        // A JSON-RPC error is the call's outcome, not a transport failure.
        let outcome = client
            .tools_call(&plain_tool("broken"), json!({}), None)
            .await
            .unwrap();
        assert_eq!(outcome.text, "the tool failed");
        assert!(outcome.is_error);

        let requests = log.lock().unwrap();
        let call = requests
            .iter()
            .find(|request| request.message() == "tools/call")
            .unwrap();
        assert_eq!(call.name.as_deref(), Some("read"));
        assert_eq!(call.param("arguments"), json!({ "path": "a" }));
    }

    #[tokio::test]
    async fn tools_call_names_the_tool_and_reads_past_progress_notifications() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/call" => sse_ok(
                    &[
                        notification("notifications/progress"),
                        result_message(
                            request.id(),
                            json!({ "content": [{ "type": "text", "text": "done" }] }),
                        ),
                    ],
                    false,
                ),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();
        let (progress, mut received) = tokio::sync::mpsc::unbounded_channel();

        let outcome = client
            .tools_call(&plain_tool("read"), json!({}), Some(&progress))
            .await
            .unwrap();

        assert_eq!(outcome.text, "done");
        assert!(!outcome.is_error);
        let reported = received.try_recv().unwrap();
        assert_eq!(reported["method"], "notifications/progress");

        let requests = log.lock().unwrap();
        let call = requests
            .iter()
            .find(|request| request.message() == "tools/call")
            .unwrap();
        assert_eq!(call.name.as_deref(), Some("read"));
        assert_eq!(call.param("name"), json!("read"));
        assert!(call.meta("progressToken").is_u64());
    }

    #[tokio::test]
    async fn subscriptions_listen_reports_a_tools_list_change() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(
                    &[
                        notification("notifications/subscriptions/acknowledged"),
                        notification("notifications/tools/list_changed"),
                    ],
                    false,
                ),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = changes.clone();

        client
            .subscriptions_listen(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .unwrap();

        assert_eq!(changes.load(Ordering::SeqCst), 1);
        let requests = log.lock().unwrap();
        let listen = requests
            .iter()
            .find(|request| request.message() == "subscriptions/listen")
            .unwrap();
        assert_eq!(listen.param("notifications")["toolsListChanged"], true);
        assert_eq!(
            listen.meta("io.modelcontextprotocol/protocolVersion"),
            json!(MODERN_PROTOCOL_VERSION)
        );
    }

    #[tokio::test]
    async fn subscriptions_listen_runs_until_the_caller_cancels() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(
                    &[notification("notifications/subscriptions/acknowledged")],
                    true,
                ),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = changes.clone();

        let listening = tokio::spawn(async move {
            client
                .subscriptions_listen(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
        });

        wait_for("the listen stream to open", || {
            let log = log.clone();
            async move {
                log.lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.message() == "subscriptions/listen")
            }
        })
        .await;

        // Dropping the call closes the response stream, which is the only
        // cancellation HTTP has.
        listening.abort();
        assert!(listening.await.unwrap_err().is_cancelled());
        assert_eq!(changes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unknown_method_on_the_probe_falls_back_to_the_legacy_handshake() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                // An unknown method over HTTP is 404 with JSON-RPC -32601,
                // which is what a legacy Streamable HTTP server answers.
                "server/discover" => StubReply {
                    status: 404,
                    content_type: "application/json",
                    body: error_message(request.id(), -32601, "Method not found").to_string(),
                    session_id: None,
                    www_authenticate: None,
                    keep_open: false,
                },
                "initialize" => json_ok(result_message(
                    request.id(),
                    json!({ "protocolVersion": "2025-06-18", "capabilities": {} }),
                )),
                "notifications/initialized" => accepted(),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);

        let era = client.connect().await.unwrap();

        assert_eq!(
            era,
            ServerEra::Legacy {
                version: "2025-06-18".to_string()
            }
        );
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].message(), "initialize");
    }

    #[tokio::test]
    async fn a_required_capability_error_keeps_the_server_modern() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| {
                json_ok(json!({
                    "jsonrpc": "2.0",
                    "id": request.id(),
                    "error": {
                        "code": MISSING_CAPABILITY_CODE,
                        "message": "Server requires the elicitation capability",
                        "data": { "requiredCapabilities": { "elicitation": {} } },
                    },
                }))
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);

        let error = client.connect().await.unwrap_err();

        assert!(matches!(
            &error,
            McpClientError::MissingCapability { required }
                if required == &["elicitation".to_string()]
        ));
        assert_eq!(client.era(), None);
    }

    #[tokio::test]
    async fn tools_list_stops_at_the_page_cap() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                // A server that never stops returning a cursor must not make
                // the listing run forever.
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({ "tools": [], "nextCursor": "more" }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();

        let error = client.tools_list().await.unwrap_err();

        assert!(matches!(error, McpClientError::Protocol { .. }));
        let pages = log
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.message() == "tools/list")
            .count();
        assert_eq!(pages, MAX_TOOL_PAGES);
    }

    #[tokio::test]
    async fn one_client_calls_a_tool_while_it_listens() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(
                    &[notification("notifications/subscriptions/acknowledged")],
                    true,
                ),
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "content": [{ "type": "text", "text": "done" }] }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();
        let client = Arc::new(client);

        let listening = {
            let client = client.clone();
            tokio::spawn(async move { client.subscriptions_listen(|| {}).await.unwrap() })
        };
        wait_for("the listen stream to open", || {
            let log = log.clone();
            async move {
                log.lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.message() == "subscriptions/listen")
            }
        })
        .await;

        // The listen stream holds no exclusive access: a call runs on the
        // same client while it stays open.
        let outcome = client
            .tools_call(&plain_tool("read"), json!({}), None)
            .await
            .unwrap();

        assert_eq!(outcome.text, "done");
        listening.abort();
        assert!(listening.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn a_401_returns_unauthorized() {
        let log = empty_log();
        let url = stub_endpoint(|_| text_status(401, "no token"), log.clone()).await;
        let mut client = client_at(&url, Some("secret-token".to_string()));

        let error = client.connect().await.unwrap_err();

        assert!(matches!(error, McpClientError::Unauthorized));
        assert_eq!(
            log.lock().unwrap()[0].authorization.as_deref(),
            Some("Bearer secret-token")
        );
    }

    /// The challenge a server sends when the token carries too few scopes.
    const INSUFFICIENT_SCOPE: &str =
        "Bearer error=\"insufficient_scope\", scope=\"files.read files.write\"";

    #[tokio::test]
    async fn an_insufficient_scope_challenge_carries_its_scope_from_a_401_or_a_403() {
        for status in [401, 403] {
            let url = stub_endpoint(
                move |_| {
                    text_status(status, "insufficient scope")
                        .with_www_authenticate(INSUFFICIENT_SCOPE)
                },
                empty_log(),
            )
            .await;
            let mut client = client_at(&url, Some("token".to_string()));

            let error = client.connect().await.unwrap_err();

            assert!(
                matches!(
                    &error,
                    McpClientError::InsufficientScope { scope }
                        if scope.as_deref() == Some("files.read files.write")
                ),
                "unexpected error for {status}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_insufficient_scope_challenge_without_a_scope_reports_none() {
        let url = stub_endpoint(
            |_| {
                text_status(403, "insufficient scope")
                    .with_www_authenticate("Bearer error=\"insufficient_scope\"")
            },
            empty_log(),
        )
        .await;
        let mut client = client_at(&url, Some("token".to_string()));

        let error = client.connect().await.unwrap_err();

        assert!(
            matches!(&error, McpClientError::InsufficientScope { scope } if scope.is_none()),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_challenge_is_found_among_several_header_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            reqwest::header::WWW_AUTHENTICATE,
            reqwest::header::HeaderValue::from_static("Basic realm=\"mcp\""),
        );
        headers.append(
            reqwest::header::WWW_AUTHENTICATE,
            reqwest::header::HeaderValue::from_static(
                "Bearer error=\"insufficient_scope\", scope=\"files.read\"",
            ),
        );

        assert_eq!(
            insufficient_scope_challenge(reqwest::StatusCode::FORBIDDEN, &headers),
            Some(Some("files.read".to_string()))
        );
        // A challenge only counts on the statuses the spec sends it with.
        assert_eq!(
            insufficient_scope_challenge(reqwest::StatusCode::OK, &headers),
            None
        );
        // The parameters of a challenge in another scheme name a credential
        // the bearer token cannot answer.
        let mut another_scheme = reqwest::header::HeaderMap::new();
        another_scheme.append(
            reqwest::header::WWW_AUTHENTICATE,
            reqwest::header::HeaderValue::from_static(
                "Basic error=\"insufficient_scope\", scope=\"files.read\"",
            ),
        );
        assert_eq!(
            insufficient_scope_challenge(reqwest::StatusCode::UNAUTHORIZED, &another_scheme),
            None
        );
    }

    #[tokio::test]
    async fn a_challenge_in_another_scheme_stays_unauthorized() {
        let url = stub_endpoint(
            |_| {
                text_status(401, "bad token").with_www_authenticate(
                    "Basic error=\"insufficient_scope\", scope=\"files.read\"",
                )
            },
            empty_log(),
        )
        .await;
        let mut client = client_at(&url, Some("token".to_string()));

        let error = client.connect().await.unwrap_err();

        // The parameters belong to a Basic challenge, which names a credential
        // this token cannot answer, so no step-up is started for it.
        assert!(
            matches!(&error, McpClientError::Unauthorized),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_bearer_challenge_for_another_error_stays_unauthorized() {
        let url = stub_endpoint(
            |_| {
                text_status(401, "bad token")
                    .with_www_authenticate("Bearer error=\"invalid_token\", scope=\"files.read\"")
            },
            empty_log(),
        )
        .await;
        let mut client = client_at(&url, Some("token".to_string()));

        let error = client.connect().await.unwrap_err();

        assert!(
            matches!(&error, McpClientError::Unauthorized),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_server_without_the_streamable_transport_handshakes_over_sse() {
        let log = empty_log();
        let (url, _) = sse_stub(log.clone(), false).await;
        let mut client = client_at(&url, Some("server-token".to_string()));

        let era = client.connect().await.unwrap();

        assert_eq!(
            era,
            ServerEra::LegacySse {
                version: LEGACY_SSE_VERSION.to_string()
            }
        );
        {
            let requests = log.lock().unwrap();
            // Only messages POSTed to the endpoint event's URL are logged, so
            // these two prove the handshake ran over the event stream.
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].message(), "initialize");
            assert_eq!(
                requests[0].param("protocolVersion"),
                json!(LEGACY_INITIALIZE_VERSION)
            );
            assert_eq!(
                requests[0].param("clientInfo")["name"].as_str(),
                Some("bosun")
            );
            assert_eq!(requests[1].message(), "notifications/initialized");
            // The deprecated transport still carries the bearer token.
            assert_eq!(
                requests[0].authorization.as_deref(),
                Some("Bearer server-token")
            );
            assert_eq!(
                requests[1].authorization.as_deref(),
                Some("Bearer server-token")
            );
        }
    }

    #[tokio::test]
    async fn a_request_over_a_closed_event_stream_fails_instead_of_waiting() {
        let log = empty_log();
        // The server ends the event stream once the handshake is answered,
        // which is routine, so nothing is left to carry a later response.
        let (url, open) = sse_stub(log.clone(), true).await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();
        wait_for("the event stream to end", || {
            let open = open.clone();
            async move { open.load(Ordering::SeqCst) == 0 }
        })
        .await;

        let listed = tokio::time::timeout(Duration::from_secs(5), client.tools_list()).await;

        let error = listed
            .expect("a closed event stream must fail the request, not hold it")
            .unwrap_err();
        assert!(matches!(error, McpClientError::Protocol { .. }), "{error}");
    }

    #[tokio::test]
    async fn dropping_the_client_ends_the_deprecated_transports_reader() {
        let log = empty_log();
        let (url, open) = sse_stub(log.clone(), false).await;
        // The HTTP client carries no timeout, so the GET stream stays open
        // until the client ends it and only the reader's lifetime is tested.
        let mut client = McpClient::new(reqwest::Client::new(), &url, None);
        client.connect().await.unwrap();
        wait_for("the event stream to open", || {
            let open = open.clone();
            async move { open.load(Ordering::SeqCst) == 1 }
        })
        .await;

        drop(client);

        // Replacing a connection drops its client, and the reader task holds
        // the open GET stream, so the reader must end with the client.
        wait_for("the event stream to close", || {
            let open = open.clone();
            async move { open.load(Ordering::SeqCst) == 0 }
        })
        .await;
    }

    #[test]
    fn header_specs_accepts_statically_reachable_annotations() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": { "type": "string", "x-mcp-header": "Region" },
                "filter": {
                    "type": "object",
                    "properties": {
                        "tenant": { "type": "integer", "x-mcp-header": "Tenant" }
                    }
                }
            }
        });
        assert_eq!(
            header_specs(&schema).unwrap(),
            vec![
                ("filter.tenant".to_string(), "Tenant".to_string()),
                ("region".to_string(), "Region".to_string()),
            ],
            "properties come in the schema's key order"
        );
    }

    #[test]
    fn header_specs_rejects_every_invalid_annotation() {
        let cases = [
            (
                json!({ "properties": { "p": { "type": "string", "x-mcp-header": "" } } }),
                "an empty name",
            ),
            (
                json!({ "properties": { "p": { "type": "number", "x-mcp-header": "P" } } }),
                "a number parameter",
            ),
            (
                json!({ "properties": { "p": { "type": "string", "x-mcp-header": "Bad Name" } } }),
                "a name that is not a header name",
            ),
            (
                json!({ "items": { "type": "string", "x-mcp-header": "P" } }),
                "an annotation below items",
            ),
            (
                json!({ "oneOf": [{ "type": "string", "x-mcp-header": "P" }] }),
                "an annotation below oneOf",
            ),
            (
                json!({ "properties": { "p": { "x-mcp-header": "P" } } }),
                "a parameter with no type",
            ),
            (
                json!({ "properties": {
                    "a": { "type": "string", "x-mcp-header": "Header" },
                    "b": { "type": "string", "x-mcp-header": "header" }
                } }),
                "a name used twice",
            ),
            (
                json!({ "properties": {
                    "p": { "type": "integer", "default": 9007199254740992i64, "x-mcp-header": "P" }
                } }),
                "an out-of-range integer default",
            ),
        ];
        for (schema, reason) in cases {
            assert!(header_specs(&schema).is_err(), "{reason}: {schema}");
        }
    }

    #[test]
    fn header_values_use_the_base64_sentinel_only_when_needed() {
        assert_eq!(encode_header_value("us-west1"), "us-west1");
        assert_eq!(
            encode_header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(encode_header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            encode_header_value("line1\nline2"),
            "=?base64?bGluZTEKbGluZTI=?="
        );
        assert_eq!(
            encode_header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn mirrored_headers_convert_types_and_omit_absent_arguments() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": { "type": "string", "x-mcp-header": "Region" },
                "retries": { "type": "integer", "x-mcp-header": "Retries" },
                "verbose": { "type": "boolean", "x-mcp-header": "Verbose" }
            }
        });
        assert_eq!(
            mirrored_headers(
                &schema,
                &json!({ "region": "eu", "retries": 3, "verbose": false, "other": "x" }),
            )
            .unwrap(),
            vec![
                ("Mcp-Param-Region".to_string(), "eu".to_string()),
                ("Mcp-Param-Retries".to_string(), "3".to_string()),
                ("Mcp-Param-Verbose".to_string(), "false".to_string()),
            ]
        );
        assert_eq!(
            mirrored_headers(&schema, &json!({ "region": "eu" })).unwrap(),
            vec![("Mcp-Param-Region".to_string(), "eu".to_string())]
        );
    }

    #[test]
    fn an_endpoint_event_may_not_name_another_origin() {
        let client = client_at("http://127.0.0.1:9/mcp", None);

        assert!(
            client
                .resolve_endpoint("http://evil.example/messages")
                .is_err(),
            "another origin must not receive the bearer token"
        );
        assert!(
            client
                .resolve_endpoint("http://127.0.0.1:9/messages")
                .is_ok()
        );
        assert!(client.resolve_endpoint("/messages").is_ok());
    }

    #[tokio::test]
    async fn a_call_sends_the_mirrored_parameter_headers() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "content": [{ "type": "text", "text": "ok" }] }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let client = client_at(&url, None);
        let tool = McpTool {
            name: "execute_sql".to_string(),
            description: String::new(),
            schema: json!({
                "type": "object",
                "properties": { "region": { "type": "string", "x-mcp-header": "Region" } }
            }),
        };

        let outcome = client
            .tools_call(
                &tool,
                json!({ "region": "us-west1", "query": "select 1" }),
                None,
            )
            .await
            .unwrap();

        assert_eq!(outcome.text, "ok");
        let requests = log.lock().unwrap();
        let call = &requests[0];
        assert_eq!(call.name.as_deref(), Some("execute_sql"));
        assert_eq!(call.mirrored("Region"), Some("us-west1"));
        // The parameter stays in the body as well as in the header.
        assert_eq!(call.param("arguments")["query"].as_str(), Some("select 1"));
    }

    #[tokio::test]
    async fn an_incomplete_result_type_is_a_protocol_error() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "resultType": "input_required" }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let client = client_at(&url, None);

        let error = client
            .tools_call(&plain_tool("read"), json!({}), None)
            .await
            .unwrap_err();

        assert!(matches!(error, McpClientError::Protocol { .. }), "{error}");
    }

    #[tokio::test]
    async fn an_invalid_annotation_excludes_the_tool_from_the_listing() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [
                            {
                                "name": "good",
                                "description": "d",
                                "inputSchema": { "type": "object" }
                            },
                            {
                                "name": "bad",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "p": { "type": "number", "x-mcp-header": "P" }
                                    }
                                }
                            },
                            { "name": "no-schema" }
                        ]
                    }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let client = client_at(&url, None);

        let listed = client.tools_list().await.unwrap();

        let names: Vec<&str> = listed.tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            ["good"],
            "an invalid annotation excludes only its tool"
        );
    }

    #[tokio::test]
    async fn a_negative_ttl_means_immediately_stale() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({ "tools": [], "ttlMs": -1 }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let client = client_at(&url, None);

        assert_eq!(client.tools_list().await.unwrap().ttl_ms, Some(0));
    }

    #[tokio::test]
    async fn a_non_json_probe_body_falls_back_to_the_legacy_handshake() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => text_status(200, "<html>not an MCP server</html>"),
                "initialize" => json_ok(result_message(
                    request.id(),
                    json!({ "protocolVersion": "2025-03-26" }),
                )),
                "notifications/initialized" => accepted(),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);

        let era = client.connect().await.unwrap();

        assert_eq!(
            era,
            ServerEra::Legacy {
                version: "2025-03-26".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_header_mismatch_error_keeps_the_server_modern() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => text_status(
                    400,
                    &error_message(request.id(), HEADER_MISMATCH_CODE, "header mismatch")
                        .to_string(),
                ),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);

        let error = client.connect().await.unwrap_err();

        assert!(
            matches!(error, McpClientError::JsonRpc { code, .. } if code == HEADER_MISMATCH_CODE),
            "{error}"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "a modern error must not trigger the legacy handshake"
        );
    }

    #[tokio::test]
    async fn a_404_with_a_session_id_reports_an_expired_session() {
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => text_status(400, "unknown method"),
                "initialize" => json_ok(result_message(
                    request.id(),
                    json!({ "protocolVersion": "2025-03-26" }),
                ))
                .with_session_id("session-1"),
                "notifications/initialized" => accepted(),
                "tools/list" => text_status(404, "no such session"),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let mut client = client_at(&url, None);
        client.connect().await.unwrap();

        let error = client.tools_list().await.unwrap_err();

        assert!(matches!(error, McpClientError::SessionExpired), "{error}");
    }
}
