use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::RwLock;

use anyhow::Context;
use tracing::info;

pub struct ProxyManager {
    bind_addr: String,
    proxies: RwLock<HashMap<String, ProxyRecord>>,
}

struct ProxyRecord {
    port: u16,
    handle: tokio::task::AbortHandle,
}

impl ProxyManager {
    pub fn new(bind_addr: String) -> Self {
        Self {
            bind_addr,
            proxies: RwLock::new(HashMap::new()),
        }
    }

    pub async fn ensure(
        &self,
        session_id: &str,
        forwarder_addr: &str,
    ) -> Result<u16, anyhow::Error> {
        if let Some(record) = self.proxies.read().unwrap().get(session_id) {
            return Ok(record.port);
        }
        let target: SocketAddr = forwarder_addr
            .parse()
            .with_context(|| format!("failed to parse forwarder address {forwarder_addr}"))?;
        let (addr, handle) =
            bosun_common::forward::start(&format!("{}:0", self.bind_addr), target).await?;
        let port = addr.port();
        self.proxies
            .write()
            .unwrap()
            .insert(session_id.to_string(), ProxyRecord { port, handle });
        info!(session_id = %session_id, port = port, "proxy started");
        Ok(port)
    }

    pub fn port(&self, session_id: &str) -> Option<u16> {
        self.proxies
            .read()
            .unwrap()
            .get(session_id)
            .map(|record| record.port)
    }

    pub fn remove(&self, session_id: &str) {
        if let Some(record) = self.proxies.write().unwrap().remove(session_id) {
            record.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let manager = ProxyManager::new("127.0.0.1".into());
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        let echo_task =
            tokio::spawn(async move { while let Ok((_stream, _)) = echo.accept().await {} });

        let first = manager.ensure("s1", &echo_addr.to_string()).await.unwrap();
        let second = manager.ensure("s1", &echo_addr.to_string()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(manager.port("s1"), Some(first));

        echo_task.abort();
    }

    #[tokio::test]
    async fn proxy_forwards_bytes_to_the_forwarder() {
        let manager = ProxyManager::new("127.0.0.1".into());
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            stream.write_all(&buf[..n]).await.unwrap();
        });

        let port = manager.ensure("s1", &echo_addr.to_string()).await.unwrap();
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        echo_task.await.unwrap();
    }

    #[tokio::test]
    async fn remove_closes_the_proxy_port() {
        let manager = ProxyManager::new("127.0.0.1".into());
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            let _ = echo.accept().await;
        });

        let port = manager.ensure("s1", &echo_addr.to_string()).await.unwrap();
        manager.remove("s1");
        assert!(manager.port("s1").is_none());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let connect = tokio::net::TcpStream::connect(("127.0.0.1", port)).await;
            if connect.is_err() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("proxy port still accepts connections after remove");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        echo_task.abort();
    }

    #[tokio::test]
    async fn ensure_rejects_an_invalid_forwarder_address() {
        let manager = ProxyManager::new("127.0.0.1".into());
        let err = manager
            .ensure("s1", "not-an-address")
            .await
            .expect_err("invalid address should fail");
        assert!(err.chain().any(|e| e.is::<std::net::AddrParseError>()));
    }
}
