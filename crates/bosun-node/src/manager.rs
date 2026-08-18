use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::types::NodeSpawnRequest;
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
    pub repo_url: String,
    pub git_ref: Option<String>,
    pub status: String,
    pub dir: PathBuf,
    pub opencode_port: Option<u16>,
    pub forwarder_addr: Option<String>,
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("failed to clone {repo_url}: {stderr}")]
    CloneFailed { repo_url: String, stderr: String },

    #[error("opencode server on port {port} did not become healthy")]
    HealthTimeout { port: u16 },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

pub struct NodeManager {
    work_dir: PathBuf,
    advertise_addr: String,
    sessions: RwLock<HashMap<String, SessionRecord>>,
    processes: RwLock<HashMap<String, Child>>,
    forwarders: RwLock<HashMap<String, tokio::task::AbortHandle>>,
}

impl NodeManager {
    pub fn new(work_dir: PathBuf, advertise_addr: String) -> Self {
        Self {
            work_dir,
            advertise_addr,
            sessions: RwLock::new(HashMap::new()),
            processes: RwLock::new(HashMap::new()),
            forwarders: RwLock::new(HashMap::new()),
        }
    }

    pub async fn spawn(&self, req: &NodeSpawnRequest) -> Result<SessionRecord, NodeError> {
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

        let config_path = dir.join("opencode.json");
        if let Err(e) = tokio::fs::write(&config_path, &req.opencode_config)
            .await
            .with_context(|| {
                format!(
                    "failed to write opencode config to {}",
                    config_path.display()
                )
            })
        {
            cleanup(&dir).await;
            return Err(NodeError::Internal(e));
        }

        let port = match pick_free_port().await {
            Ok(port) => port,
            Err(e) => {
                cleanup(&dir).await;
                return Err(NodeError::Internal(e));
            }
        };

        let mut child = match self.start_server(&req.session_id, &dir, port).await {
            Ok(child) => child,
            Err(e) => {
                cleanup(&dir).await;
                return Err(e);
            }
        };

        let forwarder_addr = match self.open_forwarder(&req.session_id, port).await {
            Ok(addr) => addr,
            Err(e) => {
                let _ = child.kill().await;
                cleanup(&dir).await;
                return Err(e);
            }
        };

        let record = SessionRecord {
            id: req.session_id.clone(),
            repo_url: req.repo_url.clone(),
            git_ref: req.git_ref.clone(),
            status: "running".into(),
            dir,
            opencode_port: Some(port),
            forwarder_addr: Some(forwarder_addr),
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

        let forwarder = self.forwarders.write().unwrap().remove(session_id);
        if let Some(handle) = forwarder {
            handle.abort();
        }

        self.sessions.write().unwrap().remove(session_id);
        cleanup(&record.dir).await;

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
            .map(|record| SessionInfo {
                id: record.id.clone(),
                repo_url: record.repo_url.clone(),
                git_ref: record.git_ref.clone(),
                status: record.status.clone(),
                opencode_port: record.opencode_port,
                forwarder_addr: record.forwarder_addr.clone(),
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }

    async fn start_server(&self, id: &str, dir: &Path, port: u16) -> Result<Child, NodeError> {
        let child = tokio::process::Command::new("opencode")
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
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

    async fn open_forwarder(&self, id: &str, opencode_port: u16) -> Result<String, NodeError> {
        let target = SocketAddr::from(([127, 0, 0, 1], opencode_port));
        let (addr, handle) =
            bosun_common::forward::start(&format!("{}:0", self.advertise_addr), target).await?;
        self.forwarders
            .write()
            .unwrap()
            .insert(id.to_string(), handle);
        Ok(format!("{}:{}", self.advertise_addr, addr.port()))
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
            let dir = self.work_dir.join(&session.id);
            if !dir.is_dir() {
                warn!(session_id = %session.id, "skipping restore: session directory is missing");
                continue;
            }

            kill_pid_if_alive(session.pid).await;

            let mut child = match self
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

            let forwarder_addr = match self
                .open_forwarder(&session.id, session.opencode_port)
                .await
            {
                Ok(addr) => addr,
                Err(e) => {
                    let _ = child.kill().await;
                    error!(
                        session_id = %session.id,
                        error = %e.display_chain(),
                        "failed to restart forwarder"
                    );
                    failed_to_restore.push(session);
                    continue;
                }
            };

            let record = SessionRecord {
                id: session.id.clone(),
                repo_url: session.repo_url,
                git_ref: session.git_ref,
                status: "running".into(),
                dir,
                opencode_port: Some(session.opencode_port),
                forwarder_addr: Some(forwarder_addr),
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

    fn request(session_id: &str, repo_url: &str) -> NodeSpawnRequest {
        NodeSpawnRequest {
            session_id: session_id.into(),
            repo_url: repo_url.into(),
            git_ref: None,
            opencode_config: String::new(),
        }
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
    async fn spawn_of_bogus_repo_returns_clone_failure() {
        let work = tempdir().unwrap();
        let manager = NodeManager::new(work.path().to_path_buf(), "127.0.0.1".into());

        let err = manager
            .spawn(&request("s2", "file:///nonexistent/bogus-repo"))
            .await
            .expect_err("clone should fail");

        assert!(matches!(err, NodeError::CloneFailed { .. }));
        assert!(manager.sessions().is_empty());
        assert!(!work.path().join("s2").exists());
    }

    #[tokio::test]
    async fn stop_of_unknown_session_is_idempotent() {
        let work = tempdir().unwrap();
        let manager = NodeManager::new(work.path().to_path_buf(), "127.0.0.1".into());

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
        let manager = NodeManager::new(work.path().to_path_buf(), "127.0.0.1".into());
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
}
