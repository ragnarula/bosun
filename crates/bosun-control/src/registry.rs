use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bosun_common::types::SessionInfo;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealth {
    pub name: String,
    pub up: bool,
    pub last_seen_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHealth {
    pub id: String,
    pub node: String,
    pub repo_url: Option<String>,
    pub git_ref: Option<String>,
    pub dir: Option<PathBuf>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub last_seen: SystemTime,
    pub sessions: HashMap<String, SessionInfo>,
}

pub struct NodeRegistry {
    nodes: RwLock<HashMap<String, NodeRecord>>,
    timeout: Duration,
}

impl NodeRegistry {
    pub fn new(timeout: Duration) -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            timeout,
        }
    }

    /// Replaces the control plane's whole view of a node with what the node
    /// itself reported.
    pub fn upsert(&self, node_name: &str, sessions: &[SessionInfo], now: SystemTime) {
        let record = NodeRecord {
            last_seen: now,
            sessions: sessions
                .iter()
                .map(|session| (session.id.clone(), session.clone()))
                .collect(),
        };
        self.nodes
            .write()
            .unwrap()
            .insert(node_name.to_string(), record);
    }

    pub fn add_session(&self, node: &str, session: SessionInfo) {
        if let Some(record) = self.nodes.write().unwrap().get_mut(node) {
            record.sessions.insert(session.id.clone(), session);
        }
    }

    pub fn list(&self, now: SystemTime) -> Vec<NodeHealth> {
        let mut records: Vec<(String, NodeRecord)> = self
            .nodes
            .read()
            .unwrap()
            .iter()
            .map(|(name, record)| (name.clone(), record.clone()))
            .collect();
        records.sort_by(|a, b| a.0.cmp(&b.0));

        records
            .into_iter()
            .map(|(name, record)| NodeHealth {
                name,
                up: is_up(record.last_seen, now, self.timeout),
                last_seen_secs: record
                    .last_seen
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            })
            .collect()
    }

    pub fn session(&self, id: &str, now: SystemTime) -> Option<(String, SessionInfo)> {
        let nodes = self.nodes.read().unwrap();
        for (name, record) in nodes.iter() {
            if !is_up(record.last_seen, now, self.timeout) {
                continue;
            }
            if let Some(session) = record.sessions.get(id) {
                return Some((name.clone(), session.clone()));
            }
        }
        None
    }

    pub fn node(&self, name: &str, now: SystemTime) -> Option<NodeHealth> {
        let nodes = self.nodes.read().unwrap();
        let record = nodes.get(name)?;
        if !is_up(record.last_seen, now, self.timeout) {
            return None;
        }
        Some(NodeHealth {
            name: name.to_string(),
            up: true,
            last_seen_secs: record
                .last_seen
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
    }

    pub fn remove_session(&self, node: &str, session_id: &str) {
        if let Some(record) = self.nodes.write().unwrap().get_mut(node) {
            record.sessions.remove(session_id);
        }
    }

    pub fn sessions(&self, now: SystemTime) -> Vec<SessionHealth> {
        let mut sessions: Vec<SessionHealth> = self
            .nodes
            .read()
            .unwrap()
            .iter()
            .filter(|(_, record)| is_up(record.last_seen, now, self.timeout))
            .flat_map(|(name, record)| {
                record.sessions.values().map(|session| SessionHealth {
                    id: session.id.clone(),
                    node: name.clone(),
                    repo_url: session.repo_url.clone(),
                    git_ref: session.git_ref.clone(),
                    dir: session.dir.clone(),
                    status: session.status.clone(),
                })
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }
}

fn is_up(last_seen: SystemTime, now: SystemTime, timeout: Duration) -> bool {
    now.duration_since(last_seen)
        .map(|since| since <= timeout)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn epoch(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn session(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            repo_url: Some("https://example.com/repo".into()),
            git_ref: None,
            dir: None,
            status: "ready".into(),
        }
    }

    #[test]
    fn empty_registry_lists_nothing() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        assert!(registry.list(epoch(100)).is_empty());
    }

    #[test]
    fn recent_upsert_is_up() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[], epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health.len(), 1);
        assert!(health[0].up);
        assert_eq!(health[0].name, "node-1");
        assert_eq!(health[0].last_seen_secs, 100);
    }

    #[test]
    fn stale_node_is_down() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[], epoch(100));
        let health = registry.list(epoch(200));
        assert!(!health[0].up);
    }

    #[test]
    fn upsert_updates_last_seen_instead_of_duplicating() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[], epoch(100));
        registry.upsert("node-1", &[], epoch(110));
        let health = registry.list(epoch(120));
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].last_seen_secs, 110);
    }

    #[test]
    fn list_is_sorted_by_name() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-b", &[], epoch(100));
        registry.upsert("node-a", &[], epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health[0].name, "node-a");
        assert_eq!(health[1].name, "node-b");
    }

    #[test]
    fn upsert_replaces_whole_record() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[session("s1")], epoch(100));
        registry.upsert("node-1", &[], epoch(110));

        let health = registry.list(epoch(120));
        assert_eq!(health.len(), 1);
        assert!(registry.sessions(epoch(120)).is_empty());
    }

    #[test]
    fn sessions_flatten_across_nodes_sorted_by_id() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-b", &[session("s2"), session("s3")], epoch(100));
        registry.upsert("node-a", &[session("s1")], epoch(100));

        let sessions = registry.sessions(epoch(110));
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "s2", "s3"]);
        assert_eq!(sessions[0].node, "node-a");
        assert_eq!(sessions[2].node, "node-b");
    }

    #[test]
    fn down_node_sessions_are_not_listed() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[session("s1")], epoch(100));

        assert!(registry.sessions(epoch(200)).is_empty());
        assert!(registry.session("s1", epoch(200)).is_none());

        registry.upsert("node-1", &[session("s1")], epoch(199));
        let (node, found) = registry
            .session("s1", epoch(200))
            .expect("session should be found");
        assert_eq!(node, "node-1");
        assert_eq!(found.id, "s1");
    }

    #[test]
    fn remove_session_drops_one_of_two() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[session("s1"), session("s2")], epoch(100));

        registry.remove_session("node-1", "s1");

        let sessions = registry.sessions(epoch(110));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s2");
    }

    #[test]
    fn remove_session_for_unknown_node_is_a_noop() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", &[session("s1")], epoch(100));

        registry.remove_session("node-unknown", "s1");
        registry.remove_session("node-1", "s-unknown");

        assert_eq!(registry.sessions(epoch(110)).len(), 1);
    }

    #[test]
    fn node_is_none_for_unknown_or_down_nodes() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        assert!(registry.node("node-1", epoch(100)).is_none());

        registry.upsert("node-1", &[], epoch(100));

        assert!(registry.node("node-1", epoch(200)).is_none());
        let node = registry
            .node("node-1", epoch(110))
            .expect("node should be up");
        assert_eq!(node.name, "node-1");
        assert!(node.up);
    }
}
