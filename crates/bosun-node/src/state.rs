use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub id: String,
    pub repo_url: String,
    pub git_ref: Option<String>,
    pub opencode_port: u16,
    pub pid: u32,
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
            repo_url: "https://example.com/repo".into(),
            git_ref: Some("main".into()),
            opencode_port: 43210,
            pid: 4242,
        };
        let json = serde_json::to_string(&session).unwrap();
        let decoded: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, session.id);
        assert_eq!(decoded.repo_url, session.repo_url);
        assert_eq!(decoded.git_ref, session.git_ref);
        assert_eq!(decoded.opencode_port, session.opencode_port);
        assert_eq!(decoded.pid, session.pid);
    }

    #[test]
    fn state_path_joins_under_work_dir() {
        assert_eq!(
            state_path(Path::new("/tmp/bosun")),
            PathBuf::from("/tmp/bosun/state.json")
        );
    }
}
