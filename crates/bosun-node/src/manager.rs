use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::types::DirEntry;
use bosun_common::types::DirListing;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeDevRequest;
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
    pub opencode_port: Option<u16>,
}

impl SessionRecord {
    pub fn to_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            repo_url: self.repo_url.clone(),
            git_ref: self.git_ref.clone(),
            dir: if self.reapable {
                None
            } else {
                Some(self.dir.clone())
            },
            status: self.status.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("failed to clone {repo_url}: {stderr}")]
    CloneFailed { repo_url: String, stderr: String },

    #[error("opencode server on port {port} did not become healthy")]
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
    sessions: RwLock<HashMap<String, SessionRecord>>,
    processes: RwLock<HashMap<String, Child>>,
    tunnels: RwLock<HashMap<String, tokio::task::JoinHandle<()>>>,
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
            sessions: RwLock::new(HashMap::new()),
            processes: RwLock::new(HashMap::new()),
            tunnels: RwLock::new(HashMap::new()),
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
            )
            .await?;
        info!(session_id = %req.session_id, "clone session started");
        Ok(record)
    }

    pub async fn dev(&self, req: &NodeDevRequest) -> Result<SessionRecord, NodeError> {
        let dir = self.resolve_within_roots(&req.dir)?;
        let record = self
            .start_in_dir(&req.session_id, &dir, false, None, None)
            .await?;
        info!(session_id = %req.session_id, dir = %record.dir.display(), "dev session started");
        Ok(record)
    }

    async fn start_in_dir(
        &self,
        session_id: &str,
        dir: &Path,
        reapable: bool,
        repo_url: Option<String>,
        git_ref: Option<String>,
    ) -> Result<SessionRecord, NodeError> {
        let port = pick_free_port().await?;

        let child = match self.start_server(session_id, dir, port).await {
            Ok(child) => child,
            Err(e) => {
                if reapable {
                    cleanup(dir).await;
                }
                return Err(e);
            }
        };

        self.open_tunnel(session_id, port);

        let record = SessionRecord {
            id: session_id.to_string(),
            repo_url,
            git_ref,
            dir: dir.to_path_buf(),
            reapable,
            status: "running".into(),
            opencode_port: Some(port),
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
            child.kill().await.with_context(|| {
                format!("failed to kill opencode serve for session {session_id}")
            })?;
        }

        let tunnel = self.tunnels.write().unwrap().remove(session_id);
        if let Some(handle) = tunnel {
            handle.abort();
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

    async fn start_server(&self, id: &str, dir: &Path, port: u16) -> Result<Child, NodeError> {
        let child = tokio::process::Command::new("opencode")
            .args(serve_args(id, port, &self.cp_url))
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start opencode serve for session {id}"))?;

        let client = reqwest::Client::new();
        wait_for_health(&client, port, HEALTH_TIMEOUT).await?;
        Ok(child)
    }

    /// Opens the session's outbound tunnel to the control plane. The tunnel
    /// reconnects on its own until the session stops, so a refused connection
    /// does not fail the session.
    fn open_tunnel(&self, session_id: &str, opencode_port: u16) {
        let cp_url = self.cp_url.clone();
        let session_id = session_id.to_string();
        let tls_config = self.tls_config.clone();
        let handle = tokio::spawn(crate::tunnel::run_session_tunnel(
            cp_url,
            session_id.clone(),
            opencode_port,
            tls_config,
        ));
        let _ = self.tunnels.write().unwrap().insert(session_id, handle);
    }

    fn persisted_sessions(&self) -> Vec<PersistedSession> {
        let sessions = self.sessions.read().unwrap();
        let processes = self.processes.read().unwrap();
        let mut persisted: Vec<PersistedSession> = sessions
            .values()
            .filter_map(|record| {
                let port = record.opencode_port?;
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
                    opencode_port: port,
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
                .start_server(&session.id, &dir, session.opencode_port)
                .await
            {
                Ok(child) => child,
                Err(e) => {
                    error!(
                        session_id = %session.id,
                        error = %e.display_chain(),
                        "failed to restart opencode serve"
                    );
                    failed_to_restore.push(session);
                    continue;
                }
            };

            self.open_tunnel(&session.id, session.opencode_port);

            let record = SessionRecord {
                id: session.id.clone(),
                repo_url: session.repo_url,
                git_ref: session.git_ref,
                dir,
                reapable: session.reapable,
                status: "running".into(),
                opencode_port: Some(session.opencode_port),
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

/// Builds the `opencode serve` arguments for a session. The session's web UI
/// is served at `<session-id>.<control-plane-host>`, so the node passes that
/// origin to `--cors`; without it the browser's cross-origin requests to the
/// subdomain would be rejected.
fn serve_args(id: &str, port: u16, cp_url: &str) -> Vec<String> {
    let mut args = vec![
        "serve".into(),
        "--hostname".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
    ];
    if let Some(origin) = bosun_common::origin::session_origin(cp_url, id) {
        args.push("--cors".into());
        args.push(origin);
    }
    args
}

async fn kill_pid_if_alive(pid: u32) {
    if pid == 0 {
        return;
    }
    let alive = tokio::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false);
    if !alive {
        debug!(pid = pid, "no stale opencode process to kill");
        return;
    }
    info!(pid = pid, "killing stale opencode process");
    if let Err(e) = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await
    {
        debug!(pid = pid, error = %e, "failed to terminate stale opencode process");
    }
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
    let url = format!("http://127.0.0.1:{port}/global/health");
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
    use tempfile::tempdir;

    use super::*;

    fn request(session_id: &str, repo_url: &str) -> NodeCloneRequest {
        NodeCloneRequest {
            session_id: session_id.into(),
            repo_url: repo_url.into(),
            git_ref: None,
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
                {"id":"s1","repo_url":"https://example.com/repo","git_ref":null,"opencode_port":43210,"pid":4242}
            ]"#,
        )
        .await
        .unwrap();

        manager.restore().await;

        assert!(manager.sessions().is_empty());
    }

    #[tokio::test]
    async fn kill_pid_if_alive_terminates_a_process() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();

        kill_pid_if_alive(pid).await;

        let output = child.wait().await.unwrap();
        assert!(!output.success());
    }

    #[test]
    fn serve_args_include_the_session_cors_origin() {
        let args = serve_args("s1", 4321, "https://bosun.on.21cs.biz");
        assert!(args.contains(&"--cors".to_string()));
        assert!(args.contains(&"https://s1.bosun.on.21cs.biz".to_string()));

        let local = serve_args("s1", 4321, "http://127.0.0.1:8090");
        assert!(local.contains(&"http://s1.localhost:8090".to_string()));
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
            })
            .await
            .unwrap_err();
        assert!(matches!(missing, NodeError::DirNotFound { .. }));

        let outside = tempdir().unwrap();
        let err = manager
            .dev(&NodeDevRequest {
                session_id: "s2".into(),
                dir: outside.path().to_path_buf(),
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
            })
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::NoBrowseRoots));
    }
}
