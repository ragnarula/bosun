//! The executor's local HTTP API: one axum server per session that turns
//! tool requests into calls against the session's working copy.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::anyhow;
use axum::Json;
use axum::Router;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::Event;
use axum::response::sse::Sse;
use axum::routing::get;
use axum::routing::post;
use bosun_common::error::ErrorExt;
use bosun_common::session::Permission;
use bosun_common::tool::ToolRequest;
use futures_util::StreamExt;
use futures_util::stream::unfold;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::error;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use crate::tools::ToolError;
use crate::tools::{self};

/// Cap on the total bytes streamed as `out` events for one shell run. Output
/// past the cap is still drained from the pipes but not forwarded.
const MAX_SHELL_OUTPUT_BYTES: usize = 1 << 20;
/// After the shell exits, keep forwarding buffered output for this long even
/// if a backgrounded grandchild still holds the pipes open.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

pub struct ExecutorState {
    pub session_dir: PathBuf,
    pub permission: RwLock<Permission>,
    pub running: RwLock<HashMap<String, RunningShell>>,
}

pub struct RunningShell {
    pub pid: u32,
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to serve executor")]
    Internal(#[from] anyhow::Error),
}

/// One streamed shell output line, or a signal that a pipe was drained.
enum ShellMsg {
    Out(String),
    ReaderDone,
}

/// Owns the shell's process-group id for the lifetime of the SSE stream. On
/// drop the whole group is killed and the run is removed from the running
/// map, so an aborted client leaks nothing.
struct ShellGuard {
    pid: u32,
    state: Arc<ExecutorState>,
    run_id: String,
}

impl Drop for ShellGuard {
    fn drop(&mut self) {
        let pid = self.pid;
        let state = self.state.clone();
        let run_id = self.run_id.clone();
        tokio::spawn(async move {
            kill_process_tree(pid).await;
            state.running.write().await.remove(&run_id);
        });
    }
}

/// Kills a shell and its children. On Unix the shell runs in its own process
/// group, so killing the negative pid kills the whole tree. On Windows there
/// are no process groups; taskkill with /T covers the tree.
async fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// Read a pipe until it closes, coalescing chunks into newline-terminated
/// lines and sending each as an Out message, then a ReaderDone signal. Once
/// the total streamed output passes MAX_SHELL_OUTPUT_BYTES, lines are dropped
/// instead of sent, but the pipe keeps being drained so the process is not
/// blocked.
async fn pump(
    mut reader: impl AsyncRead + Unpin,
    tx: mpsc::Sender<ShellMsg>,
    total: Arc<AtomicUsize>,
) {
    let mut buf = [0u8; 8192];
    let mut pending = String::new();
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        pending.push_str(&String::from_utf8_lossy(&buf[..n]));
        while let Some(index) = pending.find('\n') {
            let line: String = pending.drain(..=index).collect();
            let seen = total.fetch_add(line.len(), Ordering::Relaxed);
            if seen < MAX_SHELL_OUTPUT_BYTES && tx.send(ShellMsg::Out(line)).await.is_err() {
                return;
            }
        }
    }
    if !pending.is_empty() {
        let seen = total.fetch_add(pending.len(), Ordering::Relaxed);
        if seen < MAX_SHELL_OUTPUT_BYTES && tx.send(ShellMsg::Out(pending)).await.is_err() {
            return;
        }
    }
    let _ = tx.send(ShellMsg::ReaderDone).await;
}

pub fn router(session_dir: PathBuf, permission: Permission) -> Router {
    router_with_state(Arc::new(ExecutorState {
        session_dir,
        permission: RwLock::new(permission),
        running: RwLock::new(HashMap::new()),
    }))
}

fn router_with_state(state: Arc<ExecutorState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/permission", post(set_permission))
        .route("/tool/shell", post(shell))
        .route("/tool/file/read", post(tool_read))
        .route("/tool/file/write", post(tool_write))
        .route("/tool/edit", post(tool_edit))
        .route("/tool/grep", post(tool_grep))
        .route("/tool/glob", post(tool_glob))
        .route("/tool/git", post(tool_git))
        .route("/tool/webfetch", post(tool_webfetch))
        .route("/tool/{run_id}/cancel", post(cancel))
        .fallback(not_found)
        .with_state(state)
}

pub async fn serve(
    session_dir: PathBuf,
    port: u16,
    permission: Permission,
) -> Result<(), ServerError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("failed to bind executor on port {port}"))?;
    info!(port, dir = %session_dir.display(), "executor listening");
    axum::serve(listener, router(session_dir, permission))
        .await
        .with_context(|| "failed to serve executor")?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct PermissionBody {
    permission: Permission,
}

#[instrument(skip_all)]
async fn set_permission(
    State(state): State<Arc<ExecutorState>>,
    Json(body): Json<PermissionBody>,
) -> Response {
    *state.permission.write().await = body.permission;
    Json(json!({})).into_response()
}

#[instrument(skip_all)]
async fn tool_read(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "file/read", req).await
}

#[instrument(skip_all)]
async fn tool_write(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "file/write", req).await
}

#[instrument(skip_all)]
async fn tool_edit(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "edit", req).await
}

#[instrument(skip_all)]
async fn tool_grep(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "grep", req).await
}

#[instrument(skip_all)]
async fn tool_glob(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "glob", req).await
}

#[instrument(skip_all)]
async fn tool_git(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "git", req).await
}

#[instrument(skip_all)]
async fn tool_webfetch(
    State(state): State<Arc<ExecutorState>>,
    Json(req): Json<ToolRequest>,
) -> Response {
    tool(state, "webfetch", req).await
}

/// Shared dispatcher for the JSON tools. Shell is handled separately because
/// it returns an SSE stream.
async fn tool(state: Arc<ExecutorState>, name: &str, req: ToolRequest) -> Response {
    let permission = *state.permission.read().await;
    let args = &req.args;

    let result = match name {
        "file/read" => {
            let Some(path) = args.get("path").and_then(Value::as_str) else {
                return bad_arg("path");
            };
            tools::read_file(&state.session_dir, path).map(|content| json!({ "content": content }))
        }
        "file/write" => {
            if permission != Permission::ReadWrite {
                return tool_error_response(
                    "file/write",
                    ToolError::ReadOnly { tool: "file/write" },
                );
            }
            let (Some(path), Some(content)) = (
                args.get("path").and_then(Value::as_str),
                args.get("content").and_then(Value::as_str),
            ) else {
                return bad_arg("path or content");
            };
            tools::write_file(&state.session_dir, path, content).map(|()| json!({}))
        }
        "edit" => {
            if permission != Permission::ReadWrite {
                return tool_error_response("edit", ToolError::ReadOnly { tool: "edit" });
            }
            let (Some(path), Some(old), Some(new)) = (
                args.get("path").and_then(Value::as_str),
                args.get("old").and_then(Value::as_str),
                args.get("new").and_then(Value::as_str),
            ) else {
                return bad_arg("path, old or new");
            };
            tools::edit(&state.session_dir, path, old, new).map(|()| json!({ "replaced": true }))
        }
        "grep" => {
            let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
                return bad_arg("pattern");
            };
            let path = args.get("path").and_then(Value::as_str);
            tools::grep(&state.session_dir, pattern, path)
                .map(|matches| json!({ "matches": matches }))
        }
        "glob" => {
            let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
                return bad_arg("pattern");
            };
            tools::glob(&state.session_dir, pattern).map(|paths| json!({ "paths": paths }))
        }
        "git" => {
            let Some(git_args) = args.get("args").and_then(Value::as_array) else {
                return bad_arg("args");
            };
            let mut parsed = Vec::with_capacity(git_args.len());
            for value in git_args {
                let Some(arg) = value.as_str() else {
                    return bad_arg("args must be strings");
                };
                parsed.push(arg.to_string());
            }
            tools::git(&state.session_dir, permission, &parsed)
                .await
                .map(|out| json!({ "stdout": out.stdout, "stderr": out.stderr, "exit_code": out.exit_code }))
        }
        "webfetch" => {
            let Some(url) = args.get("url").and_then(Value::as_str) else {
                return bad_arg("url");
            };
            tools::webfetch(url)
                .await
                .map(|content| json!({ "content": content }))
        }
        _ => return json_response(StatusCode::BAD_REQUEST, &format!("unknown tool {name}")),
    };

    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => tool_error_response(name, e),
    }
}

#[instrument(skip_all)]
async fn shell(State(state): State<Arc<ExecutorState>>, Json(req): Json<ToolRequest>) -> Response {
    if *state.permission.read().await != Permission::ReadWrite {
        return tool_error_response("shell", ToolError::ReadOnly { tool: "shell" });
    }
    let Some(command) = req.args.get("command").and_then(Value::as_str) else {
        return bad_arg("command");
    };
    if command.is_empty() {
        return bad_arg("command");
    }
    let mut shell = Command::new("sh");
    shell
        .arg("-c")
        .arg(command)
        .current_dir(&state.session_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Run the shell in its own process group so cancel can kill it and its
    // children at once. Windows has no process groups; taskkill covers it.
    #[cfg(unix)]
    shell.process_group(0);
    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(e) => return tool_error_response("shell", ToolError::Internal(anyhow!(e))),
    };
    let Some(pid) = child.id() else {
        return tool_error_response(
            "shell",
            ToolError::Internal(anyhow!("spawned shell reported no pid")),
        );
    };
    let stdout = child.stdout.take().expect("shell stdout is piped");
    let stderr = child.stderr.take().expect("shell stderr is piped");
    let run_id = req.run_id.clone();
    // Register the run before returning the response, so a cancel arriving
    // right after the POST returns still finds it.
    state
        .running
        .write()
        .await
        .insert(run_id.clone(), RunningShell { pid });

    // Wait for the shell in a task: the stream must not hang on the pipes
    // when a backgrounded grandchild inherited them and keeps them open.
    let (exit_tx, exit_rx) = oneshot::channel();
    tokio::spawn(async move {
        let code = child
            .wait()
            .await
            .ok()
            .and_then(|status| status.code())
            .unwrap_or(-1);
        let _ = exit_tx.send(code);
    });

    let (tx, rx) = mpsc::channel::<ShellMsg>(64);
    let total = Arc::new(AtomicUsize::new(0));
    tokio::spawn(pump(stdout, tx.clone(), total.clone()));
    tokio::spawn(pump(stderr, tx, total));

    let guard = ShellGuard {
        pid,
        state: state.clone(),
        run_id,
    };
    let stream = unfold(
        StreamState {
            rx,
            exit: exit_rx,
            _guard: guard,
            readers_left: 2,
            rx_closed: false,
            exit_code: None,
            drain_deadline: None,
            done_sent: false,
        },
        stream_step,
    );
    Sse::new(stream.map(Ok::<_, Infallible>)).into_response()
}

/// Mutable state threaded through the SSE stream of one shell run. `_guard`
/// is held only for its Drop side effect: killing the process group and
/// removing the run when the stream ends.
struct StreamState {
    rx: mpsc::Receiver<ShellMsg>,
    exit: oneshot::Receiver<i32>,
    _guard: ShellGuard,
    /// Pipes that have not EOF'd yet (stdout and stderr).
    readers_left: usize,
    rx_closed: bool,
    exit_code: Option<i32>,
    /// When the shell exited but the pipes are still open, the moment after
    /// which the stream ends regardless.
    drain_deadline: Option<tokio::time::Instant>,
    done_sent: bool,
}

fn done_event(code: i32) -> Event {
    Event::default().event("done").data(code.to_string())
}

async fn stream_step(mut st: StreamState) -> Option<(Event, StreamState)> {
    loop {
        if st.done_sent {
            return None;
        }
        // Both pipes drained and the shell exited: done.
        if st.exit_code.is_some() && st.readers_left == 0 {
            st.done_sent = true;
            return Some((done_event(st.exit_code.unwrap_or(-1)), st));
        }
        // The shell exited but a backgrounded grandchild may still hold the
        // pipes open; after the grace period end the stream regardless.
        if let Some(deadline) = st.drain_deadline
            && tokio::time::Instant::now() >= deadline
        {
            st.done_sent = true;
            return Some((done_event(st.exit_code.unwrap_or(-1)), st));
        }
        let drain = st.drain_deadline;
        tokio::select! {
            biased;
            msg = st.rx.recv(), if !st.rx_closed => {
                match msg {
                    Some(ShellMsg::Out(text)) => {
                        return Some((Event::default().event("out").data(text), st));
                    }
                    Some(ShellMsg::ReaderDone) => {
                        st.readers_left = st.readers_left.saturating_sub(1);
                    }
                    None => {
                        st.rx_closed = true;
                        st.readers_left = 0;
                    }
                }
            }
            code = &mut st.exit, if st.exit_code.is_none() => {
                st.exit_code = Some(code.unwrap_or(-1));
                if st.drain_deadline.is_none() {
                    st.drain_deadline = Some(tokio::time::Instant::now() + DRAIN_GRACE);
                }
            }
            _ = tokio::time::sleep_until(
                drain.unwrap_or_else(tokio::time::Instant::now),
            ),
                if drain.is_some() =>
            {
                // The deadline elapsed; the top of the loop emits done.
            }
        }
    }
}

#[instrument(skip_all)]
async fn cancel(
    State(state): State<Arc<ExecutorState>>,
    AxumPath(run_id): AxumPath<String>,
) -> Json<Value> {
    let pid = {
        let running = state.running.read().await;
        running.get(&run_id).map(|shell| shell.pid)
    };
    if let Some(pid) = pid {
        kill_process_tree(pid).await;
    }
    Json(json!({}))
}

async fn not_found() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

fn bad_arg(key: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        &format!("missing or invalid argument {key}"),
    )
}

fn json_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn tool_error_status(e: &ToolError) -> StatusCode {
    match e {
        ToolError::ReadOnly { .. } => StatusCode::FORBIDDEN,
        ToolError::NotFound { .. }
        | ToolError::OldTextNotFound
        | ToolError::FileTooLarge { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        ToolError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

fn tool_error_response(name: &str, e: ToolError) -> Response {
    let status = tool_error_status(&e);
    let message = e.to_string();
    match &e {
        ToolError::Internal(error) => error!(
            error = %error.display_chain(),
            tool = %name,
            "tool call failed with an internal error"
        ),
        _ => warn!(error = %e.display_chain(), tool = %name, "tool call failed"),
    }
    json_response(status, &message)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    /// Boots the executor router on an ephemeral loopback port, returning the
    /// address and the shared state so tests can inspect the running map. The
    /// caller owns the session dir and must keep it alive for the whole test.
    async fn test_app(dir: &Path, permission: Permission) -> (SocketAddr, Arc<ExecutorState>) {
        let state = Arc::new(ExecutorState {
            session_dir: dir.to_path_buf(),
            permission: RwLock::new(permission),
            running: RwLock::new(HashMap::new()),
        });
        let app = router_with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, state)
    }

    /// POST one tool request in the standard envelope and return the response.
    async fn tool_call(
        client: &reqwest::Client,
        addr: &SocketAddr,
        tool: &str,
        run_id: &str,
        args: Value,
    ) -> reqwest::Response {
        client
            .post(format!("http://{addr}/tool/{tool}"))
            .json(&json!({ "run_id": run_id, "args": args }))
            .send()
            .await
            .unwrap()
    }

    /// POST a shell command and read its whole SSE stream.
    async fn shell_stream(
        client: &reqwest::Client,
        addr: &SocketAddr,
        run_id: &str,
        command: &str,
    ) -> String {
        let response =
            tool_call(client, addr, "shell", run_id, json!({ "command": command })).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.text().await.unwrap()
    }

    /// Split SSE wire text into (event, data) pairs, one per block.
    fn sse_events(text: &str) -> Vec<(String, String)> {
        let mut events = Vec::new();
        for block in text.split("\n\n") {
            if block.is_empty() {
                continue;
            }
            let mut event = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    event = value.to_string();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
            }
            events.push((event, data));
        }
        events
    }

    /// Serves one `ok` response per connection until the listener is dropped.
    async fn stub_backend() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    fn git_quiet(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git_quiet(dir, &["init", "-q"]);
        git_quiet(dir, &["config", "user.name", "test"]);
        git_quiet(dir, &["config", "user.email", "test@example.com"]);
    }

    #[test]
    fn tool_error_status_maps_every_variant_to_http() {
        assert_eq!(
            tool_error_status(&ToolError::ReadOnly { tool: "shell" }),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            tool_error_status(&ToolError::NotFound { path: "x".into() }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            tool_error_status(&ToolError::OldTextNotFound),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            tool_error_status(&ToolError::FileTooLarge { path: "x".into() }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            tool_error_status(&ToolError::PathOutsideRoot { path: "x".into() }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            tool_error_status(&ToolError::TooManyMatches { limit: 500 }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            tool_error_status(&ToolError::TooManyResults { limit: 1000 }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            tool_error_status(&ToolError::UnsupportedUrl {
                url: "ftp://x".into()
            }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            tool_error_status(&ToolError::GitVerbNotAllowed {
                verb: "push".into()
            }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            tool_error_status(&ToolError::GitPushForbidden),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            tool_error_status(&ToolError::Internal(anyhow!("boom"))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let response = tool_call(
            &client,
            &addr,
            "file/write",
            "run-1",
            json!({ "path": "hello.txt", "content": "hi there" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(
            &client,
            &addr,
            "file/read",
            "run-2",
            json!({ "path": "hello.txt" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["content"], "hi there");
    }

    #[tokio::test]
    async fn edit_replaces_and_errors_on_missing_old() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let response = tool_call(
            &client,
            &addr,
            "file/write",
            "run-1",
            json!({ "path": "f.txt", "content": "hello world" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(
            &client,
            &addr,
            "edit",
            "run-2",
            json!({ "path": "f.txt", "old": "world", "new": "there" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["replaced"], true);

        let response = tool_call(
            &client,
            &addr,
            "file/read",
            "run-3",
            json!({ "path": "f.txt" }),
        )
        .await;
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["content"], "hello there");

        let response = tool_call(
            &client,
            &addr,
            "edit",
            "run-4",
            json!({ "path": "f.txt", "old": "absent", "new": "x" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn read_only_refuses_mutating_tools() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("needle.txt"), "needle").unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadOnly).await;
        let client = reqwest::Client::new();

        for (tool, args) in [
            ("shell", json!({ "command": "echo hi" })),
            ("file/write", json!({ "path": "f.txt", "content": "x" })),
            ("edit", json!({ "path": "f.txt", "old": "a", "new": "b" })),
        ] {
            let response = tool_call(&client, &addr, tool, "run-1", args).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "tool {tool}");
        }

        let response = tool_call(
            &client,
            &addr,
            "file/read",
            "run-2",
            json!({ "path": "needle.txt" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(
            &client,
            &addr,
            "git",
            "run-3",
            json!({ "args": ["status"] }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(
            &client,
            &addr,
            "grep",
            "run-4",
            json!({ "pattern": "needle" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(&client, &addr, "glob", "run-5", json!({ "pattern": "*" })).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn shell_streams_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let text = shell_stream(&client, &addr, "run-echo", "echo hello").await;
        let events = sse_events(&text);
        assert!(
            events
                .iter()
                .any(|(event, data)| event == "out" && data.contains("hello")),
            "echo hello should stream an out event: {text:?}"
        );
        assert!(
            events
                .iter()
                .any(|(event, data)| event == "done" && data == "0"),
            "echo hello should end with exit code 0: {text:?}"
        );

        let text = shell_stream(&client, &addr, "run-exit", "exit 3").await;
        let events = sse_events(&text);
        assert!(
            events
                .iter()
                .any(|(event, data)| event == "done" && data == "3"),
            "exit 3 should end with exit code 3: {text:?}"
        );
    }

    #[tokio::test]
    async fn shell_cancel_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();
        let run_id = "run-sleep";

        let shell_response = tool_call(
            &client,
            &addr,
            "shell",
            run_id,
            json!({ "command": "sleep 30" }),
        )
        .await;
        assert_eq!(shell_response.status(), StatusCode::OK);

        // The run is registered before the response is returned, so the
        // cancel can go out immediately.
        let cancel_response = client
            .post(format!("http://{addr}/tool/{run_id}/cancel"))
            .send()
            .await
            .unwrap();
        assert_eq!(cancel_response.status(), StatusCode::OK);

        // The stream must end with a killed-run done event within a few
        // seconds; poll instead of sleeping a fixed amount.
        let mut stream = Box::pin(async move { shell_response.text().await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout(Duration::from_millis(200), &mut stream).await {
                Ok(Ok(text)) => {
                    let events = sse_events(&text);
                    assert!(
                        events
                            .iter()
                            .any(|(event, data)| event == "done" && data == "-1"),
                        "a cancelled shell should end with exit code -1: {text:?}"
                    );
                    break;
                }
                Ok(Err(error)) => panic!("failed to read shell stream: {error}"),
                Err(_) if tokio::time::Instant::now() >= deadline => {
                    panic!("shell stream did not end after cancel");
                }
                Err(_) => {}
            }
        }
    }

    #[tokio::test]
    async fn shell_caps_streamed_output_at_1_mib() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let text = shell_stream(
            &client,
            &addr,
            "run-cap",
            "yes xxxxxxxxxxxxxxxxxxxx | head -c 2000000",
        )
        .await;
        let events = sse_events(&text);
        let out_bytes: usize = events
            .iter()
            .filter(|(event, _)| event == "out")
            .map(|(_, data)| data.len())
            .sum();
        assert!(
            out_bytes <= MAX_SHELL_OUTPUT_BYTES + 64,
            "streamed {out_bytes} bytes, cap is {MAX_SHELL_OUTPUT_BYTES}"
        );
        assert!(
            events.iter().any(|(event, _)| event == "done"),
            "a capped stream must still end with a done event: {text:?}"
        );
    }

    #[tokio::test]
    async fn shell_stream_ends_when_backgrounded_grandchild_holds_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let response = tool_call(
            &client,
            &addr,
            "shell",
            "run-bg",
            json!({ "command": "sleep 100 & echo started" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // The shell exits immediately but `sleep 100` inherited the pipes and
        // keeps them open; the stream must still end within a few seconds.
        let text = tokio::time::timeout(Duration::from_secs(10), response.text())
            .await
            .expect("stream must end despite the backgrounded grandchild")
            .unwrap();
        let events = sse_events(&text);
        assert!(
            events
                .iter()
                .any(|(event, data)| event == "out" && data.contains("started")),
            "expected the backgrounded command's output: {text:?}"
        );
        assert!(
            events
                .iter()
                .any(|(event, data)| event == "done" && data == "0"),
            "expected done with exit 0: {text:?}"
        );
    }

    #[tokio::test]
    async fn client_abort_kills_the_shell_and_empties_running() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();
        let run_id = "run-abort";

        let response = tool_call(
            &client,
            &addr,
            "shell",
            run_id,
            json!({ "command": "sleep 30" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let pid = state
            .running
            .read()
            .await
            .get(run_id)
            .map(|shell| shell.pid)
            .expect("the run must be registered");

        // Dropping the response without reading the body closes the
        // connection; the guard must kill the shell and empty the running map.
        drop(response);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state.running.read().await.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("running map was not emptied after the client aborted");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The process group is dead. Poll `kill -0` because the wait task may
        // take a moment to reap the killed shell.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let alive = std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("shell process {pid} is still alive after the client aborted");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn git_tool_validates_verbs() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let response = tool_call(
            &client,
            &addr,
            "git",
            "run-1",
            json!({ "args": ["status"] }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert!(body["stdout"].is_string(), "git output has stdout: {body}");
        assert!(body["stderr"].is_string(), "git output has stderr: {body}");
        assert!(
            body["exit_code"].is_number(),
            "git output has exit_code: {body}"
        );
        assert_eq!(body["exit_code"], 0);

        let response = tool_call(&client, &addr, "git", "run-2", json!({ "args": ["push"] })).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = tool_call(
            &client,
            &addr,
            "git",
            "run-3",
            json!({ "args": ["checkout", "master"] }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn path_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        for path in ["../escape", "/etc/hosts"] {
            let response = tool_call(
                &client,
                &addr,
                "file/read",
                "run-1",
                json!({ "path": path }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "path {path}");
        }
    }

    #[tokio::test]
    async fn webfetch_returns_body() {
        let backend = stub_backend().await;
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let response = tool_call(
            &client,
            &addr,
            "webfetch",
            "run-1",
            json!({ "url": format!("http://{backend}/") }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["content"], "ok");

        let response = tool_call(
            &client,
            &addr,
            "webfetch",
            "run-2",
            json!({ "url": "file:///etc/hosts" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn permission_change_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _state) = test_app(dir.path(), Permission::ReadWrite).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("http://{addr}/permission"))
            .json(&json!({ "permission": "read_only" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(
            &client,
            &addr,
            "shell",
            "run-1",
            json!({ "command": "echo hi" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = client
            .post(format!("http://{addr}/permission"))
            .json(&json!({ "permission": "read_write" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = tool_call(
            &client,
            &addr,
            "shell",
            "run-2",
            json!({ "command": "echo hi" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let text = response.text().await.unwrap();
        assert!(text.contains("hi"), "shell output missing: {text:?}");
    }
}
