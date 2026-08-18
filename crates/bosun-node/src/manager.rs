use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::Context;
use bosun_common::types::NodeSpawnRequest;
use bosun_common::types::SessionInfo;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub repo_url: String,
    pub git_ref: Option<String>,
    pub status: String,
    pub dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("failed to clone {repo_url}: {stderr}")]
    CloneFailed { repo_url: String, stderr: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

pub struct NodeManager {
    work_dir: PathBuf,
    sessions: RwLock<HashMap<String, SessionRecord>>,
}

impl NodeManager {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir,
            sessions: RwLock::new(HashMap::new()),
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
            return Err(NodeError::CloneFailed {
                repo_url: req.repo_url.clone(),
                stderr,
            });
        }

        let record = SessionRecord {
            id: req.session_id.clone(),
            repo_url: req.repo_url.clone(),
            git_ref: req.git_ref.clone(),
            status: "ready".into(),
            dir,
        };
        self.sessions
            .write()
            .unwrap()
            .insert(record.id.clone(), record.clone());
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
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    fn init_repo(path: &std::path::Path) {
        let status = Command::new("git")
            .args(["init", "-q"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");
    }

    fn request(session_id: &str, repo_url: &str) -> NodeSpawnRequest {
        NodeSpawnRequest {
            session_id: session_id.into(),
            repo_url: repo_url.into(),
            git_ref: None,
            opencode_config: String::new(),
        }
    }

    #[tokio::test]
    async fn spawn_clones_repo_and_marks_ready() {
        let work = tempdir().unwrap();
        let repo = tempdir().unwrap();
        init_repo(repo.path());

        let manager = NodeManager::new(work.path().to_path_buf());
        let record = manager
            .spawn(&request("s1", &format!("file://{}", repo.path().display())))
            .await
            .expect("clone should succeed");

        assert_eq!(record.status, "ready");
        assert!(record.dir.join(".git").is_dir());

        let sessions = manager.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
        assert_eq!(sessions[0].status, "ready");
    }

    #[tokio::test]
    async fn spawn_of_bogus_repo_returns_clone_failure() {
        let work = tempdir().unwrap();
        let manager = NodeManager::new(work.path().to_path_buf());

        let err = manager
            .spawn(&request("s2", "file:///nonexistent/bogus-repo"))
            .await
            .expect_err("clone should fail");

        assert!(matches!(err, NodeError::CloneFailed { .. }));
        assert!(manager.sessions().is_empty());
    }
}
