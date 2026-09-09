//! One node tunnel carries tool calls for every session on the node. Two
//! sessions sharing a node reach their own executors concurrently over the
//! tunnel, and a large tool response over one logical connection does not
//! stall the other (the original flow-control regression: concurrent streams
//! over one tunnel stalled after roughly 800 KiB of a large response).
//! Mirrors the reported deployment: a control plane, a node that dials out
//! over one tunnel and hosts in-process executors, and tool calls over
//! logical connections on the same tunnel at the same time.
//!
//! A large tool response now never travels as one big frame: the node spills
//! it to a file in the session's working copy and answers with a short
//! `Spilled` frame instead. The concurrency guarantee these tests guard is
//! therefore that concurrent calls all *complete* promptly — each oversized
//! read ends in a `Spilled` reply naming a file whose content is the full
//! body, and the sibling small reads return `Result` bodies — rather than
//! that a multi-hundred-KiB body streams across the tunnel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bosun_common::session::Permission;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::tool::ToolMsg;
use bosun_common::tool::ToolOp;
use bosun_common::tool::read_tool_frame;
use bosun_common::tool::write_tool_frame;
use bosun_common::tunnel::LogicalStream;
use bosun_common::types::NodeStartRequest;
use bosun_control::api::AppState;
use bosun_control::api::router;
use bosun_control::commands::CommandQueue;
use bosun_control::loops::AgentRegistry;
use bosun_control::registry::NodeRegistry;
use bosun_control::skills_repos::GitHubClient;
use bosun_control::tunnel::TunnelRegistry;
use bosun_node::manager::NodeManager;
use bosun_store::store::Store;
use serde_json::json;

/// The oversized asset: a file this large cannot come back inline in one
/// result frame, so reading it ends in a `Spilled` reply. The file_read cap
/// (1 MiB) bounds the file; the spill cap bounds what travels as frames.
const ASSET_LEN: usize = 1_000_000;
const INDEX_A: &str = "index page from executor A";
const INDEX_B: &str = "index page from executor B";

/// A control plane with a node tunnel registry. Returns the store too, so a
/// test can register the tunnel's sessions up front, plus the temp dir the
/// store lives in: the test keeps it bound so the database file is not
/// removed under the store.
async fn control_plane() -> (SocketAddr, Store, Arc<TunnelRegistry>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("sessions.db")).unwrap();
    let tunnels = Arc::new(TunnelRegistry::new());
    let state = Arc::new(AppState {
        registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
        commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
        tunnels: tunnels.clone(),
        store: store.clone(),
        github: GitHubClient::new("http://127.0.0.1:1", "http://127.0.0.1:1", None),
        loops: Arc::new(AgentRegistry::new(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )),
        providers: HashMap::new(),
        personas: HashMap::new(),
        default_persona: None,
    });
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, store, tunnels, dir)
}

/// Registers a session on `node-1` in the store, as a clone/dev request
/// would.
async fn register_session(store: &Store, session_id: &str) {
    store
        .create_session(&Session {
            id: session_id.to_string(),
            node: "node-1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/work".into(),
            model: "mock-model".into(),
            persona: None,
            parent_id: None,
            owner_id: session_id.to_string(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            state: SessionState::WaitingForInput,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: None,
        })
        .await
        .unwrap();
}

/// A real node hosting the sessions' in-process executors: one working copy
/// per session under `work`, each with a small index file and a large asset
/// file, then the node's one outbound tunnel to the control plane.
async fn node_with_sessions(
    work: &Path,
    cp_addr: SocketAddr,
    sessions: &[(&str, &str)],
) -> Arc<NodeManager> {
    let manager = Arc::new(NodeManager::new(
        work.to_path_buf(),
        vec![work.to_path_buf()],
        format!("http://{cp_addr}"),
        None,
    ));
    for (session_id, index_body) in sessions {
        let dir = work.join(session_id);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("index.txt"), index_body)
            .await
            .unwrap();
        tokio::fs::write(dir.join("asset.txt"), vec![b'x'; ASSET_LEN])
            .await
            .unwrap();
        manager
            .start(&NodeStartRequest {
                session_id: (*session_id).to_string(),
                dir,
                permission: Permission::ReadWrite,
            })
            .await
            .expect("the session should start on the node");
    }
    manager.start_node_tunnel("node-1");
    manager
}

/// Opens a logical connection on the node's tunnel for one session and runs
/// one typed `file_read` call, returning the terminal reply. Returns `None`
/// while the tunnel is not yet registered.
async fn tunnel_read(tunnels: &TunnelRegistry, session_id: &str, path: &str) -> Option<ToolMsg> {
    let mut conn: LogicalStream = tunnels.open("node-1", session_id).await.ok()?;
    write_tool_frame(
        &mut conn,
        &ToolOp::Call {
            run_id: "run-1".into(),
            tool: "file_read".into(),
            args: json!({ "path": path }),
        },
    )
    .await
    .ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message =
            match tokio::time::timeout_at(deadline, read_tool_frame::<_, ToolMsg>(&mut conn))
                .await
                .ok()?
            {
                Ok(Some(message)) => message,
                _ => return None,
            };
        match message {
            // `Result` is a small inline body, `Spilled` is a large one the
            // node wrote to a file; both end the reply.
            ToolMsg::Result { .. } | ToolMsg::Error { .. } | ToolMsg::Spilled { .. } => {
                return Some(message);
            }
            ToolMsg::Done { .. } | ToolMsg::Ack | ToolMsg::Event { .. } => {}
        }
    }
}

fn content_len(reply: &ToolMsg) -> usize {
    let ToolMsg::Result { content } = reply else {
        panic!("file_read must return a result: {reply:?}");
    };
    content["content"].as_str().unwrap().len()
}

fn content(reply: &ToolMsg) -> String {
    let ToolMsg::Result { content } = reply else {
        panic!("file_read must return a result: {reply:?}");
    };
    content["content"].as_str().unwrap().to_string()
}

/// Waits until the tunnel is live, then reads the index and the asset
/// concurrently over two logical connections on one tunnel.
#[tokio::test]
async fn concurrent_streams_over_one_node_tunnel_deliver_both_bodies() {
    let (cp_addr, store, tunnels, _state_dir) = control_plane().await;
    let work = tempfile::tempdir().unwrap();
    let session_id = uuid::Uuid::new_v4().to_string();
    register_session(&store, &session_id).await;
    node_with_sessions(work.path(), cp_addr, &[(&session_id, INDEX_A)]).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel_read(&tunnels, &session_id, "index.txt").await {
            Some(reply) if !matches!(reply, ToolMsg::Error { .. }) => break,
            _ => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "node route never became live"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    let index = tunnel_read(&tunnels, &session_id, "index.txt");
    let asset = tunnel_read(&tunnels, &session_id, "asset.txt");

    let (index, asset) = tokio::join!(index, asset);
    let index = index.expect("index read failed");
    let asset = asset.expect("asset read failed");
    assert_eq!(content(&index), INDEX_A, "index body mismatch");

    // The oversized asset is spilled: the reply names a file in the session's
    // working copy instead of carrying a megabyte in the frame. The small
    // read completed alongside it, which is what the concurrency regression
    // guards.
    let asset_len = tokio::time::timeout(Duration::from_secs(10), async {
        match &asset {
            ToolMsg::Spilled { file, .. } => {
                let raw = tokio::fs::read_to_string(work.path().join(&session_id).join(file))
                    .await
                    .expect("the spilled file exists under the session's working copy");
                // A spilled result file holds the result's serialized JSON,
                // exactly the value the model would otherwise have seen
                // inline; a `file/read` result is {"content": "<body>"}.
                let parsed: serde_json::Value =
                    serde_json::from_str(&raw).expect("the spilled file holds JSON");
                parsed["content"].as_str().unwrap().len()
            }
            _ => panic!("an oversized asset must spill: {asset:?}"),
        }
    })
    .await
    .expect("asset read stalled");
    assert_eq!(asset_len, ASSET_LEN, "the spilled file holds the full asset");
}

/// Two sessions share the node's one tunnel: each session's call reaches its
/// own working copy, concurrently.
#[tokio::test]
async fn two_sessions_on_one_node_reach_their_own_executors_concurrently() {
    let (cp_addr, store, tunnels, _state_dir) = control_plane().await;
    let work = tempfile::tempdir().unwrap();
    let session_a = uuid::Uuid::new_v4().to_string();
    let session_b = uuid::Uuid::new_v4().to_string();
    register_session(&store, &session_a).await;
    register_session(&store, &session_b).await;
    node_with_sessions(
        work.path(),
        cp_addr,
        &[(&session_a, INDEX_A), (&session_b, INDEX_B)],
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel_read(&tunnels, &session_a, "index.txt").await {
            Some(reply) if !matches!(reply, ToolMsg::Error { .. }) => break,
            _ => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "node route never became live"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    let from_a = tunnel_read(&tunnels, &session_a, "asset.txt");
    let from_b = tunnel_read(&tunnels, &session_b, "asset.txt");

    let (from_a, from_b) = tokio::join!(from_a, from_b);
    let from_a = from_a.expect("session A's read failed");
    let from_b = from_b.expect("session B's read failed");
    for (reply, session_id) in [(from_a, &session_a), (from_b, &session_b)] {
        let ToolMsg::Spilled { file, .. } = &reply else {
            panic!("an oversized asset must spill: {reply:?}");
        };
        let raw = tokio::fs::read_to_string(work.path().join(session_id).join(file))
            .await
            .expect("the spilled file exists under the session's working copy");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("the spilled file holds JSON");
        assert_eq!(
            parsed["content"].as_str().unwrap().len(),
            ASSET_LEN,
            "session's asset truncated"
        );
    }

    // Confirm session routing on the same tunnel: each session's small index
    // names its own working copy.
    let index_a = tunnel_read(&tunnels, &session_a, "index.txt")
        .await
        .expect("session A's index failed");
    let index_b = tunnel_read(&tunnels, &session_b, "index.txt")
        .await
        .expect("session B's index failed");
    assert_eq!(
        content(&index_a),
        INDEX_A,
        "session A reached the wrong copy"
    );
    assert_eq!(
        content(&index_b),
        INDEX_B,
        "session B reached the wrong copy"
    );
}
