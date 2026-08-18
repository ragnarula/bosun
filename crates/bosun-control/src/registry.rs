use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealth {
    pub name: String,
    pub up: bool,
    pub last_seen_secs: u64,
}

pub struct NodeRegistry {
    nodes: RwLock<HashMap<String, SystemTime>>,
    timeout: Duration,
}

impl NodeRegistry {
    pub fn new(timeout: Duration) -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            timeout,
        }
    }

    pub fn upsert(&self, name: &str, now: SystemTime) {
        self.nodes.write().unwrap().insert(name.to_string(), now);
    }

    pub fn list(&self, now: SystemTime) -> Vec<NodeHealth> {
        let mut records: Vec<(String, SystemTime)> = self
            .nodes
            .read()
            .unwrap()
            .iter()
            .map(|(name, last_seen)| (name.clone(), *last_seen))
            .collect();
        records.sort_by(|a, b| a.0.cmp(&b.0));

        records
            .into_iter()
            .map(|(name, last_seen)| {
                let since = now.duration_since(last_seen).unwrap_or(Duration::ZERO);
                NodeHealth {
                    name,
                    up: since <= self.timeout,
                    last_seen_secs: last_seen
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
    fn recent_heartbeat_is_up() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health.len(), 1);
        assert!(health[0].up);
        assert_eq!(health[0].name, "node-1");
        assert_eq!(health[0].last_seen_secs, 100);
    }

    #[test]
    fn stale_node_is_down() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", epoch(100));
        let health = registry.list(epoch(200));
        assert!(!health[0].up);
    }

    #[test]
    fn upsert_updates_last_seen_instead_of_duplicating() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-1", epoch(100));
        registry.upsert("node-1", epoch(110));
        let health = registry.list(epoch(120));
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].last_seen_secs, 110);
    }

    #[test]
    fn list_is_sorted_by_name() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert("node-b", epoch(100));
        registry.upsert("node-a", epoch(100));
        let health = registry.list(epoch(110));
        assert_eq!(health[0].name, "node-a");
        assert_eq!(health[1].name, "node-b");
    }
}
