//! End-to-end test: boots the control plane and a node, spawns a session on a
//! local repo, drives it through the control-plane proxy, then stops it.
//!
//! Needs `git` and the `opencode` binary on PATH.
//!
//! Run with:
//!   cargo test -p bosun --test e2e -- --ignored --nocapture

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::sleep;

const BOSUN: &str = env!("CARGO_BIN_EXE_bosun");

#[tokio::test]
#[ignore]
async fn spawn_drive_and_stop_a_session_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ],
    );

    let serve_port = free_port().await;
    let node_port = free_port().await;
    let cp_url = format!("http://127.0.0.1:{serve_port}");

    let template = root.join("opencode.json");
    std::fs::write(&template, "{}").unwrap();
    let serve_config = root.join("serve.toml");
    std::fs::write(
        &serve_config,
        format!(
            "listen_addr = \"127.0.0.1:{serve_port}\"\n\
             template_path = \"{}\"\n\
             node_timeout_secs = 10\n",
            template.display()
        ),
    )
    .unwrap();

    let work_dir = root.join("work");
    let node_config = root.join("node.toml");
    std::fs::write(
        &node_config,
        format!(
            "cp_url = \"{cp_url}\"\n\
             node_name = \"e2e-node\"\n\
             work_dir = \"{}\"\n\
             advertise_addr = \"127.0.0.1\"\n\
             heartbeat_interval_secs = 1\n\
             listen_port = {node_port}\n",
            work_dir.display()
        ),
    )
    .unwrap();

    let mut serve = spawn_bosun(
        &["serve", "--config", serve_config.to_str().unwrap()],
        &root.join("serve.log"),
    );
    let mut node = spawn_bosun(
        &["node", "--config", node_config.to_str().unwrap()],
        &root.join("node.log"),
    );

    // The node registers with the control plane.
    wait_for_value(
        || async {
            let Ok(response) = reqwest::get(format!("{cp_url}/nodes")).await else {
                return None;
            };
            let Ok(nodes) = response.json::<Vec<Value>>().await else {
                return None;
            };
            nodes
                .iter()
                .any(|n| n["name"] == "e2e-node" && n["up"] == true)
                .then_some(())
        },
        "node to register",
    )
    .await;

    // Spawn a session via the CLI.
    let spawn_out = Command::new(BOSUN)
        .args([
            "spawn",
            "--node",
            "e2e-node",
            "--cp-url",
            &cp_url,
            repo.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        spawn_out.status.success(),
        "spawn failed: {}",
        String::from_utf8_lossy(&spawn_out.stderr)
    );

    // The session appears as running.
    let session = wait_for_value(
        || async {
            let Ok(response) = reqwest::get(format!("{cp_url}/sessions")).await else {
                return None;
            };
            let Ok(sessions) = response.json::<Vec<Value>>().await else {
                return None;
            };
            sessions.into_iter().find(|s| s["status"] == "running")
        },
        "session to become running",
    )
    .await;
    let session_id = session["id"].as_str().unwrap().to_string();

    // The injected config landed in the clone.
    assert!(work_dir.join(&session_id).join("opencode.json").is_file());

    // The opencode server answers through the control-plane path route.
    let health: Value = reqwest::get(format!("{cp_url}/session/{session_id}/global/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["healthy"], true);

    // The client can create a session through the route, rooted in the clone.
    let created: Value = reqwest::Client::new()
        .post(format!("{cp_url}/session/{session_id}/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(created["id"].as_str().is_some());

    // Stop the session.
    let stop_out = Command::new(BOSUN)
        .args(["stop", &session_id, "--cp-url", &cp_url])
        .output()
        .await
        .unwrap();
    assert!(
        stop_out.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop_out.stderr)
    );

    // The session disappears and its route closes.
    wait_for_value(
        || async {
            let Ok(response) = reqwest::get(format!("{cp_url}/sessions")).await else {
                return None;
            };
            let Ok(sessions) = response.json::<Vec<Value>>().await else {
                return None;
            };
            sessions.is_empty().then_some(())
        },
        "session to be removed",
    )
    .await;
    wait_for_value(
        || async {
            match reqwest::get(format!("{cp_url}/session/{session_id}/global/health")).await {
                Ok(response) => (response.status() == reqwest::StatusCode::NOT_FOUND).then_some(()),
                Err(_) => None,
            }
        },
        "session route to close",
    )
    .await;
    assert!(!work_dir.join(&session_id).exists());

    shutdown(&mut serve, &mut node).await;
}

async fn wait_for_value<F, Fut, T>(mut poll: F, what: &str) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = poll().await {
            return value;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_bosun(args: &[&str], log_path: &Path) -> Child {
    let log = std::fs::File::create(log_path).unwrap();
    Command::new(BOSUN)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn shutdown(serve: &mut Child, node: &mut Child) {
    let _ = node.kill().await;
    let _ = node.wait().await;
    let _ = serve.kill().await;
    let _ = serve.wait().await;
}

fn git(dir: &PathBuf, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}
