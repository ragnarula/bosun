//! The web pane's browser tests. The real control-plane router serves the pane
//! and the session API from a temporary store seeded with sessions, and
//! `tests/browser/pane.py` drives the pane in a phone-sized Chromium through
//! Playwright. No loop or node runs: the pane reads everything it draws from
//! the store.
//!
//! The test needs `python3` with the `playwright` package and its Chromium on
//! the machine, so it is `#[ignore]`d and runs on demand. See
//! `docs/developer/workflows/running-tests.md`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bosun_common::session::ActivityPhase;
use bosun_common::session::Block;
use bosun_common::session::Event;
use bosun_common::session::Permission;
use bosun_common::session::Role;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_control::api::AppState;
use bosun_control::api::router;
use bosun_control::commands::CommandQueue;
use bosun_control::loops::AgentRegistry;
use bosun_control::mcp_manager::McpManager;
use bosun_control::mcp_oauth::McpOAuthContext;
use bosun_control::registry::NodeRegistry;
use bosun_control::skills_repos::GitHubClient;
use bosun_control::tunnel::TunnelRegistry;
use bosun_store::store::RouteAnswer;
use bosun_store::store::Store;
use serde_json::json;

/// The session with a long transcript. `pane.py` names the same ids.
const LONG: &str = "long-session";
/// The long session's child, which the child panel follows.
const CHILD: &str = "child-session";
/// A session whose question and its answered copy sit on either side of the
/// first page boundary.
const ASK: &str = "ask-session";
/// The child whose question the ask session surfaced.
const ASK_CHILD: &str = "ask-child";
/// A running session whose newest event is loop activity, so its header
/// counts the seconds since that activity.
const RUNNING: &str = "running-session";
/// A session whose last message is a question nobody has answered.
const PENDING_ASK: &str = "pending-ask-session";
/// A session with no messages at all.
const EMPTY: &str = "empty-session";
/// A session whose one reply holds a mermaid diagram.
const DIAGRAM: &str = "diagram-session";
/// How many turns the long session holds. Each turn is four messages, so the
/// transcript is many pages long.
const TURNS: usize = 300;
/// How long the whole script may run. It takes about 15 seconds; a hung
/// browser must not hold the test forever.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// The exit status `pane.py` returns when it cannot start Chromium, most often
/// because Playwright or its Chromium is not installed.
const NO_BROWSER: i32 = 2;
/// Root sessions beyond the seeded ones, so the home list scrolls.
const FILLER_SESSIONS: usize = 40;

#[tokio::test]
#[ignore = "needs python3 with playwright and its chromium"]
async fn the_pane_works_on_a_phone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("sessions.db")).unwrap();
    seed(&store).await;
    let addr = serve(store).await;

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/browser/pane.py");
    let mut checks = tokio::process::Command::new("python3")
        .arg(&script)
        .arg(format!("http://{addr}"))
        .kill_on_drop(true)
        .spawn()
        .expect("python3 should be on PATH: see docs/developer/workflows/running-tests.md");
    let status = tokio::time::timeout(SCRIPT_TIMEOUT, checks.wait())
        .await
        .expect("pane.py should finish within its timeout")
        .unwrap();
    assert_ne!(
        status.code(),
        Some(NO_BROWSER),
        "pane.py could not start Chromium; its error is printed above. If Playwright or its Chromium is not installed, see docs/developer/workflows/running-tests.md"
    );
    assert!(status.success(), "pane.py reported failed checks");
}

/// The real router over `store`, on a free port. Nothing is configured, so the
/// routes that start work answer with an error; the pane only reads here.
async fn serve(store: Store) -> SocketAddr {
    let oauth = McpOAuthContext::new(reqwest::Client::new(), store.clone(), None);
    let state = Arc::new(AppState {
        registry: Arc::new(NodeRegistry::new(Duration::from_secs(30))),
        commands: Arc::new(CommandQueue::new(Duration::from_secs(30))),
        tunnels: Arc::new(TunnelRegistry::new()),
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
        oauth_redirect_uri: None,
        mcp: Arc::new(McpManager::new(
            store.clone(),
            oauth.clone(),
            reqwest::Client::new(),
        )),
        mcp_oauth: oauth,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    addr
}

async fn seed(store: &Store) {
    seed_long_session(store).await;
    seed_child(store).await;
    seed_ask_session(store).await;
    seed_running_session(store).await;
    seed_pending_ask_session(store).await;
    create(store, EMPTY, None, Some("the empty session"), 1_999_990).await;
    seed_diagram_session(store).await;
    for i in 0..FILLER_SESSIONS {
        // A summary with no break in it is what a model writes for a path or
        // an identifier, and the list row must still fit the screen.
        let summary = format!("filler {i:02} {}", "unbroken_summary_".repeat(12));
        create(
            store,
            &format!("filler-{i:02}"),
            None,
            Some(&summary),
            1_000 + i as i64,
        )
        .await;
    }
}

/// Every turn has the events a real turn records between its messages: an
/// activity phase, a tool call and its result, a model call. The assistant's
/// reply carries a code line, a table row and a link, each wider than a phone.
async fn seed_long_session(store: &Store) {
    create(store, LONG, None, Some("the long session"), 2_000_000).await;
    let wide = format!(
        "\n\n```\n{}\n```\n\n| a | b |\n|---|---|\n| {} | y |\n\nhttps://example.com/{}",
        "wide_code_line_".repeat(30),
        "cell".repeat(40),
        "a".repeat(150),
    );
    for i in 0..TURNS {
        store
            .append_event(
                LONG,
                &Event::Activity {
                    at_ms: now_ms(),
                    phase: ActivityPhase::WakeStarted,
                },
            )
            .await
            .unwrap();
        text(store, LONG, Role::User, &format!("user message {i}")).await;
        let call = format!("call-{i}");
        let command = format!("ls -la /very/long/path/{}", "x".repeat(200));
        message(
            store,
            LONG,
            Role::Assistant,
            Block::ToolCall {
                id: call.clone(),
                name: "shell".into(),
                args: json!({ "command": command }),
                continues_completion: false,
            },
        )
        .await;
        message(
            store,
            LONG,
            Role::User,
            Block::ToolResult {
                id: call,
                name: "shell".into(),
                is_error: false,
                content: json!("out ".repeat(50)),
            },
        )
        .await;
        store
            .append_event(
                LONG,
                &Event::ModelCall {
                    at_ms: Some(now_ms()),
                    model: "test-model".into(),
                    provider: "test-provider".into(),
                    kind: "completion".into(),
                    input_tokens: Some(10),
                    cached_input_tokens: None,
                    output_tokens: Some(5),
                    cost: None,
                },
            )
            .await
            .unwrap();
        text(store, LONG, Role::Assistant, &format!("reply {i}{wide}")).await;
    }
    // The spawn sits in the newest page, so its result's watch control is on
    // screen when the session opens.
    message(
        store,
        LONG,
        Role::Assistant,
        Block::ToolCall {
            id: "spawn-1".into(),
            name: "spawn".into(),
            args: json!({ "prompt": "look around" }),
            continues_completion: false,
        },
    )
    .await;
    message(
        store,
        LONG,
        Role::User,
        Block::ToolResult {
            id: "spawn-1".into(),
            name: "spawn".into(),
            is_error: false,
            content: json!({ "child_id": CHILD }),
        },
    )
    .await;
}

async fn seed_child(store: &Store) {
    create(store, CHILD, Some(LONG), None, 2_000_001).await;
    for i in 0..5 {
        text(store, CHILD, Role::User, &format!("child message {i}")).await;
        let reply = format!("child reply {i}\n\n```\n{}\n```", "child_code_".repeat(40));
        text(store, CHILD, Role::Assistant, &reply).await;
    }
}

/// A child raises a question, the session surfaces it, and the user's answer
/// is routed back to the child, as the loop and the answer route record it.
/// Routing appends the answered copy of the question as an event, so the
/// question and its copy are two message events. The session is seeded so the
/// pane's first page opens with the copy and the question is the last message
/// of the page before it; `pane.py` checks that the boundary falls there.
async fn seed_ask_session(store: &Store) {
    create(store, ASK, None, Some("the ask session"), 1_999_999).await;
    create(store, ASK_CHILD, Some(ASK), None, 1_999_998).await;
    for i in 0..100 {
        text(store, ASK, Role::User, &format!("before {i}")).await;
    }
    let question = "Which one?";
    let ask_id = store
        .append_message(
            ASK,
            Role::Assistant,
            &Block::Ask {
                message: question.into(),
                options: vec!["first".into(), "second".into()],
                child_id: Some(ASK_CHILD.into()),
                answer: None,
            },
        )
        .await
        .unwrap();
    store
        .set_pending_ask(ASK, ASK_CHILD, ASK_CHILD, question, ask_id)
        .await
        .unwrap();
    let routed = store.route_answer(ASK, "first").await.unwrap();
    assert_eq!(
        routed,
        RouteAnswer::Routed {
            leaf_id: ASK_CHILD.into()
        }
    );
    for i in 0..59 {
        text(store, ASK, Role::User, &format!("after {i}")).await;
    }
}

/// The state change is recorded before the first message, so the stream's tail
/// does not replay it: the pane learns the state from the session list only.
async fn seed_running_session(store: &Store) {
    create(store, RUNNING, None, Some("the running session"), 1_999_993).await;
    store
        .set_state(RUNNING, SessionState::Running)
        .await
        .unwrap();
    text(store, RUNNING, Role::User, "start working").await;
    store
        .append_event(
            RUNNING,
            &Event::Activity {
                at_ms: now_ms(),
                phase: ActivityPhase::WakeStarted,
            },
        )
        .await
        .unwrap();
}

async fn seed_pending_ask_session(store: &Store) {
    create(
        store,
        PENDING_ASK,
        None,
        Some("the pending ask session"),
        1_999_992,
    )
    .await;
    text(store, PENDING_ASK, Role::User, "ask me something").await;
    message(
        store,
        PENDING_ASK,
        Role::Assistant,
        // The ask session's question, word for word and from the same child.
        Block::Ask {
            message: "Which one?".into(),
            options: vec!["first".into(), "second".into()],
            child_id: Some(ASK_CHILD.into()),
            answer: None,
        },
    )
    .await;
}

async fn seed_diagram_session(store: &Store) {
    // The reply is the session's first message, so a session opened after one
    // that was streaming draws it first.
    create(store, DIAGRAM, None, Some("the diagram session"), 1_999_991).await;
    text(
        store,
        DIAGRAM,
        Role::Assistant,
        "Here it is:\n\n```mermaid\ngraph TD\n  A --> B\n```",
    )
    .await;
}

async fn create(
    store: &Store,
    id: &str,
    parent: Option<&str>,
    summary: Option<&str>,
    created_at_secs: i64,
) {
    store
        .create_session(&Session {
            id: id.into(),
            node: "node-1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/work/repo".into(),
            model: "test-model".into(),
            persona: None,
            parent_id: parent.map(str::to_string),
            owner_id: parent.unwrap_or(id).into(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            mcp_servers: "".into(),
            state: SessionState::WaitingForInput,
            interrupt_cause: None,
            created_at_secs,
            prompt: Some("look around".into()),
            summary: summary.map(str::to_string),
        })
        .await
        .unwrap();
}

async fn text(store: &Store, session: &str, role: Role, text: &str) {
    message(store, session, role, Block::Text { text: text.into() }).await;
}

async fn message(store: &Store, session: &str, role: Role, block: Block) {
    store.append_message(session, role, &block).await.unwrap();
}

fn now_ms() -> u64 {
    bosun_common::time::unix_ms(std::time::SystemTime::now())
}
