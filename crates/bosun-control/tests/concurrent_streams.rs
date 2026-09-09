//! One node tunnel carries tool calls for every session on the node. Two
//! sessions sharing a node reach their own executors concurrently over the
//! tunnel, and one session's reads do not stall the other (the original
//! flow-control regression: concurrent streams over one tunnel stalled after
//! roughly 800 KiB of a large response). Mirrors the reported deployment: a
//! control plane, a node that dials out over one tunnel and hosts in-process
//! executors, and tool calls over logical connections on the same tunnel at
//! the same time.
//!
//! A `file_read` is bounded to what fits inline, so no multi-hundred-KiB body
//! ever travels the tunnel: a whole-file read over the budget is refused with
//! an error instead of being spilled into the working copy, and ranged reads
//! page a large file in inline-sized windows. The concurrency guarantee these
//! tests guard is therefore that concurrent calls all *complete* promptly —
//! every read ends in a bounded `Result` frame or a prompt refusal error —
//! while the sibling small reads return `Result` bodies.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bosun_common::session::Permission;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::tool::SPILL_BYTE_LIMIT;
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

/// The asset is large enough that a whole-file read is refused while a ranged
/// read stays a small inline window.
const ASSET_LINES: usize = 3000;
const INDEX_A: &str = "index page from executor A";
const INDEX_B: &str = "index page from executor B";

/// The asset's body for one session: distinct lines per session so a ranged
/// read proves which working copy it reached.
fn asset_body(tag: &str) -> String {
    let mut body = String::new();
    for i in 1..=ASSET_LINES {
        body.push_str(&format!("{tag} line {i:04} {}\n", "x".repeat(40)));
    }
    body
}

/// The raw slice of `body` the executor returns for a read starting at the
/// 1-based `offset` line and returning up to `limit` lines.
fn expected_window(body: &str, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = body.lines().skip(offset - 1).take(limit).collect();
    format!("{}\n", lines.join("\n"))
}

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
/// per session under `work`, each with a small index file and an asset file
/// whose lines carry `tag`, then the node's one outbound tunnel to the
/// control plane.
async fn node_with_sessions(
    work: &Path,
    cp_addr: SocketAddr,
    sessions: &[(&str, &str, &str)],
) -> Arc<NodeManager> {
    let manager = Arc::new(NodeManager::new(
        work.to_path_buf(),
        vec![work.to_path_buf()],
        format!("http://{cp_addr}"),
        None,
    ));
    for (session_id, index_body, tag) in sessions {
        let dir = work.join(session_id);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("index.txt"), index_body)
            .await
            .unwrap();
        tokio::fs::write(dir.join("asset.txt"), asset_body(tag))
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
/// one typed `file_read` call, returning the terminal reply. `offset` and
/// `limit` are the ranged-read arguments; `None` reads without them. Returns
/// `None` while the tunnel is not yet registered.
async fn tunnel_read(
    tunnels: &TunnelRegistry,
    session_id: &str,
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Option<ToolMsg> {
    let mut conn: LogicalStream = tunnels.open("node-1", session_id).await.ok()?;
    let mut args = json!({ "path": path });
    if let Some(offset) = offset {
        args["offset"] = json!(offset);
    }
    if let Some(limit) = limit {
        args["limit"] = json!(limit);
    }
    write_tool_frame(
        &mut conn,
        &ToolOp::Call {
            run_id: "run-1".into(),
            tool: "file_read".into(),
            args,
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
            // `Result` is a small inline body, `Error` a refusal; both end the
            // reply. A large read never comes back as `Spilled`.
            ToolMsg::Result { .. } | ToolMsg::Error { .. } | ToolMsg::Spilled { .. } => {
                return Some(message);
            }
            ToolMsg::Done { .. } | ToolMsg::Ack | ToolMsg::Event { .. } => {}
        }
    }
}

fn content(reply: &ToolMsg) -> String {
    let ToolMsg::Result { content } = reply else {
        panic!("file_read must return a result: {reply:?}");
    };
    content["content"].as_str().unwrap().to_string()
}

/// Waits until the tunnel is live, then reads the index and the whole asset
/// concurrently, and checks that the whole-file read is refused outright.
#[tokio::test]
async fn concurrent_streams_over_one_node_tunnel_deliver_both_bodies() {
    let (cp_addr, store, tunnels, _state_dir) = control_plane().await;
    let work = tempfile::tempdir().unwrap();
    let session_id = uuid::Uuid::new_v4().to_string();
    register_session(&store, &session_id).await;
    node_with_sessions(work.path(), cp_addr, &[(&session_id, INDEX_A, "A")]).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel_read(&tunnels, &session_id, "index.txt", None, None).await {
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

    let index = tunnel_read(&tunnels, &session_id, "index.txt", None, None);
    let asset = tunnel_read(&tunnels, &session_id, "asset.txt", None, None);

    let (index, asset) = tokio::join!(index, asset);
    let index = index.expect("index read failed");
    let asset = asset.expect("asset read failed");
    assert_eq!(content(&index), INDEX_A, "index body mismatch");

    // The whole asset does not fit inline, so the read is refused with an
    // error instead of spilling a copy of the file into the working copy.
    assert!(
        matches!(asset, ToolMsg::Error { .. }),
        "an oversized whole-file read must be refused: {asset:?}"
    );

    // Ranged reads of the asset and a sibling index read run concurrently
    // over the same tunnel and all complete promptly with the right windows.
    let body = asset_body("A");
    let windows = [(1usize, 200usize), (1001, 200), (2001, 200), (2801, 200)];
    let handles: Vec<_> = windows
        .into_iter()
        .map(|(offset, limit)| {
            let tunnels = tunnels.clone();
            let session_id = session_id.clone();
            tokio::spawn(async move {
                tunnel_read(
                    &tunnels,
                    &session_id,
                    "asset.txt",
                    Some(offset as u64),
                    Some(limit as u64),
                )
                .await
            })
        })
        .collect();
    let index_handle = {
        let tunnels = tunnels.clone();
        let session_id = session_id.clone();
        tokio::spawn(
            async move { tunnel_read(&tunnels, &session_id, "index.txt", None, None).await },
        )
    };

    let replies = tokio::time::timeout(Duration::from_secs(10), async {
        let mut replies = Vec::new();
        for handle in handles {
            replies.push(
                handle
                    .await
                    .expect("the read task must not panic")
                    .expect("read failed"),
            );
        }
        replies.push(
            index_handle
                .await
                .expect("the read task must not panic")
                .expect("read failed"),
        );
        replies
    })
    .await
    .expect("the concurrent reads stalled");

    for (reply, &(offset, limit)) in replies.iter().zip(windows.iter()) {
        let ToolMsg::Result { content } = reply else {
            panic!("a ranged read must return a result: {reply:?}");
        };
        let text = content["content"].as_str().unwrap();
        assert!(
            text.len() <= SPILL_BYTE_LIMIT,
            "a window stays inline-sized"
        );
        assert_eq!(
            text,
            expected_window(&body, offset, limit),
            "the asset window reached the wrong working copy or returned wrong lines"
        );
    }
    assert_eq!(
        content(replies.last().unwrap()),
        INDEX_A,
        "the sibling index read is intact"
    );

    assert!(
        !work.path().join(&session_id).join("tool-output").exists(),
        "a refused or ranged read must not write a spilled file"
    );
}

/// Two sessions share the node's one tunnel: each session's calls reach its
/// own working copy, concurrently, and each session's oversized whole-file
/// read is refused rather than spilled.
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
        &[(&session_a, INDEX_A, "A"), (&session_b, INDEX_B, "B")],
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel_read(&tunnels, &session_a, "index.txt", None, None).await {
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

    // Each session reads a window of its own asset and its whole asset (which
    // is refused) concurrently on the shared tunnel.
    let windowed = |session_id: String| {
        let tunnels = tunnels.clone();
        tokio::spawn(async move {
            tunnel_read(&tunnels, &session_id, "asset.txt", Some(1), Some(200)).await
        })
    };
    let whole_a = {
        let tunnels = tunnels.clone();
        let session_a = session_a.clone();
        tokio::spawn(
            async move { tunnel_read(&tunnels, &session_a, "asset.txt", None, None).await },
        )
    };
    let whole_b = {
        let tunnels = tunnels.clone();
        let session_b = session_b.clone();
        tokio::spawn(
            async move { tunnel_read(&tunnels, &session_b, "asset.txt", None, None).await },
        )
    };
    let window_a = windowed(session_a.clone());
    let window_b = windowed(session_b.clone());

    let timeout = tokio::time::timeout(Duration::from_secs(10), async {
        let whole_a = whole_a
            .await
            .expect("the read task must not panic")
            .expect("read failed");
        let whole_b = whole_b
            .await
            .expect("the read task must not panic")
            .expect("read failed");
        let window_a = window_a
            .await
            .expect("the read task must not panic")
            .expect("read failed");
        let window_b = window_b
            .await
            .expect("the read task must not panic")
            .expect("read failed");
        (whole_a, whole_b, window_a, window_b)
    })
    .await
    .expect("the concurrent reads stalled");
    let (whole_a, whole_b, window_a, window_b) = timeout;

    for whole in [&whole_a, &whole_b] {
        assert!(
            matches!(whole, ToolMsg::Error { .. }),
            "an oversized whole-file read must be refused: {whole:?}"
        );
    }
    assert_eq!(
        content(&window_a),
        expected_window(&asset_body("A"), 1, 200)
    );
    assert_eq!(
        content(&window_b),
        expected_window(&asset_body("B"), 1, 200)
    );

    // A second windowed read of each session's asset reaches its own copy.
    let again_a = windowed(session_a.clone());
    let again_b = windowed(session_b.clone());
    let (again_a, again_b) = tokio::join!(again_a, again_b);
    let again_a = again_a
        .expect("session A's read failed")
        .expect("read failed");
    let again_b = again_b
        .expect("session B's read failed")
        .expect("read failed");
    assert_eq!(content(&again_a), expected_window(&asset_body("A"), 1, 200));
    assert_eq!(content(&again_b), expected_window(&asset_body("B"), 1, 200));

    for session_id in [&session_a, &session_b] {
        assert!(
            !work.path().join(session_id).join("tool-output").exists(),
            "a refused or ranged read must not write a spilled file"
        );
    }
}
