use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use bosun_common::tunnel::LogicalStream;
use bosun_common::tunnel::Tunnel;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("session {session_id} has no tunnel")]
    NoTunnel { session_id: String },

    #[error("session {session_id} tunnel is closed")]
    TunnelClosed { session_id: String },
}

/// Maps each session to the node's outbound tunnel. The gateway opens a
/// logical connection on it for every host-routed client request.
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

    /// Records a node's tunnel for a session. Replaces any previous tunnel.
    pub fn register(&self, session_id: &str, tunnel: Tunnel) {
        self.tunnels
            .write()
            .unwrap()
            .insert(session_id.to_string(), tunnel);
    }

    pub fn unregister(&self, session_id: &str) {
        self.tunnels.write().unwrap().remove(session_id);
    }

    /// Opens a logical connection on the session's tunnel for one client
    /// connection.
    pub async fn open(&self, session_id: &str) -> Result<LogicalStream, TunnelError> {
        let tunnel = self
            .tunnels
            .read()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or_else(|| TunnelError::NoTunnel {
                session_id: session_id.to_string(),
            })?;
        tunnel
            .open()
            .await
            .ok_or_else(|| TunnelError::TunnelClosed {
                session_id: session_id.to_string(),
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
    async fn open_returns_a_stream_once_a_tunnel_is_registered() {
        let (a, _b) = duplex(1024);
        let (tunnel, _opens) = Tunnel::new(a);

        let registry = TunnelRegistry::new();
        registry.register("s1", tunnel);
        let mut stream = registry.open("s1").await.expect("a tunnel is registered");
        stream.write_all(b"x").await.unwrap();
    }

    #[tokio::test]
    async fn open_fails_without_a_registered_tunnel() {
        let registry = TunnelRegistry::new();
        assert!(matches!(
            registry.open("ghost").await,
            Err(TunnelError::NoTunnel { .. })
        ));
    }
}
