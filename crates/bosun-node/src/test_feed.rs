//! An in-process stub of the GitHub-Releases layout the node fetches update
//! archives from, for the update-flow tests. Serves exactly one release: the
//! current target's archive and its `.sha256` for one version. Anything else
//! is a 404, so a request for a wrong path, asset, or version fails loudly.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;

use axum::Router;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use sha2::Digest;
use sha2::Sha256;

/// How the stub's archive download behaves.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ArchiveBehavior {
    /// Serves a matching checksum, then holds the archive request open, so an
    /// update stays in flight.
    Hang,
    /// Serves a checksum that does not match the archive bytes, so an update
    /// fails verification.
    Mismatch,
}

/// The release-feed stub and how many requests it has served.
pub(crate) struct FeedStub {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
}

impl FeedStub {
    pub(crate) fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub(crate) fn requests(&self) -> usize {
        self.requests.load(AtomicOrdering::Relaxed)
    }
}

/// Serves the current target's release assets for `version`: the archive the
/// node requests and its per-asset checksum file, in cargo-dist's layout.
pub(crate) async fn serve(version: &str, behavior: ArchiveBehavior) -> FeedStub {
    let content: Arc<Vec<u8>> = Arc::new(b"fake binary".to_vec());
    let checksum_file = match behavior {
        ArchiveBehavior::Hang => format!("{:x} *{}\n", Sha256::digest(&*content), archive_name()),
        ArchiveBehavior::Mismatch => format!("{} *{}\n", "0".repeat(64), archive_name()),
    };
    let requests = Arc::new(AtomicUsize::new(0));

    #[derive(Clone)]
    struct ServerState {
        checksum_path: String,
        archive_path: String,
        checksum_file: String,
        content: Arc<Vec<u8>>,
        behavior: ArchiveBehavior,
        requests: Arc<AtomicUsize>,
    }

    async fn serve_feed(State(state): State<ServerState>, Path(path): Path<String>) -> Response {
        state.requests.fetch_add(1, AtomicOrdering::Relaxed);
        if path == state.checksum_path {
            return state.checksum_file.into_response();
        }
        if path == state.archive_path {
            if matches!(state.behavior, ArchiveBehavior::Hang) {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            return (*state.content).clone().into_response();
        }
        StatusCode::NOT_FOUND.into_response()
    }

    let archive_path = format!("v{version}/{}", archive_name());
    let app = Router::new()
        .route("/{*path}", get(serve_feed))
        .with_state(ServerState {
            checksum_path: format!("{archive_path}.sha256"),
            archive_path,
            checksum_file,
            content,
            behavior,
            requests: requests.clone(),
        });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FeedStub { addr, requests }
}

/// cargo-dist ships every Windows target as a `.zip` and every other target
/// as a `.tar.xz`, mirroring the release-fetch layout in bosun-common.
fn archive_name() -> String {
    let target = bosun_common::target::TARGET;
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.xz"
    };
    format!("bosun-{target}.{extension}")
}
