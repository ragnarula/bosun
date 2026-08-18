use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use bosun_common::types::NodeSpawnRequest;
use bosun_common::types::SessionInfo;
use thiserror::Error;
use tokio::process::Child;
use tracing::debug;

use crate::forwarder::accept_loop;

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
    #[allow(dead_code)]
    processes: RwLock<HashMap<String, Child>>,
    #[allow(dead_code)]
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

        let mut child = match tokio::process::Command::new("opencode")
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start opencode serve for session {}",
                    req.session_id
                )
            }) {
            Ok(child) => child,
            Err(e) => {
                cleanup(&dir).await;
                return Err(NodeError::Internal(e));
            }
        };

        let client = reqwest::Client::new();
        if let Err(e) = wait_for_health(&client, port, HEALTH_TIMEOUT).await {
            let _ = child.kill().await;
            cleanup(&dir).await;
            return Err(e);
        }

        let (forwarder_addr, forwarder_handle) =
            match start_forwarder(&self.advertise_addr, port).await {
                Ok(result) => result,
                Err(e) => {
                    let _ = child.kill().await;
                    cleanup(&dir).await;
                    return Err(NodeError::Internal(e));
                }
            };
        self.forwarders
            .write()
            .unwrap()
            .insert(req.session_id.clone(), forwarder_handle);

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
        Ok(record)
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
}

async fn start_forwarder(
    advertise_addr: &str,
    opencode_port: u16,
) -> Result<(String, tokio::task::AbortHandle), anyhow::Error> {
    let listener = tokio::net::TcpListener::bind(format!("{advertise_addr}:0"))
        .await
        .with_context(|| format!("failed to bind forwarder on {advertise_addr}"))?;
    let forwarder_port = listener
        .local_addr()
        .context("failed to read the bound forwarder port")?
        .port();
    let forwarder_addr = format!("{advertise_addr}:{forwarder_port}");
    let target = SocketAddr::from(([127, 0, 0, 1], opencode_port));
    let handle = tokio::spawn(accept_loop(listener, target)).abort_handle();
    Ok((forwarder_addr, handle))
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
}
