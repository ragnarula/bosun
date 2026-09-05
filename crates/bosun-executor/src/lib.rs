//! Executes tool calls against a session's working copy. One `ExecutorState`
//! lives in the node process per session; tool calls arrive as typed
//! dispatches instead of over HTTP, and the shell runs inside the node's
//! runtime.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::anyhow;
use bosun_common::session::Permission;
use futures_util::stream::BoxStream;
use futures_util::stream::unfold;
use serde_json::Value;
use serde_json::json;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub mod tools;

use tools::ToolError;

/// Cap on the total bytes streamed as `Out` events for one shell run. Output
/// past the cap is still drained from the pipes but not forwarded.
const MAX_SHELL_OUTPUT_BYTES: usize = 1 << 20;
/// After the shell exits, keep forwarding buffered output for this long even
/// if a backgrounded grandchild still holds the pipes open.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// A session's tools: the directory they run in, the session's live
/// permission, and every shell run in flight keyed by its run id. The node
/// owns one state per session, in process, instead of an executor process.
pub struct ExecutorState {
    pub session_dir: PathBuf,
    pub permission: RwLock<Permission>,
    pub running: RwLock<HashMap<String, RunningShell>>,
}

impl std::fmt::Debug for ExecutorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorState")
            .field("session_dir", &self.session_dir)
            .field("permission", &"<locked>")
            .finish()
    }
}

/// One running shell. The map entry is what a cancel and a session stop look
/// up; the kill signal answers through the shell's owner task, which owns the
/// child, publishes its pid, and reaps it. The entry exists before the child
/// is spawned, so a kill that lands while the shell is starting is not lost;
/// `pid` stays 0 until the owner task publishes the real one.
pub struct RunningShell {
    pub pid: u32,
    kill: Arc<Notify>,
}

impl ExecutorState {
    pub fn new(session_dir: PathBuf, permission: Permission) -> Self {
        Self {
            session_dir,
            permission: RwLock::new(permission),
            running: RwLock::new(HashMap::new()),
        }
    }

    /// Replaces the session's permission, gating the next dispatches.
    pub async fn set_permission(&self, permission: Permission) {
        *self.permission.write().await = permission;
    }

    /// Asks a running shell's owner task to kill it. Unknown run ids are a
    /// no-op, so cancelling a finished or never-started run succeeds.
    pub async fn cancel(&self, run_id: &str) {
        let kill = {
            let running = self.running.read().await;
            running.get(run_id).map(|shell| shell.kill.clone())
        };
        if let Some(kill) = kill {
            kill.notify_one();
        }
    }

    /// Kills every running shell. The node calls this when a session stops,
    /// so in-flight shells die with the session instead of outliving it.
    pub async fn kill_all_shells(&self) {
        let kills: Vec<Arc<Notify>> = {
            let running = self.running.read().await;
            running.values().map(|shell| shell.kill.clone()).collect()
        };
        for kill in kills {
            kill.notify_one();
        }
    }
}

/// A terminal answer to one tool call: a JSON result, or the shell's streamed
/// output. Refusals and failures are returned as `ExecutorError` instead.
pub enum CallOutcome {
    Result { content: Value },
    Shell(ShellStream),
}

impl std::fmt::Debug for CallOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallOutcome::Result { content } => {
                f.debug_struct("Result").field("content", content).finish()
            }
            CallOutcome::Shell(_) => f.debug_struct("Shell").finish_non_exhaustive(),
        }
    }
}

pub type ShellStream = BoxStream<'static, ShellEvent>;

/// One item of a streaming shell run.
#[derive(Debug)]
pub enum ShellEvent {
    Out(String),
    Done(i32),
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("unknown tool {tool}")]
    UnknownTool { tool: String },
    #[error("missing or invalid argument {key}")]
    BadArgument { key: &'static str },
    #[error(transparent)]
    Tool(#[from] ToolError),
}

/// Runs one tool call against the session's working copy under its current
/// permission. Blocking file, directory, and skill tools run on the blocking
/// pool; `git`, `webfetch`, and `shell` are already async over child
/// processes.
pub async fn run_call(
    state: &Arc<ExecutorState>,
    run_id: &str,
    tool: &str,
    args: &Value,
) -> Result<CallOutcome, ExecutorError> {
    let permission = *state.permission.read().await;
    if tool == "shell" {
        if permission != Permission::ReadWrite {
            return Err(ToolError::ReadOnly { tool: "shell" }.into());
        }
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return Err(ExecutorError::BadArgument { key: "command" });
        };
        if command.is_empty() {
            return Err(ExecutorError::BadArgument { key: "command" });
        }
        let stream = start_shell(state.clone(), run_id, command).await?;
        return Ok(CallOutcome::Shell(stream));
    }

    let result = match tool {
        "file/read" => {
            let Some(path) = args.get("path").and_then(Value::as_str) else {
                return Err(ExecutorError::BadArgument { key: "path" });
            };
            let path = path.to_string();
            run_blocking(&state.session_dir, move |dir| tools::read_file(dir, &path))
                .await
                .map(|content| json!({ "content": content }))
        }
        "file/write" => {
            if permission != Permission::ReadWrite {
                return Err(ToolError::ReadOnly { tool: "file/write" }.into());
            }
            let (Some(path), Some(content)) = (
                args.get("path").and_then(Value::as_str),
                args.get("content").and_then(Value::as_str),
            ) else {
                return Err(ExecutorError::BadArgument {
                    key: "path or content",
                });
            };
            let (path, content) = (path.to_string(), content.to_string());
            run_blocking(&state.session_dir, move |dir| {
                tools::write_file(dir, &path, &content)
            })
            .await
            .map(|()| json!({}))
        }
        "edit" => {
            if permission != Permission::ReadWrite {
                return Err(ToolError::ReadOnly { tool: "edit" }.into());
            }
            let (Some(path), Some(old), Some(new)) = (
                args.get("path").and_then(Value::as_str),
                args.get("old").and_then(Value::as_str),
                args.get("new").and_then(Value::as_str),
            ) else {
                return Err(ExecutorError::BadArgument {
                    key: "path, old or new",
                });
            };
            let (path, old, new) = (path.to_string(), old.to_string(), new.to_string());
            run_blocking(&state.session_dir, move |dir| {
                tools::edit(dir, &path, &old, &new)
            })
            .await
            .map(|()| json!({ "replaced": true }))
        }
        "grep" => {
            let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
                return Err(ExecutorError::BadArgument { key: "pattern" });
            };
            let pattern = pattern.to_string();
            let path = args.get("path").and_then(Value::as_str).map(String::from);
            run_blocking(&state.session_dir, move |dir| {
                tools::grep(dir, &pattern, path.as_deref())
            })
            .await
            .map(|matches| json!({ "matches": matches }))
        }
        "glob" => {
            let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
                return Err(ExecutorError::BadArgument { key: "pattern" });
            };
            let pattern = pattern.to_string();
            run_blocking(&state.session_dir, move |dir| tools::glob(dir, &pattern))
                .await
                .map(|paths| json!({ "paths": paths }))
        }
        "git" => {
            let Some(git_args) = args.get("args").and_then(Value::as_array) else {
                return Err(ExecutorError::BadArgument { key: "args" });
            };
            let mut parsed = Vec::with_capacity(git_args.len());
            for value in git_args {
                let Some(arg) = value.as_str() else {
                    return Err(ExecutorError::BadArgument {
                        key: "args must be strings",
                    });
                };
                parsed.push(arg.to_string());
            }
            tools::git(&state.session_dir, permission, &parsed)
                .await
                .map_err(ExecutorError::from)
                .map(|out| {
                    json!({ "stdout": out.stdout, "stderr": out.stderr, "exit_code": out.exit_code })
                })
        }
        "webfetch" => {
            let Some(url) = args.get("url").and_then(Value::as_str) else {
                return Err(ExecutorError::BadArgument { key: "url" });
            };
            tools::webfetch(url)
                .await
                .map_err(ExecutorError::from)
                .map(|content| json!({ "content": content }))
        }
        "skills" => run_blocking_infallible(&state.session_dir, tools::list_skills)
            .await
            .map(|skills| json!({ "skills": skills })),
        "skill/read" => {
            let Some(name) = args.get("name").and_then(Value::as_str) else {
                return Err(ExecutorError::BadArgument { key: "name" });
            };
            let name = name.to_string();
            run_blocking(&state.session_dir, move |dir| tools::read_skill(dir, &name))
                .await
                .map(|content| json!({ "content": content }))
        }
        "repo_standards" => {
            run_blocking_infallible(&state.session_dir, tools::repo_standards_present)
                .await
                .map(|present| json!({ "present": present }))
        }
        _ => {
            return Err(ExecutorError::UnknownTool {
                tool: tool.to_string(),
            });
        }
    };
    result.map(|content| CallOutcome::Result { content })
}

/// Runs one synchronous tool implementation on the blocking pool. Every
/// session's tools share the node's runtime, so file system work must not
/// occupy a runtime worker.
async fn run_blocking<T, F>(session_dir: &Path, f: F) -> Result<T, ExecutorError>
where
    T: Send + 'static,
    F: FnOnce(&Path) -> Result<T, ToolError> + Send + 'static,
{
    let dir = session_dir.to_path_buf();
    tokio::task::spawn_blocking(move || f(&dir))
        .await
        .map_err(|error| ToolError::Internal(anyhow!(error)))?
        .map_err(ExecutorError::from)
}

/// [`run_blocking`] for tools whose implementations cannot fail.
async fn run_blocking_infallible<T, F>(session_dir: &Path, f: F) -> Result<T, ExecutorError>
where
    T: Send + 'static,
    F: FnOnce(&Path) -> T + Send + 'static,
{
    let dir = session_dir.to_path_buf();
    tokio::task::spawn_blocking(move || f(&dir))
        .await
        .map_err(|error| ToolError::Internal(anyhow!(error)))
        .map_err(ExecutorError::from)
}

/// One streamed shell output line, or a signal that a pipe was drained.
enum ShellMsg {
    Out(String),
    ReaderDone,
}

/// Owns the shell's kill signal for the lifetime of the streamed run. On drop
/// it asks the owner task to kill the shell and removes the run from the
/// running map, so an aborted consumer leaks nothing. The guard never signals
/// a pid itself: the owner task owns the child and reaps it, so no kill can
/// target a reaped pid.
struct ShellGuard {
    kill: Arc<Notify>,
    state: Arc<ExecutorState>,
    run_id: String,
}

impl Drop for ShellGuard {
    fn drop(&mut self) {
        self.kill.notify_one();
        let state = self.state.clone();
        let run_id = self.run_id.clone();
        tokio::spawn(async move {
            state.running.write().await.remove(&run_id);
        });
    }
}

/// Kills a shell's process group. Called only by the owner task while the
/// child is still alive, so the group it names cannot have been reused. On
/// Unix the shell runs in its own session (setsid), so the group is confined
/// to the shell and its children; on Windows taskkill with /T covers the
/// tree. Pids at or below 1 are refused: `kill -KILL -1` would reach every
/// process the user can signal and `-0` would reach the caller's own group.
async fn kill_group(pgid: u32) {
    if pgid <= 1 {
        return;
    }
    #[cfg(unix)]
    {
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", &format!("-{pgid}")])
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pgid.to_string(), "/T", "/F"])
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pgid;
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

/// Spawns one shell and returns its streamed events. The run is registered
/// before the child is spawned, so a cancel or session stop that lands while
/// the shell is still starting already finds the run, and the shell dies with
/// it instead of surviving orphaned.
async fn start_shell(
    state: Arc<ExecutorState>,
    run_id: &str,
    command: &str,
) -> Result<ShellStream, ExecutorError> {
    let mut shell = tokio::process::Command::new("sh");
    shell
        .arg("-c")
        .arg(command)
        .current_dir(&state.session_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // The shell becomes a session leader (own session, own process
        // group), so a group kill is confined to the shell and its children
        // and terminal signals from the node's session never reach it.
        unsafe {
            shell.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    // Register the run before the child exists: a cancel or session stop that
    // lands in the window between spawn and registration must not find nothing
    // and leave an orphaned shell behind. The placeholder pid is 0, which the
    // owner task replaces once the child exists; kill_group refuses pids at or
    // below 1, so nothing can ever signal the placeholder.
    let kill_signal = Arc::new(Notify::new());
    state.running.write().await.insert(
        run_id.to_string(),
        RunningShell {
            pid: 0,
            kill: kill_signal.clone(),
        },
    );

    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(error) => {
            // The shell never started; drop the placeholder so the running
            // map keeps naming only live runs.
            state.running.write().await.remove(run_id);
            return Err(ToolError::Internal(anyhow!(error)).into());
        }
    };
    let Some(pid) = child.id() else {
        state.running.write().await.remove(run_id);
        return Err(ToolError::Internal(anyhow!("spawned shell reported no pid")).into());
    };
    let stdout = child.stdout.take().expect("shell stdout is piped");
    let stderr = child.stderr.take().expect("shell stderr is piped");

    // One task owns the child: it publishes the pid, reaps the child, and
    // answers kill requests. Killing happens here, while the child is alive
    // and its pid is still allocated, so the process group it signals is this
    // shell's own and cannot have been reused by another process.
    let (exit_tx, exit_rx) = oneshot::channel();
    tokio::spawn({
        let kill_signal = kill_signal.clone();
        let state = state.clone();
        let run_id = run_id.to_string();
        async move {
            // A pid visible in the running map always belongs to a child this
            // task is about to reap. The entry can already be gone when a kill
            // landed before the spawn and the stream ended before this task
            // ran; there is nothing to publish then.
            if let Some(entry) = state.running.write().await.get_mut(&run_id) {
                entry.pid = pid;
            }
            let code = tokio::select! {
                _ = kill_signal.notified() => {
                    if child.try_wait().ok().flatten().is_none() {
                        kill_group(pid).await;
                    }
                    child.kill().await.ok();
                    child.wait().await.ok().and_then(|status| status.code()).unwrap_or(-1)
                }
                status = child.wait() => status.ok().and_then(|status| status.code()).unwrap_or(-1),
            };
            let _ = exit_tx.send(code);
        }
    });

    let (tx, rx) = mpsc::channel::<ShellMsg>(64);
    let total = Arc::new(AtomicUsize::new(0));
    tokio::spawn(pump(stdout, tx.clone(), total.clone()));
    tokio::spawn(pump(stderr, tx, total));

    let guard = ShellGuard {
        kill: kill_signal,
        state: state.clone(),
        run_id: run_id.to_string(),
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
    Ok(Box::pin(stream))
}

/// Mutable state threaded through the streamed events of one shell run.
/// `_guard` is held only for its Drop side effect: killing the process group
/// and removing the run when the stream ends.
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

async fn stream_step(mut st: StreamState) -> Option<(ShellEvent, StreamState)> {
    loop {
        if st.done_sent {
            return None;
        }
        // Both pipes drained and the shell exited: done.
        if st.exit_code.is_some() && st.readers_left == 0 {
            st.done_sent = true;
            return Some((ShellEvent::Done(st.exit_code.unwrap_or(-1)), st));
        }
        // The shell exited but a backgrounded grandchild may still hold the
        // pipes open; after the grace period end the stream regardless.
        if let Some(deadline) = st.drain_deadline
            && tokio::time::Instant::now() >= deadline
        {
            st.done_sent = true;
            return Some((ShellEvent::Done(st.exit_code.unwrap_or(-1)), st));
        }
        let drain = st.drain_deadline;
        tokio::select! {
            biased;
            msg = st.rx.recv(), if !st.rx_closed => {
                match msg {
                    Some(ShellMsg::Out(text)) => {
                        return Some((ShellEvent::Out(text), st));
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bosun_test_support::init_repo;
    use bosun_test_support::stub_backend;
    use futures_util::StreamExt;

    use super::*;

    fn state(dir: &Path, permission: Permission) -> Arc<ExecutorState> {
        Arc::new(ExecutorState::new(dir.to_path_buf(), permission))
    }

    /// Runs one tool call and returns the outcome.
    async fn call(
        state: &Arc<ExecutorState>,
        run_id: &str,
        tool: &str,
        args: Value,
    ) -> Result<CallOutcome, ExecutorError> {
        run_call(state, run_id, tool, &args).await
    }

    /// Asserts a call failed and returns its error.
    async fn call_error(
        state: &Arc<ExecutorState>,
        run_id: &str,
        tool: &str,
        args: Value,
    ) -> ExecutorError {
        match call(state, run_id, tool, args).await {
            Err(error) => error,
            Ok(_) => panic!("the {tool} call must fail"),
        }
    }

    /// Runs a shell command and collects its whole event stream.
    async fn shell_events(
        state: &Arc<ExecutorState>,
        run_id: &str,
        command: &str,
    ) -> Vec<ShellEvent> {
        let outcome = call(state, run_id, "shell", json!({ "command": command }))
            .await
            .expect("shell call should start");
        let CallOutcome::Shell(stream) = outcome else {
            panic!("shell must stream");
        };
        stream.collect().await
    }

    fn out_contains(events: &[ShellEvent], needle: &str) -> bool {
        events
            .iter()
            .any(|event| matches!(event, ShellEvent::Out(text) if text.contains(needle)))
    }

    fn done_code(events: &[ShellEvent]) -> i32 {
        events
            .iter()
            .find_map(|event| match event {
                ShellEvent::Done(code) => Some(*code),
                ShellEvent::Out(_) => None,
            })
            .expect("the stream must end with a done event")
    }

    /// Waits until the shell's owner task has published the child's pid. The
    /// running map registers the run with pid 0 before the child exists, so a
    /// real pid is the signal that the shell is up.
    async fn wait_for_pid(state: &Arc<ExecutorState>, run_id: &str) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let pid = state
                .running
                .read()
                .await
                .get(run_id)
                .map(|shell| shell.pid)
                .unwrap_or(0);
            if pid > 0 {
                return pid;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the shell {run_id} never published its pid");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let outcome = call(
            &state,
            "run-1",
            "file/write",
            json!({ "path": "hello.txt", "content": "hi there" }),
        )
        .await
        .expect("write should succeed");
        assert!(matches!(outcome, CallOutcome::Result { content } if content == json!({})));

        let outcome = call(&state, "run-2", "file/read", json!({ "path": "hello.txt" }))
            .await
            .expect("read should succeed");
        let CallOutcome::Result { content } = outcome else {
            panic!("file/read must not stream");
        };
        assert_eq!(content, json!({ "content": "hi there" }));
    }

    #[tokio::test]
    async fn edit_replaces_and_errors_on_missing_old() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        call(
            &state,
            "run-1",
            "file/write",
            json!({ "path": "f.txt", "content": "hello world" }),
        )
        .await
        .unwrap();

        let outcome = call(
            &state,
            "run-2",
            "edit",
            json!({ "path": "f.txt", "old": "world", "new": "there" }),
        )
        .await
        .expect("edit should succeed");
        assert!(matches!(outcome, CallOutcome::Result { content } if content["replaced"] == true));

        let outcome = call(&state, "run-3", "file/read", json!({ "path": "f.txt" }))
            .await
            .unwrap();
        let CallOutcome::Result { content } = outcome else {
            panic!("file/read must not stream");
        };
        assert_eq!(content["content"], "hello there");

        let error = call_error(
            &state,
            "run-4",
            "edit",
            json!({ "path": "f.txt", "old": "absent", "new": "x" }),
        )
        .await;
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::OldTextNotFound)
        ));
    }

    #[tokio::test]
    async fn read_only_refuses_mutating_tools() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("needle.txt"), "needle").unwrap();
        let state = state(dir.path(), Permission::ReadOnly);

        for (tool, args) in [
            ("shell", json!({ "command": "echo hi" })),
            ("file/write", json!({ "path": "f.txt", "content": "x" })),
            ("edit", json!({ "path": "f.txt", "old": "a", "new": "b" })),
        ] {
            let error = call_error(&state, "run-1", tool, args).await;
            assert!(
                matches!(error, ExecutorError::Tool(ToolError::ReadOnly { .. })),
                "tool {tool}"
            );
        }

        assert!(
            call(
                &state,
                "run-2",
                "file/read",
                json!({ "path": "needle.txt" })
            )
            .await
            .is_ok()
        );
        assert!(
            call(&state, "run-3", "git", json!({ "args": ["status"] }))
                .await
                .is_ok()
        );
        assert!(
            call(&state, "run-4", "grep", json!({ "pattern": "needle" }))
                .await
                .is_ok()
        );
        assert!(
            call(&state, "run-5", "glob", json!({ "pattern": "*" }))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn shell_streams_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let events = shell_events(&state, "run-echo", "echo hello").await;
        assert!(
            out_contains(&events, "hello"),
            "echo hello should stream its output"
        );
        assert_eq!(done_code(&events), 0, "echo hello exits 0");

        let events = shell_events(&state, "run-exit", "exit 3").await;
        assert_eq!(done_code(&events), 3, "exit 3 ends with code 3");
    }

    #[tokio::test]
    async fn cancel_kills_the_shells_children_too() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let state = state(dir.path(), Permission::ReadWrite);
            let run_id = "run-tree";
            let outcome = call(
                &state,
                run_id,
                "shell",
                json!({ "command": "sleep 27 & wait" }),
            )
            .await
            .expect("shell should start");
            let CallOutcome::Shell(stream) = outcome else {
                panic!("shell must stream");
            };

            let grandchild_running = || async {
                std::process::Command::new("pgrep")
                    .args(["-f", "sleep 27"])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false)
            };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !grandchild_running().await {
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell's child never started");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            let collector = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
            state.cancel(run_id).await;

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while grandchild_running().await {
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell's child survived the cancel");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let events = tokio::time::timeout(Duration::from_secs(5), collector)
                .await
                .expect("the stream must end after the cancel")
                .unwrap();
            assert_eq!(done_code(&events), -1, "a cancelled shell ends with -1");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if state.running.read().await.is_empty() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the run was not removed from the running map");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    #[tokio::test]
    async fn shell_cancel_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);
        let run_id = "run-sleep";

        let outcome = call(&state, run_id, "shell", json!({ "command": "sleep 30" }))
            .await
            .expect("shell should start");
        let CallOutcome::Shell(stream) = outcome else {
            panic!("shell must stream");
        };

        // The run is registered before the stream is returned, so the cancel
        // can go out before anything has been consumed.
        let collector = tokio::spawn(stream.collect::<Vec<_>>());
        state.cancel(run_id).await;

        let events = tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("the stream must end after the cancel")
            .unwrap();
        assert_eq!(
            done_code(&events),
            -1,
            "a cancelled shell should end with exit code -1"
        );
    }

    /// A cancel that lands while the shell is still starting — after the run
    /// is registered but before `run_call` has returned the stream — kills it.
    /// The run is registered before the child is spawned, so no cancel can
    /// land in a window that finds nothing and leaves the shell orphaned.
    #[tokio::test]
    async fn a_cancel_while_the_shell_is_starting_still_kills_it() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let state = state(dir.path(), Permission::ReadWrite);
            let run_id = "run-starting";

            let outcome = tokio::spawn({
                let state = state.clone();
                async move { call(&state, run_id, "shell", json!({ "command": "sleep 71" })).await }
            });

            // The run is registered before the child is spawned, so the cancel
            // below can go out while the shell is still starting.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if state.running.read().await.contains_key(run_id) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the run was never registered");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            state.cancel(run_id).await;

            // The caller still receives the run, ended like a cancelled one.
            let outcome = tokio::time::timeout(Duration::from_secs(5), outcome)
                .await
                .expect("the shell call must complete within 5 seconds")
                .expect("the shell call task must not panic")
                .expect("the shell call should start");
            let CallOutcome::Shell(stream) = outcome else {
                panic!("shell must stream");
            };
            let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
                .await
                .expect("the stream must end after the cancel");
            assert_eq!(done_code(&events), -1, "a cancelled shell ends with -1");

            // Nothing survives the cancel: the run is deregistered and the
            // shell's process group is gone.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if state.running.read().await.is_empty() && !process_exists("sleep 71") {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell survived the cancel that arrived while it was starting");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    #[tokio::test]
    async fn shell_caps_streamed_output_at_1_mib() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let events = shell_events(
            &state,
            "run-cap",
            "yes xxxxxxxxxxxxxxxxxxxx | head -c 2000000",
        )
        .await;
        let out_bytes: usize = events
            .iter()
            .filter_map(|event| match event {
                ShellEvent::Out(text) => Some(text.len()),
                ShellEvent::Done(_) => None,
            })
            .sum();
        assert!(
            out_bytes <= MAX_SHELL_OUTPUT_BYTES + 64,
            "streamed {out_bytes} bytes, cap is {MAX_SHELL_OUTPUT_BYTES}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ShellEvent::Done(_))),
            "a capped stream must still end with a done event"
        );
    }

    #[tokio::test]
    async fn shell_stream_ends_when_backgrounded_grandchild_holds_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let events = tokio::time::timeout(
            Duration::from_secs(10),
            shell_events(&state, "run-bg", "sleep 100 & echo started"),
        )
        .await
        .expect("the stream must end despite the backgrounded grandchild");
        assert!(
            out_contains(&events, "started"),
            "expected the backgrounded command's output"
        );
        assert_eq!(done_code(&events), 0, "expected done with exit 0");
    }

    #[tokio::test]
    async fn kill_group_never_broadcasts_or_targets_system_pids() {
        #[cfg(unix)]
        {
            let mut child = tokio::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            // If the guard were absent, `kill -KILL -1` would signal every
            // process this test can reach, killing `sleep` too.
            kill_group(0).await;
            kill_group(1).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            assert!(
                alive,
                "pid 0 and 1 must never be signalled as a process group"
            );
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
    }

    #[tokio::test]
    async fn dropping_the_stream_kills_the_shell_and_empties_running() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);
        let run_id = "run-abort";

        let outcome = call(&state, run_id, "shell", json!({ "command": "sleep 45" }))
            .await
            .expect("shell should start");
        let CallOutcome::Shell(stream) = outcome else {
            panic!("shell must stream");
        };
        let pid = wait_for_pid(&state, run_id).await;

        // Dropping the stream without consuming it closes the run; the guard
        // must kill the shell and empty the running map.
        drop(stream);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state.running.read().await.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("running map was not emptied after the stream was dropped");
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
                panic!("shell process {pid} is still alive after the stream was dropped");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn kill_all_shells_kills_every_running_run() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let state = state(dir.path(), Permission::ReadWrite);

            let mut collectors = Vec::new();
            for (run_id, command) in [("run-a", "sleep 61"), ("run-b", "sleep 62")] {
                let outcome = call(&state, run_id, "shell", json!({ "command": command }))
                    .await
                    .expect("shell should start");
                let CallOutcome::Shell(stream) = outcome else {
                    panic!("shell must stream");
                };
                let pid = wait_for_pid(&state, run_id).await;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while !std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false)
                {
                    if tokio::time::Instant::now() >= deadline {
                        panic!("shell {run_id} never started");
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                collectors.push(tokio::spawn(stream.collect::<Vec<_>>()));
            }

            state.kill_all_shells().await;

            // Every in-flight run ends with a killed-run code and the running
            // map empties.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let empty = state.running.read().await.is_empty();
                if empty && !process_exists("sleep 61") && !process_exists("sleep 62") {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("kill_all did not reap every shell");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            for collector in collectors {
                let events = tokio::time::timeout(Duration::from_secs(5), collector)
                    .await
                    .expect("the stream must end after kill_all")
                    .unwrap();
                assert_eq!(done_code(&events), -1);
            }
        }
    }

    fn process_exists(needle: &str) -> bool {
        std::process::Command::new("pgrep")
            .args(["-f", needle])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn git_tool_validates_verbs() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let state = state(dir.path(), Permission::ReadWrite);

        let outcome = call(&state, "run-1", "git", json!({ "args": ["status"] }))
            .await
            .expect("git status should run");
        let CallOutcome::Result { content } = outcome else {
            panic!("git must not stream");
        };
        assert!(content["stdout"].is_string(), "git output has stdout");
        assert!(content["stderr"].is_string(), "git output has stderr");
        assert!(content["exit_code"].is_number(), "git output has exit_code");
        assert_eq!(content["exit_code"], 0);

        let error = call_error(&state, "run-2", "git", json!({ "args": ["push"] })).await;
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::GitPushForbidden)
        ));

        let error = call_error(
            &state,
            "run-3",
            "git",
            json!({ "args": ["checkout", "master"] }),
        )
        .await;
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::GitVerbNotAllowed { .. })
        ));
    }

    #[tokio::test]
    async fn path_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        for path in ["../escape", "/etc/hosts"] {
            let error = call_error(&state, "run-1", "file/read", json!({ "path": path })).await;
            assert!(
                matches!(
                    error,
                    ExecutorError::Tool(ToolError::PathOutsideRoot { .. })
                ),
                "path {path}"
            );
        }
    }

    #[tokio::test]
    async fn webfetch_returns_body() {
        let backend = stub_backend().await;
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let outcome = call(
            &state,
            "run-1",
            "webfetch",
            json!({ "url": format!("http://{backend}/") }),
        )
        .await
        .expect("webfetch should run");
        let CallOutcome::Result { content } = outcome else {
            panic!("webfetch must not stream");
        };
        assert_eq!(content["content"], "ok");

        let error = call_error(
            &state,
            "run-2",
            "webfetch",
            json!({ "url": "file:///etc/hosts" }),
        )
        .await;
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::UnsupportedUrl { .. })
        ));
    }

    #[tokio::test]
    async fn skills_list_and_read_work() {
        let dir = tempfile::tempdir().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        std::fs::create_dir_all(skills_root.join("alpha")).unwrap();
        std::fs::write(
            skills_root.join("alpha/SKILL.md"),
            "---\nname: alpha\ndescription: The alpha skill\n---\n\nBody text",
        )
        .unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let outcome = call(&state, "run-1", "skills", json!({}))
            .await
            .expect("skills should run");
        let CallOutcome::Result { content } = outcome else {
            panic!("skills must not stream");
        };
        assert_eq!(content["skills"][0]["name"], "alpha");
        assert_eq!(content["skills"][0]["description"], "The alpha skill");

        let outcome = call(&state, "run-2", "skill/read", json!({ "name": "alpha" }))
            .await
            .expect("skill/read should run");
        let CallOutcome::Result { content } = outcome else {
            panic!("skill/read must not stream");
        };
        assert!(content["content"].as_str().unwrap().contains("Body text"));

        let error = call_error(&state, "run-3", "skill/read", json!({ "name": "absent" })).await;
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::NotFound { .. })
        ));

        let error = call_error(&state, "run-4", "skill/read", json!({})).await;
        assert!(matches!(error, ExecutorError::BadArgument { key: "name" }));
    }

    #[tokio::test]
    async fn repo_standards_lists_presence_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let state = state(root, Permission::ReadWrite);

        let outcome = call(&state, "run-1", "repo_standards", json!({}))
            .await
            .expect("repo_standards should run");
        let CallOutcome::Result { content } = outcome else {
            panic!("repo_standards must not stream");
        };
        assert_eq!(content, json!({ "present": [] }), "neither file lists none");

        // One file at the root lists its name; the contents never travel.
        std::fs::write(root.join("CLAUDE.md"), "claude body").unwrap();
        let outcome = call(&state, "run-2", "repo_standards", json!({}))
            .await
            .unwrap();
        let CallOutcome::Result { content } = outcome else {
            panic!("repo_standards must not stream");
        };
        assert_eq!(content, json!({ "present": ["CLAUDE.md"] }));
        assert!(
            !content.to_string().contains("claude body"),
            "the response must carry presence, never contents"
        );

        // Both files list in canonical order.
        std::fs::write(root.join("AGENTS.md"), "agents body").unwrap();
        let outcome = call(&state, "run-3", "repo_standards", json!({}))
            .await
            .unwrap();
        let CallOutcome::Result { content } = outcome else {
            panic!("repo_standards must not stream");
        };
        assert_eq!(content, json!({ "present": ["AGENTS.md", "CLAUDE.md"] }));
    }

    #[tokio::test]
    async fn permission_change_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        state.set_permission(Permission::ReadOnly).await;
        let error = call_error(&state, "run-1", "shell", json!({ "command": "echo hi" })).await;
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::ReadOnly { tool: "shell" })
        ));

        state.set_permission(Permission::ReadWrite).await;
        let events = shell_events(&state, "run-2", "echo hi").await;
        assert!(out_contains(&events, "hi"), "shell output missing");
        assert_eq!(done_code(&events), 0);
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_args_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let error = call_error(&state, "run-1", "nope", json!({})).await;
        assert!(matches!(error, ExecutorError::UnknownTool { tool } if tool == "nope"));

        let error = call_error(&state, "run-2", "file/read", json!({})).await;
        assert!(matches!(error, ExecutorError::BadArgument { key: "path" }));
    }

    #[tokio::test]
    async fn the_running_map_empties_after_a_normal_run_ends() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);

        let events = shell_events(&state, "run-1", "true").await;
        assert_eq!(done_code(&events), 0);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state.running.read().await.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the run stayed registered after it ended");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_cancel_for_an_unknown_run_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), Permission::ReadWrite);
        state.cancel("ghost-run").await;
        state.kill_all_shells().await;
        assert!(state.running.read().await.is_empty());
    }
}
