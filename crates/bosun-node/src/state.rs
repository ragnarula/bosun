use std::path::Path;
use std::path::PathBuf;

use bosun_common::session::Permission;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub id: String,
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub dir: Option<PathBuf>,
    #[serde(default = "default_reapable")]
    pub reapable: bool,
    #[serde(default = "default_permission")]
    pub permission: Permission,
}

fn default_reapable() -> bool {
    true
}

fn default_permission() -> Permission {
    Permission::ReadWrite
}

pub fn state_path(work_dir: &Path) -> PathBuf {
    work_dir.join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_session_round_trips_through_json() {
        let session = PersistedSession {
            id: "s1".into(),
            repo_url: Some("https://example.com/repo".into()),
            git_ref: Some("main".into()),
            dir: None,
            reapable: true,
            permission: Permission::ReadOnly,
        };
        let json = serde_json::to_string(&session).unwrap();
        let decoded: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, session.id);
        assert_eq!(decoded.repo_url, session.repo_url);
        assert_eq!(decoded.git_ref, session.git_ref);
        assert_eq!(decoded.dir, session.dir);
        assert_eq!(decoded.reapable, session.reapable);
        assert_eq!(decoded.permission, session.permission);
    }

    #[test]
    fn old_state_json_with_executor_fields_still_parses() {
        // Rows written before the in-process executor decision carried a port
        // and pid; serde ignores the unknown fields so restore keeps working.
        let json = r#"[
            {"id":"s1","repo_url":"https://example.com/repo","git_ref":null,"executor_port":43210,"pid":4242,"permission":"read_write"}
        ]"#;
        let decoded: Vec<PersistedSession> = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].repo_url, Some("https://example.com/repo".into()));
        assert_eq!(decoded[0].dir, None);
        assert!(decoded[0].reapable);
        assert_eq!(decoded[0].permission, Permission::ReadWrite);
    }

    #[test]
    fn state_json_never_serializes_a_port_or_pid() {
        let session = PersistedSession {
            id: "s1".into(),
            repo_url: None,
            git_ref: None,
            dir: Some(PathBuf::from("/work/s1")),
            reapable: false,
            permission: Permission::ReadOnly,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("executor_port"));
        assert!(!json.contains("\"pid\""));
    }

    #[test]
    fn state_path_joins_under_work_dir() {
        assert_eq!(
            state_path(Path::new("/tmp/bosun")),
            PathBuf::from("/tmp/bosun/state.json")
        );
    }
}
