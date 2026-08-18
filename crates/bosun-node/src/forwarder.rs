use std::net::SocketAddr;

use anyhow::Context;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tracing::debug;
use tracing::instrument;

pub async fn accept_loop(listener: TcpListener, target: SocketAddr) {
    loop {
        let (inbound, _peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                debug!(error = %error, "forwarder accept failed; stopping");
                break;
            }
        };
        tokio::spawn(forward_connection(inbound, target));
    }
}

#[instrument(skip(inbound), fields(target = %target))]
async fn forward_connection(mut inbound: TcpStream, target: SocketAddr) {
    let result = async {
        let mut outbound = TcpStream::connect(target)
            .await
            .context("failed to connect to target")?;
        copy_bidirectional(&mut inbound, &mut outbound)
            .await
            .context("failed to forward bytes between connections")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = result {
        debug!(error = %error, "forwarder connection failed");
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn accept_loop_forwards_bytes_both_ways() {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            stream.write_all(&buf[..n]).await.unwrap();
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let forwarder_addr = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(accept_loop(listener, echo_addr));

        let mut client = tokio::net::TcpStream::connect(forwarder_addr)
            .await
            .unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        echo_task.await.unwrap();
        accept_task.abort();
    }
}
