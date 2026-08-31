//! Test helpers shared across the workspace crate test suites.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::net::TcpListener;

/// Polls `condition` every 10ms until it returns true, failing the test after
/// 5 seconds.
pub async fn wait_for<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if condition().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Serves one `ok` response per connection until the listener is dropped.
pub async fn stub_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                use tokio::io::AsyncWriteExt;
                let mut buf = [0u8; 4096];
                let mut read = 0;
                while let Ok(n) = stream.read(&mut buf[read..]).await {
                    if n == 0 {
                        break;
                    }
                    read += n;
                    if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
            });
        }
    });
    addr
}

/// Runs `git` in `dir`, asserting the command succeeds.
pub fn git_quiet(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("failed to run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Creates a git repository in `dir` with a fixed test identity.
pub fn init_repo(dir: &Path) {
    git_quiet(dir, &["init", "-q"]);
    git_quiet(dir, &["config", "user.name", "test"]);
    git_quiet(dir, &["config", "user.email", "test@example.com"]);
}
