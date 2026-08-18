use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::RwLock;

use anyhow::Context;
use bosun_common::forward::accept_loop;
use tokio::net::TcpListener;
use tracing::info;

pub struct ProxyManager {
    bind_addr: String,
    proxies: RwLock<HashMap<String, ProxyRecord>>,
}

struct ProxyRecord {
    port: u16,
    listener: std::net::TcpListener,
    handle: tokio::task::AbortHandle,
    target: String,
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
        {
            let proxies = self.proxies.read().unwrap();
            if let Some(record) = proxies.get(session_id)
                && record.target == forwarder_addr
            {
                return Ok(record.port);
            }
        }

        let target: SocketAddr = forwarder_addr
            .parse()
            .with_context(|| format!("failed to parse forwarder address {forwarder_addr}"))?;

        let mut proxies = self.proxies.write().unwrap();
        if let Some(record) = proxies.get_mut(session_id) {
            record.handle.abort();
            let accept = record
                .listener
                .try_clone()
                .context("failed to clone proxy listener")?;
            record.handle =
                tokio::spawn(accept_loop(to_tokio_listener(accept)?, target)).abort_handle();
            record.target = forwarder_addr.to_string();
            info!(
                session_id = %session_id,
                port = record.port,
                "proxy re-pointed to the new forwarder"
            );
            return Ok(record.port);
        }
        drop(proxies);

        let listener = std::net::TcpListener::bind(format!("{}:0", self.bind_addr))
            .with_context(|| format!("failed to bind proxy on {}", self.bind_addr))?;
        let port = listener
            .local_addr()
            .context("failed to read the bound proxy port")?
            .port();
        let accept = listener
            .try_clone()
            .context("failed to clone proxy listener")?;
        let handle = tokio::spawn(accept_loop(to_tokio_listener(accept)?, target)).abort_handle();
        self.proxies.write().unwrap().insert(
            session_id.to_string(),
            ProxyRecord {
                port,
                listener,
                handle,
                target: forwarder_addr.to_string(),
            },
        );
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

fn to_tokio_listener(listener: std::net::TcpListener) -> Result<TcpListener, anyhow::Error> {
    listener
        .set_nonblocking(true)
        .context("failed to set proxy listener nonblocking")?;
    TcpListener::from_std(listener).context("failed to move proxy listener to tokio")
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

    #[tokio::test]
    async fn ensure_repoints_an_existing_proxy_to_a_new_target_on_the_same_port() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ProxyManager::new("127.0.0.1".into());
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = listener_b.local_addr().unwrap();
        let echo_a = tokio::spawn(async move {
            let (mut stream, _) = listener_a.accept().await.unwrap();
            stream.write_all(b"A").await.unwrap();
        });
        let echo_b = tokio::spawn(async move {
            let (mut stream, _) = listener_b.accept().await.unwrap();
            stream.write_all(b"B").await.unwrap();
        });

        let port = manager.ensure("s1", &addr_a.to_string()).await.unwrap();

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut reply = [0u8; 1];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"A");

        let repointed = manager.ensure("s1", &addr_b.to_string()).await.unwrap();
        assert_eq!(repointed, port);

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut reply = [0u8; 1];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"B");

        echo_a.await.unwrap();
        echo_b.await.unwrap();
    }
}
