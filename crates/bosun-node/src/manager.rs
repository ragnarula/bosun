use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::session::Permission;
use bosun_common::types::DirEntry;
use bosun_common::types::DirListing;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeDevRequest;
use bosun_common::types::NodeStartRequest;
use bosun_common::types::SessionInfo;
use thiserror::Error;
use tokio::process::Child;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::state::PersistedSession;
use crate::state::state_path;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub repo_url: Option<String>,
    pub git_ref: Option<String>,
    pub dir: PathBuf,
    pub reapable: bool,
    pub status: String,
    pub executor_port: Option<u16>,
    pub permission: Permission,
}

impl SessionRecord {
    pub fn to_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            repo_url: self.repo_url.clone(),
            git_ref: self.git_ref.clone(),
            dir: Some(self.dir.clone()),
            status: self.status.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("failed to clone {repo_url}: {stderr}")]
    CloneFailed { repo_url: String, stderr: String },

    #[error("executor on port {port} did not become healthy")]
    HealthTimeout { port: u16 },

    #[error("no browse roots configured on this node")]
    NoBrowseRoots,

    #[error("directory {dir} does not exist")]
    DirNotFound { dir: String },

    #[error("path {path} is not a directory")]
    NotADirectory { path: String },

    #[error("directory {dir} is outside the configured browse roots")]
    OutsideRoot { dir: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

pub struct NodeManager {
    work_dir: PathBuf,
    cp_url: String,
    browse_roots: Vec<PathBuf>,
    tls_config: Option<std::sync::Arc<rustls::ClientConfig>>,
    /// How long an executor start waits to become healthy before the start
    /// fails. Tests shorten it; the node always uses `HEALTH_TIMEOUT`.
    health_timeout: Duration,
    sessions: RwLock<HashMap<String, SessionRecord>>,
    processes: RwLock<HashMap<String, Child>>,
}

impl NodeManager {
    pub fn new(
        work_dir: PathBuf,
        browse_roots: Vec<PathBuf>,
        cp_url: String,
        tls_config: Option<std::sync::Arc<rustls::ClientConfig>>,
    ) -> Self {
        let browse_roots: Vec<PathBuf> = browse_roots
            .into_iter()
            .filter_map(|root| match root.canonicalize() {
                Ok(path) => Some(path),
                Err(e) => {
                    warn!(
                        root = %root.display(),
                        error = %e,
                        "browse root does not exist; ignoring it"
                    );
                    None
                }
            })
            .collect();
        Self {
            work_dir,
            cp_url,
            browse_roots,
            tls_config,
            health_timeout: HEALTH_TIMEOUT,
            sessions: RwLock::new(HashMap::new()),
            processes: RwLock::new(HashMap::new()),
        }
    }

    pub async fn run_clone(&self, req: &NodeCloneRequest) -> Result<SessionRecord, NodeError> {
        tokio::fs::create_dir_all(&self.work_dir)
            .await
            .with_context(|| format!("failed to create work dir {}", self.work_dir.display()))?;

        let dir = self.work_dir.join(&req.session_id);

        let mut command = tokio::process::Command::new("git");
        command.arg("clone");
        if let Some(git_ref) = &req.git_ref {
            command.arg("--branch").arg(git_ref).arg("--single-branch");
        }
        let output = command
            .arg(&req.repo_url)
            .arg(&dir)
            .output()
            .await
            .with_context(|| format!("failed to run git clone for session {}", req.session_id))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            cleanup(&dir).await;
            return Err(NodeError::CloneFailed {
                repo_url: req.repo_url.clone(),
                stderr,
            });
        }

        let record = self
            .start_in_dir(
                &req.session_id,
                &dir,
                true,
                Some(req.repo_url.clone()),
                req.git_ref.clone(),
                req.permission,
            )
            .await?;
        info!(session_id = %req.session_id, "clone session started");
        Ok(record)
    }

    pub async fn dev(&self, req: &NodeDevRequest) -> Result<SessionRecord, NodeError> {
        let dir = self.resolve_within_roots(&req.dir)?;
        let record = self
            .start_in_dir(&req.session_id, &dir, false, None, None, req.permission)
            .await?;
        info!(session_id = %req.session_id, dir = %record.dir.display(), "dev session started");
        Ok(record)
    }

    /// Starts a session executor in a directory that already exists on the
    /// node. Only the control plane's child-session spawner calls this: the
    /// directory is the parent session's working copy. A spawned child's
    /// executor holds the same shell and file access as any session's, so the
    /// directory is confined to the configured browse roots exactly like a
    /// `dev` session's directory; clone-session parents live under
    /// `work_dir/<session_id>`, so a root must cover the node's `work_dir`
    /// for their children to start.
    pub async fn start(&self, req: &NodeStartRequest) -> Result<SessionRecord, NodeError> {
        let dir = self.resolve_within_roots(&req.dir)?;
        let record = self
            .start_in_dir(&req.session_id, &dir, false, None, None, req.permission)
            .await?;
        info!(session_id = %req.session_id, dir = %record.dir.display(), "session started in existing dir");
        Ok(record)
    }

    async fn start_in_dir(
        &self,
        session_id: &str,
        dir: &Path,
        reapable: bool,
        repo_url: Option<String>,
        git_ref: Option<String>,
        permission: Permission,
    ) -> Result<SessionRecord, NodeError> {
        let port = pick_free_port().await?;

        let child = match self.start_executor(session_id, dir, port, permission).await {
            Ok(child) => child,
            Err(e) => {
                if reapable {
                    cleanup(dir).await;
                }
                return Err(e);
            }
        };

        let record = SessionRecord {
            id: session_id.to_string(),
            repo_url,
            git_ref,
            dir: dir.to_path_buf(),
            reapable,
            status: "running".into(),
            executor_port: Some(port),
            permission,
        };
        self.sessions
            .write()
            .unwrap()
            .insert(record.id.clone(), record.clone());
        self.processes
            .write()
            .unwrap()
            .insert(record.id.clone(), child);

        if let Err(e) = self.persist().await {
            warn!(
                session_id = %record.id,
                error = %e.display_chain(),
                "failed to persist session state"
            );
        }
        Ok(record)
    }

    pub fn list_dir(&self, requested: Option<&Path>) -> Result<DirListing, NodeError> {
        let Some(requested) = requested else {
            return self.list_roots();
        };
        let canonical = self.resolve_within_roots(requested)?;

        let read = std::fs::read_dir(&canonical)
            .with_context(|| format!("failed to read directory {}", canonical.display()))?;

        let mut entries: Vec<DirEntry> = read
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !path.is_dir() {
                    return None;
                }
                let is_repo = path.join(".git").exists();
                Some(DirEntry {
                    name,
                    path: path.clone(),
                    is_repo,
                })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        let parent = canonical
            .parent()
            .filter(|parent| self.within_roots(parent))
            .map(PathBuf::from);

        Ok(DirListing {
            path: Some(canonical),
            parent,
            entries,
        })
    }

    fn list_roots(&self) -> Result<DirListing, NodeError> {
        if self.browse_roots.is_empty() {
            return Err(NodeError::NoBrowseRoots);
        }
        let entries = self
            .browse_roots
            .iter()
            .map(|root| DirEntry {
                name: root.display().to_string(),
                path: root.clone(),
                is_repo: root.join(".git").exists(),
            })
            .collect();
        Ok(DirListing {
            path: None,
            parent: None,
            entries,
        })
    }

    fn resolve_within_roots(&self, requested: &Path) -> Result<PathBuf, NodeError> {
        if self.browse_roots.is_empty() {
            return Err(NodeError::NoBrowseRoots);
        }
        if !requested.exists() {
            return Err(NodeError::DirNotFound {
                dir: requested.display().to_string(),
            });
        }
        let canonical = requested
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", requested.display()))?;
        if !canonical.is_dir() {
            return Err(NodeError::NotADirectory {
                path: requested.display().to_string(),
            });
        }
        if !self.within_roots(&canonical) {
            return Err(NodeError::OutsideRoot {
                dir: requested.display().to_string(),
            });
        }
        Ok(canonical)
    }

    fn within_roots(&self, path: &Path) -> bool {
        self.browse_roots.iter().any(|root| path.starts_with(root))
    }

    pub async fn stop(&self, session_id: &str) -> Result<(), NodeError> {
        let record = match self.sessions.read().unwrap().get(session_id) {
            Some(record) => record.clone(),
            None => {
                info!(session_id = %session_id, "stop requested for unknown session");
                return Ok(());
            }
        };

        let child = self.processes.write().unwrap().remove(session_id);
        if let Some(mut child) = child {
            child
                .kill()
                .await
                .with_context(|| format!("failed to kill executor for session {session_id}"))?;
        }

        self.sessions.write().unwrap().remove(session_id);
        if record.reapable {
            cleanup(&record.dir).await;
        }

        if let Err(e) = self.persist().await {
            warn!(
                session_id = %session_id,
                error = %e.display_chain(),
                "failed to persist session state after stop"
            );
        }

        info!(session_id = %session_id, "session stopped");
        Ok(())
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self
            .sessions
            .read()
            .unwrap()
            .values()
            .map(|record| record.to_info())
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }

    async fn start_executor(
        &self,
        id: &str,
        dir: &Path,
        port: u16,
        permission: Permission,
    ) -> Result<Child, NodeError> {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bosun"));
        // The session dir must be absolute: the executor resolves its tools
        // against it, so a relative path would be re-resolved against the
        // executor's own working directory and point at the wrong tree.
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut command = tokio::process::Command::new(exe);
        let permission_arg = match permission {
            Permission::ReadOnly => "read_only",
            Permission::ReadWrite => "read_write",
        };
        command
            .arg("executor")
            .arg("--session-dir")
            .arg(&dir)
            .arg("--port")
            .arg(port.to_string())
            .arg("--permission")
            .arg(permission_arg)
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .with_context(|| format!("failed to start executor for session {id}"))?;

        let client = reqwest::Client::new();
        wait_for_health(&client, port, self.health_timeout).await?;
        Ok(child)
    }

    /// Starts the node's one outbound tunnel to the control plane. The task
    /// reconnects on its own until the node exits, so sessions never nudge
    /// it, and a control-plane restart needs no per-session nudge either.
    pub fn start_node_tunnel(self: &Arc<Self>, node_name: &str) {
        let cp_url = self.cp_url.clone();
        let node_name = node_name.to_string();
        let tls_config = self.tls_config.clone();
        let manager = self.clone();
        tokio::spawn(crate::tunnel::run_node_tunnel(
            cp_url, node_name, manager, tls_config,
        ));
    }

    /// The executor port of one running session. The node's tunnel relay
    /// dials it when a logical connection addressed to the session arrives.
    pub fn executor_port(&self, session_id: &str) -> Option<u16> {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .and_then(|record| record.executor_port)
    }

    /// Registers a running session without spawning an executor, so tunnel
    /// tests can relay to stub executors on real ports.
    #[cfg(test)]
    pub(crate) fn add_session_for_test(&self, id: &str, executor_port: u16) {
        let record = SessionRecord {
            id: id.to_string(),
            repo_url: None,
            git_ref: None,
            dir: PathBuf::from("/work"),
            reapable: false,
            status: "running".into(),
            executor_port: Some(executor_port),
            permission: Permission::ReadWrite,
        };
        self.sessions
            .write()
            .unwrap()
            .insert(id.to_string(), record);
    }

    fn persisted_sessions(&self) -> Vec<PersistedSession> {
        let sessions = self.sessions.read().unwrap();
        let processes = self.processes.read().unwrap();
        let mut persisted: Vec<PersistedSession> = sessions
            .values()
            .filter_map(|record| {
                let port = record.executor_port?;
                let pid = processes
                    .get(&record.id)
                    .and_then(|child| child.id())
                    .unwrap_or(0);
                Some(PersistedSession {
                    id: record.id.clone(),
                    repo_url: record.repo_url.clone(),
                    git_ref: record.git_ref.clone(),
                    dir: if record.reapable {
                        None
                    } else {
                        Some(record.dir.clone())
                    },
                    reapable: record.reapable,
                    executor_port: port,
                    permission: record.permission,
                    pid,
                })
            })
            .collect();
        persisted.sort_by(|a, b| a.id.cmp(&b.id));
        persisted
    }

    async fn write_state(&self, persisted: &[PersistedSession]) -> Result<(), anyhow::Error> {
        let json = serde_json::to_string_pretty(persisted).context("failed to serialize state")?;
        let path = state_path(&self.work_dir);
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &json)
            .await
            .with_context(|| format!("failed to write state to {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("failed to move state into {}", path.display()))?;
        Ok(())
    }

    async fn persist(&self) -> Result<(), anyhow::Error> {
        let persisted = self.persisted_sessions();
        self.write_state(&persisted).await
    }

    pub async fn restore(&self) {
        let path = state_path(&self.work_dir);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                error!(
                    error = %e.display_chain(),
                    path = %path.display(),
                    "failed to read session state; starting empty"
                );
                return;
            }
        };
        let persisted: Vec<PersistedSession> = match serde_json::from_str(&text) {
            Ok(sessions) => sessions,
            Err(e) => {
                error!(
                    error = %e.display_chain(),
                    path = %path.display(),
                    "failed to parse session state; starting empty"
                );
                return;
            }
        };

        let mut failed_to_restore = Vec::new();
        for session in persisted {
            let dir = match session.dir.clone() {
                Some(dir) => dir,
                None => self.work_dir.join(&session.id),
            };
            if !dir.is_dir() {
                warn!(session_id = %session.id, "skipping restore: session directory is missing");
                continue;
            }

            kill_pid_if_alive(session.pid).await;

            let child = match self
                .start_executor(&session.id, &dir, session.executor_port, session.permission)
                .await
            {
                Ok(child) => child,
                Err(e) => {
                    error!(
                        session_id = %session.id,
                        error = %e.display_chain(),
                        "failed to restart executor"
                    );
                    failed_to_restore.push(session);
                    continue;
                }
            };

            let record = SessionRecord {
                id: session.id.clone(),
                repo_url: session.repo_url,
                git_ref: session.git_ref,
                dir,
                reapable: session.reapable,
                status: "running".into(),
                executor_port: Some(session.executor_port),
                permission: session.permission,
            };
            self.sessions
                .write()
                .unwrap()
                .insert(record.id.clone(), record.clone());
            self.processes
                .write()
                .unwrap()
                .insert(record.id.clone(), child);
            info!(session_id = %session.id, "session restored");
        }

        if let Err(e) = self.persist().await {
            warn!(error = %e.display_chain(), "failed to rewrite session state after restore");
        }

        if !failed_to_restore.is_empty() {
            let mut merged = self.persisted_sessions();
            merged.extend(failed_to_restore);
            merged.sort_by(|a, b| a.id.cmp(&b.id));
            if let Err(e) = self.write_state(&merged).await {
                warn!(error = %e.display_chain(), "failed to keep un-restored sessions in state");
            }
        }
    }
}

/// Kills a stale executor before restore re-spawns it. The pid comes from
/// `state.json` and may belong to an executor that died long ago, whose number
/// was reused by an unrelated process; killing that pid would kill whatever
/// the number now names. Only signal when the process at the pid is actually
/// a `bosun executor` from this binary, and never a system pid.
async fn kill_pid_if_alive(pid: u32) {
    if pid <= 1 {
        return;
    }
    let our_exe_name = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let Ok(output) = tokio::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .await
    else {
        return;
    };
    if !output.status.success() {
        debug!(pid = pid, "no stale executor process to kill");
        return;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    if !is_bosun_executor(&command, &our_exe_name) {
        debug!(
            pid = pid,
            "the pid does not name a bosun executor; leaving it alone"
        );
        return;
    }
    info!(pid = pid, "killing stale executor process");
    if let Err(e) = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await
    {
        debug!(pid = pid, error = %e, "failed to terminate stale executor process");
    }
}

/// True when a command line is a `bosun executor`: the executable basename
/// matches ours (or is `bosun`) and one of the arguments is `executor`.
fn is_bosun_executor(command_line: &str, our_exe_name: &str) -> bool {
    let mut parts = command_line.split_whitespace();
    let Some(executable) = parts.next() else {
        return false;
    };
    let basename = executable.rsplit('/').next().unwrap_or(executable);
    if basename != our_exe_name && basename != "bosun" {
        return false;
    }
    parts.any(|arg| arg == "executor")
}

async fn pick_free_port() -> Result<u16, anyhow::Error> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind a free port")?;
    let port = listener
        .local_addr()
        .context("failed to read the bound port")?
        .port();
    Ok(port)
}

async fn wait_for_health(
    client: &reqwest::Client,
    port: u16,
    timeout: Duration,
) -> Result<(), NodeError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let url = format!("http://127.0.0.1:{port}/health");
    loop {
        let healthy = matches!(
            client
                .get(&url)
                .timeout(Duration::from_secs(1))
                .send()
                .await,
            Ok(response) if response.status().is_success()
        );
        if healthy {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(NodeError::HealthTimeout { port });
        }
        tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
    }
}

async fn cleanup(dir: &Path) {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            debug!(
                dir = %dir.display(),
                error = %e,
                "failed to remove session dir during cleanup"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use bosun_common::session::Permission;
    use tempfile::tempdir;

    use super::*;

    fn request(session_id: &str, repo_url: &str) -> NodeCloneRequest {
        NodeCloneRequest {
            session_id: session_id.into(),
            repo_url: repo_url.into(),
            git_ref: None,
            permission: Permission::ReadWrite,
        }
    }

    fn manager(work: &tempfile::TempDir) -> NodeManager {
        NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://127.0.0.1:8090".into(),
            None,
        )
    }

    async fn stub_server() -> u16 {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
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
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn clone_of_bogus_repo_returns_clone_failure() {
        let work = tempdir().unwrap();
        let manager = manager(&work);

        let err = manager
            .run_clone(&request("s2", "file:///nonexistent/bogus-repo"))
            .await
            .expect_err("clone should fail");

        assert!(matches!(err, NodeError::CloneFailed { .. }));
        assert!(manager.sessions().is_empty());
        assert!(!work.path().join("s2").exists());
    }

    #[tokio::test]
    async fn stop_of_unknown_session_is_idempotent() {
        let work = tempdir().unwrap();
        let manager = manager(&work);

        manager
            .stop("s1")
            .await
            .expect("stopping an unknown session should succeed");
        assert!(manager.sessions().is_empty());
    }

    #[test]
    fn to_info_reports_the_dir_for_reapable_and_dev_sessions() {
        let clone = SessionRecord {
            id: "s1".into(),
            repo_url: Some("https://example.com/repo".into()),
            git_ref: None,
            dir: PathBuf::from("/work/s1"),
            reapable: true,
            status: "running".into(),
            executor_port: Some(43210),
            permission: Permission::ReadWrite,
        };
        let dev = SessionRecord {
            reapable: false,
            ..clone.clone()
        };

        assert_eq!(clone.to_info().dir, Some(PathBuf::from("/work/s1")));
        assert_eq!(dev.to_info().dir, Some(PathBuf::from("/work/s1")));
    }

    #[tokio::test]
    async fn wait_for_health_succeeds_against_stub() {
        let port = stub_server().await;
        let client = reqwest::Client::new();
        wait_for_health(&client, port, Duration::from_secs(5))
            .await
            .expect("stub server should report healthy");
    }

    #[tokio::test]
    async fn wait_for_health_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let client = reqwest::Client::new();
        let err = wait_for_health(&client, port, Duration::from_millis(700))
            .await
            .expect_err("health check should time out");
        assert!(matches!(err, NodeError::HealthTimeout { .. }));
    }

    #[tokio::test]
    async fn restore_skips_sessions_whose_dir_is_missing() {
        let work = tempdir().unwrap();
        let manager = manager(&work);
        tokio::fs::write(
            state_path(work.path()),
            r#"[
                {"id":"s1","repo_url":"https://example.com/repo","git_ref":null,"executor_port":43210,"pid":4242}
            ]"#,
        )
        .await
        .unwrap();

        manager.restore().await;

        assert!(manager.sessions().is_empty());
    }

    #[test]
    fn is_bosun_executor_recognizes_only_executor_command_lines() {
        let our = "bosun";
        assert!(is_bosun_executor(
            "/usr/local/bin/bosun executor --session-dir work/s1 --port 51503 --permission read_write",
            our
        ));
        assert!(
            is_bosun_executor(
                "/Users/me/bosun executor --port 1 --permission read_only",
                our
            ),
            "an installed bosun at another path is still an executor"
        );
        assert!(!is_bosun_executor("sleep 30", our));
        assert!(!is_bosun_executor("git status", our));
        assert!(
            !is_bosun_executor("/usr/local/bin/bosun node --config node.toml", our),
            "a bosun process running a different subcommand is not an executor"
        );
        assert!(
            !is_bosun_executor("/usr/bin/executor --session-dir x", our),
            "a different binary named executor is not ours"
        );
        assert!(!is_bosun_executor("", our));
    }

    #[tokio::test]
    async fn kill_pid_if_alive_leaves_unrelated_processes_alone() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();

        kill_pid_if_alive(pid).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(
            alive,
            "a reused pid must not be signalled when it is not a bosun executor"
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn kill_pid_if_alive_refuses_system_pids() {
        // `kill -TERM 1` would signal the system init process; the guard must
        // return without signalling anything.
        kill_pid_if_alive(0).await;
        kill_pid_if_alive(1).await;

        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(alive, "the test's own process must survive");
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[test]
    fn list_dir_without_roots_reports_no_browse_roots() {
        let work = tempdir().unwrap();
        let manager = NodeManager::new(
            work.path().to_path_buf(),
            Vec::new(),
            "http://127.0.0.1:8090".into(),
            None,
        );

        let err = manager.list_dir(None).unwrap_err();
        assert!(matches!(err, NodeError::NoBrowseRoots));
    }

    #[test]
    fn list_dir_without_path_lists_the_roots() {
        let work = tempdir().unwrap();
        std::fs::create_dir_all(work.path().join(".git")).unwrap();
        let manager = NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://127.0.0.1:8090".into(),
            None,
        );

        let listing = manager.list_dir(None).unwrap();
        assert_eq!(listing.path, None);
        assert_eq!(listing.parent, None);
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.entries[0].is_repo);
        let canonical = work.path().canonicalize().unwrap();
        assert_eq!(listing.entries[0].name, canonical.display().to_string());
    }

    #[test]
    fn list_dir_lists_directories_sorted_within_a_root() {
        let work = tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("zebra")).unwrap();
        std::fs::create_dir_all(work.path().join("alpha")).unwrap();
        std::fs::create_dir_all(work.path().join("alpha/.git")).unwrap();
        std::fs::create_dir_all(work.path().join(".hidden")).unwrap();
        std::fs::write(work.path().join("file.txt"), "x").unwrap();
        let manager = manager(&work);

        let listing = manager.list_dir(Some(work.path())).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
        assert!(listing.entries[0].is_repo);
        assert!(!listing.entries[1].is_repo);
        let canonical = work.path().canonicalize().unwrap();
        assert_eq!(listing.path.as_deref(), Some(canonical.as_path()));
    }

    #[test]
    fn list_dir_rejects_missing_and_out_of_root_paths() {
        let work = tempdir().unwrap();
        let manager = manager(&work);

        let err = manager
            .list_dir(Some(&work.path().join("missing")))
            .unwrap_err();
        assert!(matches!(err, NodeError::DirNotFound { .. }));

        let outside = tempdir().unwrap();
        let err = manager.list_dir(Some(outside.path())).unwrap_err();
        assert!(matches!(err, NodeError::OutsideRoot { .. }));
    }

    #[test]
    fn list_dir_rejects_a_file_path() {
        let work = tempdir().unwrap();
        let file = work.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let manager = manager(&work);

        let err = manager.list_dir(Some(&file)).unwrap_err();
        assert!(matches!(err, NodeError::NotADirectory { .. }));
    }

    #[tokio::test]
    async fn dev_rejects_missing_out_of_root_and_file_paths() {
        let work = tempdir().unwrap();
        let manager = manager(&work);

        let missing = manager
            .dev(&NodeDevRequest {
                session_id: "s1".into(),
                dir: work.path().join("missing"),
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(missing, NodeError::DirNotFound { .. }));

        let outside = tempdir().unwrap();
        let err = manager
            .dev(&NodeDevRequest {
                session_id: "s2".into(),
                dir: outside.path().to_path_buf(),
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::OutsideRoot { .. }));

        let file = work.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let err = manager
            .dev(&NodeDevRequest {
                session_id: "s3".into(),
                dir: file,
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::NotADirectory { .. }));
    }

    #[tokio::test]
    async fn dev_without_roots_reports_no_browse_roots() {
        let work = tempdir().unwrap();
        let manager = NodeManager::new(
            work.path().to_path_buf(),
            Vec::new(),
            "http://127.0.0.1:8090".into(),
            None,
        );

        let err = manager
            .dev(&NodeDevRequest {
                session_id: "s1".into(),
                dir: work.path().to_path_buf(),
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::NoBrowseRoots));
    }

    #[tokio::test]
    async fn start_rejects_missing_out_of_root_and_file_paths() {
        let work = tempdir().unwrap();
        let manager = manager(&work);

        let missing = manager
            .start(&NodeStartRequest {
                session_id: "s1".into(),
                dir: work.path().join("missing"),
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(missing, NodeError::DirNotFound { .. }));

        // The escape browse roots close: the directory exists, but it sits
        // outside every root, so the start is refused.
        let outside = tempdir().unwrap();
        let err = manager
            .start(&NodeStartRequest {
                session_id: "s2".into(),
                dir: outside.path().to_path_buf(),
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::OutsideRoot { .. }));

        let file = work.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let err = manager
            .start(&NodeStartRequest {
                session_id: "s3".into(),
                dir: file,
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::NotADirectory { .. }));
    }

    #[tokio::test]
    async fn start_without_browse_roots_refuses_an_existing_dir() {
        // A spawned child's executor has full shell and file access, so a node
        // without browse roots refuses to run one anywhere: the roots gate
        // applies to `start` exactly as it does to `dev`.
        let work = tempdir().unwrap();
        let session_dir = work.path().join("repo");
        std::fs::create_dir_all(&session_dir).unwrap();
        let manager = NodeManager::new(
            work.path().to_path_buf(),
            Vec::new(),
            "http://127.0.0.1:8090".into(),
            None,
        );

        let err = manager
            .start(&NodeStartRequest {
                session_id: "s1".into(),
                dir: session_dir,
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::NoBrowseRoots));
    }

    #[tokio::test]
    async fn start_within_browse_roots_reaches_executor_startup() {
        // Browse roots cover the directory, so the gate lets the start through
        // to executor startup. The node re-execs its own binary as a `bosun
        // executor`, and the unit-test binary is not bosun, so the spawned
        // executor can never become healthy; the health timeout is the proof
        // the start passed the roots gate and ran the executor.
        let work = tempdir().unwrap();
        let session_dir = work.path().join("repo");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut manager = NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://127.0.0.1:8090".into(),
            None,
        );
        manager.health_timeout = Duration::from_millis(700);

        let err = manager
            .start(&NodeStartRequest {
                session_id: "s1".into(),
                dir: session_dir,
                permission: Permission::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::HealthTimeout { .. }));
    }
}
