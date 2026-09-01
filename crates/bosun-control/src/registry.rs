use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use std::time::SystemTime;

use bosun_common::types::UpdateStatus;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealth {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub update_status: UpdateStatus,
    pub up: bool,
    pub last_seen_secs: u64,
}

#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub last_seen: SystemTime,
    pub version: String,
    pub update_status: UpdateStatus,
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

    /// Records a node's heartbeat. The registry holds liveness, the reported
    /// version, and the reported update status only; sessions live in the
    /// control-plane store.
    pub fn upsert(
        &self,
        node_name: &str,
        version: &str,
        update_status: UpdateStatus,
        now: SystemTime,
    ) {
        let record = NodeRecord {
            last_seen: now,
            version: version.to_string(),
            update_status,
        };
        self.nodes
            .write()
            .unwrap()
            .insert(node_name.to_string(), record);
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
                version: record.version,
                update_status: record.update_status,
                up: is_up(record.last_seen, now, self.timeout),
                last_seen_secs: bosun_common::time::unix_secs(record.last_seen) as u64,
            })
            .collect()
    }

    pub fn node(&self, name: &str, now: SystemTime) -> Option<NodeHealth> {
        let nodes = self.nodes.read().unwrap();
        let record = nodes.get(name)?;
        if !is_up(record.last_seen, now, self.timeout) {
            return None;
        }
        Some(NodeHealth {
            name: name.to_string(),
            version: record.version.clone(),
            update_status: record.update_status.clone(),
            up: true,
            last_seen_secs: bosun_common::time::unix_secs(record.last_seen) as u64,
        })
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
    use std::time::UNIX_EPOCH;

    use super::*;

    fn epoch(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn empty_registry_lists_nothing() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        assert!(registry.list(epoch(100)).is_empty());
    }

    #[test]
    fn recent_upsert_is_up() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", "0.5.5", UpdateStatus::default(), epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health.len(), 1);
        assert!(health[0].up);
        assert_eq!(health[0].name, "node-1");
        assert_eq!(health[0].last_seen_secs, 100);
    }

    #[test]
    fn stale_node_is_down() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", "0.5.5", UpdateStatus::default(), epoch(100));
        let health = registry.list(epoch(200));
        assert!(!health[0].up);
    }

    #[test]
    fn upsert_updates_last_seen_instead_of_duplicating() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", "0.5.5", UpdateStatus::default(), epoch(100));
        registry.upsert("node-1", "0.6.0", UpdateStatus::default(), epoch(110));
        let health = registry.list(epoch(120));
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].last_seen_secs, 110);
        assert_eq!(health[0].version, "0.6.0");
    }

    #[test]
    fn upsert_stores_the_reported_version() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", "0.9.0", UpdateStatus::default(), epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health[0].version, "0.9.0");
    }

    #[test]
    fn upsert_stores_the_reported_update_status() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert(
            "node-1",
            "0.5.5",
            UpdateStatus::Failed("checksum mismatch".into()),
            epoch(100),
        );
        let health = registry.list(epoch(110));
        assert_eq!(
            health[0].update_status,
            UpdateStatus::Failed("checksum mismatch".into())
        );
    }

    #[test]
    fn list_is_sorted_by_name() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-b", "0.5.5", UpdateStatus::default(), epoch(100));
        registry.upsert("node-a", "0.5.5", UpdateStatus::default(), epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health[0].name, "node-a");
        assert_eq!(health[1].name, "node-b");
    }

    #[test]
    fn node_is_none_for_unknown_or_down_nodes() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        assert!(registry.node("node-1", epoch(100)).is_none());

        registry.upsert("node-1", "0.5.5", UpdateStatus::default(), epoch(100));

        assert!(registry.node("node-1", epoch(200)).is_none());
        let node = registry
            .node("node-1", epoch(110))
            .expect("node should be up");
        assert_eq!(node.name, "node-1");
        assert_eq!(node.version, "0.5.5");
        assert_eq!(node.update_status, UpdateStatus::UpToDate);
        assert!(node.up);
    }

    #[test]
    fn node_health_deserializes_without_version() {
        let health: NodeHealth =
            serde_json::from_str(r#"{"name": "node-1", "up": true, "last_seen_secs": 100}"#)
                .expect("a node from an old control plane has no version");
        assert_eq!(health.name, "node-1");
        assert_eq!(health.version, "");
        assert!(health.up);
        assert_eq!(health.last_seen_secs, 100);
    }

    #[test]
    fn node_health_deserializes_without_update_status() {
        let health: NodeHealth =
            serde_json::from_str(r#"{"name": "node-1", "up": true, "last_seen_secs": 100}"#)
                .expect("a node from an old control plane has no update status");
        assert_eq!(health.update_status, UpdateStatus::UpToDate);
    }
}
