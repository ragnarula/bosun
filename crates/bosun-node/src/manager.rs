use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::session::Permission;
use bosun_common::types::DirEntry;
use bosun_common::types::DirListing;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeDevRequest;
use bosun_common::types::NodeStartRequest;
use bosun_common::types::SessionInfo;
use bosun_executor::ExecutorState;
use thiserror::Error;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::state::PersistedSession;
use crate::state::state_path;

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub repo_url: Option<String>,
    pub git_ref: Option<String>,
    pub dir: PathBuf,
    pub reapable: bool,
    pub status: String,
    pub permission: Permission,
    /// The session's in-process executor. The node owns one state per session
    /// and relays tool calls to it; no executor process, port, or pid exists.
    pub executor: Arc<ExecutorState>,
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
        let dir = resolve_within_roots(&self.browse_roots, &req.dir)?;
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
        let dir = resolve_within_roots(&self.browse_roots, &req.dir)?;
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
        let record = SessionRecord {
            id: session_id.to_string(),
            repo_url,
            git_ref,
            dir: dir.to_path_buf(),
            reapable,
            status: "running".into(),
            permission,
            executor: Arc::new(ExecutorState::new(dir.to_path_buf(), permission)),
        };
        self.sessions
            .write()
            .unwrap()
            .insert(record.id.clone(), record.clone());

        if let Err(e) = self.persist().await {
            warn!(
                session_id = %record.id,
                error = %e.display_chain(),
                "failed to persist session state"
            );
        }
        Ok(record)
    }

    /// Lists directories within the browse roots. Runs on the blocking pool
    /// because directory walks can be slow, and the node's runtime now serves
    /// every session's tools too.
    pub async fn list_dir(&self, requested: Option<&Path>) -> Result<DirListing, NodeError> {
        let requested = requested.map(PathBuf::from);
        let browse_roots = self.browse_roots.clone();
        tokio::task::spawn_blocking(move || list_dir_blocking(&browse_roots, requested.as_deref()))
            .await
            .map_err(|error| NodeError::Internal(anyhow::Error::from(error)))?
    }

    pub async fn stop(&self, session_id: &str) -> Result<(), NodeError> {
        let record = match self.sessions.write().unwrap().remove(session_id) {
            Some(record) => record,
            None => {
                info!(session_id = %session_id, "stop requested for unknown session");
                return Ok(());
            }
        };
        // In-flight shells die with the session instead of outliving it.
        record.executor.kill_all_shells().await;
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

    /// The executor state of one running session. The node's tunnel relay
    /// dispatches a logical connection addressed to the session to it.
    pub fn executor(&self, session_id: &str) -> Option<Arc<ExecutorState>> {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .map(|record| record.executor.clone())
    }

    fn persisted_sessions(&self) -> Vec<PersistedSession> {
        let sessions = self.sessions.read().unwrap();
        let mut persisted: Vec<PersistedSession> = sessions
            .values()
            .map(|record| PersistedSession {
                id: record.id.clone(),
                repo_url: record.repo_url.clone(),
                git_ref: record.git_ref.clone(),
                dir: if record.reapable {
                    None
                } else {
                    Some(record.dir.clone())
                },
                reapable: record.reapable,
                permission: record.permission,
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

    /// Rebuilds every persisted session's executor state at boot from
    /// `state.json`, without spawning an executor process. Sessions whose
    /// directory is gone are dropped from the state.
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

        for session in persisted {
            let dir = match session.dir.clone() {
                Some(dir) => dir,
                None => self.work_dir.join(&session.id),
            };
            if !dir.is_dir() {
                warn!(session_id = %session.id, "skipping restore: session directory is missing");
                continue;
            }

            let record = SessionRecord {
                id: session.id.clone(),
                repo_url: session.repo_url,
                git_ref: session.git_ref,
                dir: dir.clone(),
                reapable: session.reapable,
                status: "running".into(),
                permission: session.permission,
                executor: Arc::new(ExecutorState::new(dir, session.permission)),
            };
            self.sessions
                .write()
                .unwrap()
                .insert(record.id.clone(), record);
            info!(session_id = %session.id, "session restored");
        }

        if let Err(e) = self.persist().await {
            warn!(error = %e.display_chain(), "failed to rewrite session state after restore");
        }
    }
}

/// Lists directories within `browse_roots`, or the roots themselves when no
/// path is requested.
fn list_dir_blocking(
    browse_roots: &[PathBuf],
    requested: Option<&Path>,
) -> Result<DirListing, NodeError> {
    let Some(requested) = requested else {
        return list_roots(browse_roots);
    };
    let canonical = resolve_within_roots(browse_roots, requested)?;

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
        .filter(|parent| within_roots(browse_roots, parent))
        .map(PathBuf::from);

    Ok(DirListing {
        path: Some(canonical),
        parent,
        entries,
    })
}

fn list_roots(browse_roots: &[PathBuf]) -> Result<DirListing, NodeError> {
    if browse_roots.is_empty() {
        return Err(NodeError::NoBrowseRoots);
    }
    let entries = browse_roots
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

/// Resolves `requested` inside the browse roots, refusing missing paths,
/// files, and escapes.
fn resolve_within_roots(browse_roots: &[PathBuf], requested: &Path) -> Result<PathBuf, NodeError> {
    if browse_roots.is_empty() {
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
    if !within_roots(browse_roots, &canonical) {
        return Err(NodeError::OutsideRoot {
            dir: requested.display().to_string(),
        });
    }
    Ok(canonical)
}

fn within_roots(browse_roots: &[PathBuf], path: &Path) -> bool {
    browse_roots.iter().any(|root| path.starts_with(root))
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
    use std::time::Duration;

    use bosun_common::session::Permission;
    use futures_util::StreamExt;
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

    async fn start_session(manager: &NodeManager, session_id: &str, dir: &Path) -> SessionRecord {
        manager
            .start(&NodeStartRequest {
                session_id: session_id.into(),
                dir: dir.to_path_buf(),
                permission: Permission::ReadWrite,
            })
            .await
            .expect("the session should start")
    }

    /// Waits until the shell's owner task published the child's pid. The
    /// running map registers the run with pid 0 before the child exists, so a
    /// real pid is the signal that the shell is up.
    async fn wait_for_shell_pid(executor: &Arc<ExecutorState>, run_id: &str) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let pid = executor
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
            permission: Permission::ReadWrite,
            executor: Arc::new(ExecutorState::new(
                PathBuf::from("/work/s1"),
                Permission::ReadWrite,
            )),
        };
        let dev = SessionRecord {
            reapable: false,
            ..clone.clone()
        };

        assert_eq!(clone.to_info().dir, Some(PathBuf::from("/work/s1")));
        assert_eq!(dev.to_info().dir, Some(PathBuf::from("/work/s1")));
    }

    #[tokio::test]
    async fn restore_skips_sessions_whose_dir_is_missing() {
        let work = tempdir().unwrap();
        let manager = manager(&work);
        tokio::fs::write(
            state_path(work.path()),
            r#"[
                {"id":"s1","repo_url":"https://example.com/repo","git_ref":null,"permission":"read_write"}
            ]"#,
        )
        .await
        .unwrap();

        manager.restore().await;

        assert!(manager.sessions().is_empty());
    }

    #[tokio::test]
    async fn restore_rebuilds_executors_and_drops_stale_rows_from_state() {
        let work = tempdir().unwrap();
        let dir = work.path().join("s1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            state_path(work.path()),
            r#"[
                {"id":"s1","repo_url":null,"git_ref":null,"dir":"DIR","reapable":false,"permission":"read_only"},
                {"id":"gone","repo_url":null,"git_ref":null,"dir":null,"reapable":true,"permission":"read_write"}
            ]"#
            .replace("DIR", &dir.display().to_string()),
        )
        .await
        .unwrap();

        let manager = manager(&work);
        manager.restore().await;

        let sessions = manager.sessions();
        assert_eq!(sessions.len(), 1, "the missing-dir row is dropped");
        assert_eq!(sessions[0].id, "s1");

        let executor = manager.executor("s1").expect("s1 has an executor");
        let permission = *executor.permission.read().await;
        assert_eq!(permission, Permission::ReadOnly);
        assert_eq!(executor.session_dir, dir);

        // The rewritten state no longer names the dropped session.
        let text = tokio::fs::read_to_string(state_path(work.path()))
            .await
            .unwrap();
        assert!(!text.contains("gone"), "state was rewritten: {text}");
        assert!(!text.contains("executor_port"), "no port persists: {text}");
        assert!(!text.contains("\"pid\""), "no pid persists: {text}");
    }

    #[tokio::test]
    async fn old_state_json_with_executor_fields_still_restores() {
        let work = tempdir().unwrap();
        let dir = work.path().join("s1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            state_path(work.path()),
            r#"[
                {"id":"s1","repo_url":null,"git_ref":null,"executor_port":43210,"pid":4242,"permission":"read_write"}
            ]"#,
        )
        .await
        .unwrap();

        let manager = manager(&work);
        manager.restore().await;

        let sessions = manager.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
        assert!(manager.executor("s1").is_some());
    }

    #[tokio::test]
    async fn list_dir_without_roots_reports_no_browse_roots() {
        let work = tempdir().unwrap();
        let manager = NodeManager::new(
            work.path().to_path_buf(),
            Vec::new(),
            "http://127.0.0.1:8090".into(),
            None,
        );

        let err = manager.list_dir(None).await.unwrap_err();
        assert!(matches!(err, NodeError::NoBrowseRoots));
    }

    #[tokio::test]
    async fn list_dir_without_path_lists_the_roots() {
        let work = tempdir().unwrap();
        std::fs::create_dir_all(work.path().join(".git")).unwrap();
        let manager = NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://127.0.0.1:8090".into(),
            None,
        );

        let listing = manager.list_dir(None).await.unwrap();
        assert_eq!(listing.path, None);
        assert_eq!(listing.parent, None);
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.entries[0].is_repo);
        let canonical = work.path().canonicalize().unwrap();
        assert_eq!(listing.entries[0].name, canonical.display().to_string());
    }

    #[tokio::test]
    async fn list_dir_lists_directories_sorted_within_a_root() {
        let work = tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("zebra")).unwrap();
        std::fs::create_dir_all(work.path().join("alpha")).unwrap();
        std::fs::create_dir_all(work.path().join("alpha/.git")).unwrap();
        std::fs::create_dir_all(work.path().join(".hidden")).unwrap();
        std::fs::write(work.path().join("file.txt"), "x").unwrap();
        let manager = manager(&work);

        let listing = manager.list_dir(Some(work.path())).await.unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
        assert!(listing.entries[0].is_repo);
        assert!(!listing.entries[1].is_repo);
        let canonical = work.path().canonicalize().unwrap();
        assert_eq!(listing.path.as_deref(), Some(canonical.as_path()));
    }

    #[tokio::test]
    async fn list_dir_rejects_missing_and_out_of_root_paths() {
        let work = tempdir().unwrap();
        let manager = manager(&work);

        let err = manager
            .list_dir(Some(&work.path().join("missing")))
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::DirNotFound { .. }));

        let outside = tempdir().unwrap();
        let err = manager.list_dir(Some(outside.path())).await.unwrap_err();
        assert!(matches!(err, NodeError::OutsideRoot { .. }));
    }

    #[tokio::test]
    async fn list_dir_rejects_a_file_path() {
        let work = tempdir().unwrap();
        let file = work.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let manager = manager(&work);

        let err = manager.list_dir(Some(&file)).await.unwrap_err();
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
    async fn start_registers_an_in_process_executor_and_persists_it() {
        let work = tempdir().unwrap();
        let session_dir = work.path().join("repo");
        std::fs::create_dir_all(&session_dir).unwrap();
        let node = manager(&work);

        let record = start_session(&node, "s1", &session_dir).await;
        assert_eq!(record.status, "running");

        let executor = node.executor("s1").expect("the session has an executor");
        assert_eq!(executor.session_dir, session_dir.canonicalize().unwrap());

        // The state file carries the session but neither a port nor a pid.
        let text = tokio::fs::read_to_string(state_path(work.path()))
            .await
            .unwrap();
        assert!(text.contains("s1"));
        assert!(!text.contains("executor_port"), "no port persists: {text}");
        assert!(!text.contains("\"pid\""), "no pid persists: {text}");

        // A fresh manager restores the session from the file.
        drop(node);
        let restored = manager(&work);
        restored.restore().await;
        assert_eq!(restored.sessions().len(), 1);
        assert!(restored.executor("s1").is_some());
    }

    #[tokio::test]
    async fn stop_removes_the_session_and_kills_its_running_shell() {
        #[cfg(unix)]
        {
            let work = tempdir().unwrap();
            let session_dir = work.path().join("repo");
            std::fs::create_dir_all(&session_dir).unwrap();
            let manager = manager(&work);

            start_session(&manager, "s1", &session_dir).await;
            let executor = manager.executor("s1").expect("the session has an executor");

            // Start a long-running shell through the session's executor and
            // keep draining its stream, as the relay would.
            let outcome = bosun_executor::run_call(
                &executor,
                "run-1",
                "shell",
                &serde_json::json!({ "command": "sleep 51" }),
            )
            .await
            .expect("the shell should start");
            let bosun_executor::CallOutcome::Shell(stream) = outcome else {
                panic!("shell must stream");
            };
            let pid = wait_for_shell_pid(&executor, "run-1").await;
            let collector = tokio::spawn(stream.collect::<Vec<_>>());

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
            {
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell never started");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            manager
                .stop("s1")
                .await
                .expect("stopping the session should succeed");
            assert!(manager.sessions().is_empty());
            assert!(manager.executor("s1").is_none());

            // The stop killed the in-flight shell: the process group dies and
            // the stream ends with a killed-run code.
            let events = tokio::time::timeout(Duration::from_secs(5), collector)
                .await
                .expect("the shell stream must end after the stop")
                .unwrap();
            let killed = events.iter().any(|event| match event {
                bosun_executor::ShellEvent::Done(code) => *code == -1,
                bosun_executor::ShellEvent::Out(_) => false,
            });
            assert!(killed, "the stop must end the shell with a killed-run code");

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let alive = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if !alive {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell survived the session stop");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    /// A session stop that lands while one of its shells is still starting —
    /// the run is registered but the stream has not been returned — still
    /// kills it. The run is registered before the child is spawned, so the
    /// stop's kill-all cannot run in a window that finds nothing and leaves
    /// an orphaned shell behind.
    #[tokio::test]
    async fn a_stop_concurrent_with_a_starting_shell_kills_it_and_empties_running() {
        #[cfg(unix)]
        {
            let work = tempdir().unwrap();
            let session_dir = work.path().join("repo");
            std::fs::create_dir_all(&session_dir).unwrap();
            let manager = manager(&work);
            start_session(&manager, "s1", &session_dir).await;

            // The dispatch holds the executor Arc, exactly as a relay task
            // does while a tool call is in flight.
            let executor = manager.executor("s1").expect("the session has an executor");
            let run_id = "run-starting";
            let outcome = tokio::spawn({
                let executor = executor.clone();
                async move {
                    bosun_executor::run_call(
                        &executor,
                        run_id,
                        "shell",
                        &serde_json::json!({ "command": "sleep 72" }),
                    )
                    .await
                }
            });

            // The run is registered before the child is spawned, so the stop
            // below can land while the shell is still starting.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if executor.running.read().await.contains_key(run_id) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the run was never registered");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            manager
                .stop("s1")
                .await
                .expect("stopping the session should succeed");
            assert!(manager.sessions().is_empty());
            assert!(manager.executor("s1").is_none());

            // The in-flight call still answers, with a run that ends killed.
            let outcome = tokio::time::timeout(Duration::from_secs(5), outcome)
                .await
                .expect("the shell call must complete within 5 seconds")
                .expect("the shell call task must not panic")
                .expect("the shell call should start");
            let bosun_executor::CallOutcome::Shell(stream) = outcome else {
                panic!("shell must stream");
            };
            let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
                .await
                .expect("the stream must end after the stop");
            let killed = events.iter().any(|event| match event {
                bosun_executor::ShellEvent::Done(code) => *code == -1,
                bosun_executor::ShellEvent::Out(_) => false,
            });
            assert!(
                killed,
                "the stop must end the starting shell with a killed-run code"
            );

            // The stop emptied the executor's running map and the shell's
            // process group is gone.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let empty = executor.running.read().await.is_empty();
                let alive = std::process::Command::new("pgrep")
                    .args(["-f", "sleep 72"])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if empty && !alive {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("the shell survived the stop that arrived while it was starting");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    #[tokio::test]
    async fn stop_of_a_reapable_session_removes_its_dir() {
        let work = tempdir().unwrap();
        let manager = manager(&work);
        let dir = work.path().join("s1");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Clone sessions are reapable; simulate one by starting in the dir and
        // marking the record reapable.
        let record = start_session(&manager, "s1", &dir).await;
        assert!(!record.reapable);
        manager
            .sessions
            .write()
            .unwrap()
            .get_mut("s1")
            .unwrap()
            .reapable = true;

        manager.stop("s1").await.unwrap();
        assert!(!dir.exists(), "a stopped clone's dir is cleaned up");
    }
}
