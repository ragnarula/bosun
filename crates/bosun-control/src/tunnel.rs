use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use bosun_common::tunnel::LogicalStream;
use bosun_common::tunnel::Tunnel;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("node {node} has no tunnel")]
    NoTunnel { node: String },

    #[error("node {node} tunnel is closed")]
    TunnelClosed { node: String },
}

/// Maps each node to its one outbound tunnel. A tool call opens a logical
/// connection on the tunnel of the session's node, addressed with the session
/// id so the node's relay can dispatch it to that session's in-process
/// executor.
#[derive(Clone)]
pub struct TunnelRegistry {
    tunnels: Arc<RwLock<HashMap<String, Tunnel>>>,
}

impl TunnelRegistry {
    pub fn new() -> Self {
        Self {
            tunnels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Records a node's tunnel. Replaces any previous tunnel.
    pub fn register(&self, node: &str, tunnel: Tunnel) {
        self.tunnels
            .write()
            .unwrap()
            .insert(node.to_string(), tunnel);
    }

    /// Removes the registration only when `tunnel` is still the registered
    /// one, so a stale close can never drop a newer tunnel for the same node.
    pub fn unregister_if_current(&self, node: &str, tunnel: &Tunnel) {
        let mut tunnels = self.tunnels.write().unwrap();
        if tunnels
            .get(node)
            .is_some_and(|current| current.same_as(tunnel))
        {
            tunnels.remove(node);
        }
    }

    /// Opens a logical connection for one session on its node's tunnel. The
    /// connection names the session, so the node relay dispatches it to the
    /// session's in-process executor.
    pub async fn open(&self, node: &str, session_id: &str) -> Result<LogicalStream, TunnelError> {
        let tunnel = self
            .tunnels
            .read()
            .unwrap()
            .get(node)
            .cloned()
            .ok_or_else(|| TunnelError::NoTunnel {
                node: node.to_string(),
            })?;
        tunnel
            .open(session_id)
            .await
            .ok_or_else(|| TunnelError::TunnelClosed {
                node: node.to_string(),
            })
    }
}

impl Default for TunnelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    use super::*;

    #[tokio::test]
    async fn open_returns_a_stream_once_a_node_tunnel_is_registered() {
        let (a, _b) = duplex(1024);
        let (tunnel, _opens) = Tunnel::new(a);

        let registry = TunnelRegistry::new();
        registry.register("node-1", tunnel);
        let mut stream = registry
            .open("node-1", "s1")
            .await
            .expect("a tunnel is registered");
        stream.write_all(b"x").await.unwrap();
    }

    #[tokio::test]
    async fn one_node_tunnel_serves_every_session_on_the_node() {
        let (a, _b) = duplex(1024);
        let (tunnel, _opens) = Tunnel::new(a);

        let registry = TunnelRegistry::new();
        registry.register("node-1", tunnel);

        // A second session on the same node opens on the same tunnel.
        let mut s2 = registry
            .open("node-1", "s2")
            .await
            .expect("s2 has a tunnel");
        s2.write_all(b"y").await.unwrap();

        // A session on an unregistered node fails with the node's identity.
        assert!(matches!(
            registry.open("ghost-node", "s3").await,
            Err(TunnelError::NoTunnel { node }) if node == "ghost-node"
        ));
    }

    #[tokio::test]
    async fn open_fails_without_a_registered_tunnel() {
        let registry = TunnelRegistry::new();
        assert!(matches!(
            registry.open("node-1", "ghost").await,
            Err(TunnelError::NoTunnel { .. })
        ));
    }

    #[tokio::test]
    async fn unregister_if_current_removes_only_a_matching_tunnel() {
        let (a, _b) = duplex(1024);
        let (tunnel, _opens) = Tunnel::new(a);
        let (c, _d) = duplex(1024);
        let (other, _opens) = Tunnel::new(c);

        let registry = TunnelRegistry::new();
        registry.register("node-1", tunnel.clone());
        registry.register("node-2", other.clone());
        // A stale close of an older tunnel must not remove the newer tunnel
        // that replaced it.
        registry.register("node-1", other.clone());
        registry.unregister_if_current("node-1", &tunnel);
        let mut stream = registry
            .open("node-1", "s1")
            .await
            .expect("the newer tunnel survives a stale close");
        stream.write_all(b"z").await.unwrap();

        registry.unregister_if_current("node-1", &other);
        assert!(matches!(
            registry.open("node-1", "s1").await,
            Err(TunnelError::NoTunnel { .. })
        ));
        // A tunnel that never was registered for the node changes nothing.
        let (e, _f) = duplex(1024);
        let (unrelated, _opens) = Tunnel::new(e);
        registry.unregister_if_current("node-2", &unrelated);
        let mut stream = registry
            .open("node-2", "s1")
            .await
            .expect("node-2 keeps its tunnel");
        stream.write_all(b"q").await.unwrap();
    }
}
