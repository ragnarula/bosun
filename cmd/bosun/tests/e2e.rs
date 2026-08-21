//! End-to-end tests: boot the control plane and a node, clone a session on a
//! local repo, drive it through the control-plane proxy, and stop it. A second
//! test spawns a dev session in an existing directory and checks the
//! directory survives a stop.
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
async fn clone_drive_and_stop_a_session_end_to_end() {
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
    let cp_url = format!("http://127.0.0.1:{serve_port}");

    let serve_config = root.join("serve.toml");
    std::fs::write(
        &serve_config,
        format!(
            "listen_addr = \"127.0.0.1:{serve_port}\"\n\
             node_timeout_secs = 10\n"
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
             browse_roots = [\"{}\"]\n",
            work_dir.display(),
            root.display()
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

    // Clone a session via the CLI.
    let clone_out = Command::new(BOSUN)
        .args([
            "clone",
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
        clone_out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&clone_out.stderr)
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
    assert_eq!(
        session["repo_url"].as_str(),
        repo.to_str(),
        "clone session must report its repository"
    );

    // The opencode server answers through the control-plane path route. The
    // tunnel registers just after the session appears, so retry until the
    // route is live.
    let health = wait_for_value(
        || async {
            let Ok(response) =
                reqwest::get(format!("{cp_url}/session/{session_id}/global/health")).await
            else {
                return None;
            };
            if !response.status().is_success() {
                return None;
            }
            response.json::<Value>().await.ok()
        },
        "session route to become live",
    )
    .await;
    assert_eq!(health["healthy"], true);

    // The client can create a session through the route, rooted in the clone.
    let created = wait_for_value(
        || async {
            let Ok(response) = reqwest::Client::new()
                .post(format!("{cp_url}/session/{session_id}/session"))
                .send()
                .await
            else {
                return None;
            };
            if !response.status().is_success() {
                return None;
            }
            response.json::<Value>().await.ok()
        },
        "session creation through the route",
    )
    .await;
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

    // The session disappears, its route closes, and the clone is removed.
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

#[tokio::test]
#[ignore]
async fn dev_session_in_existing_directory_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let repo = root.join("existing");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);

    let serve_port = free_port().await;
    let cp_url = format!("http://127.0.0.1:{serve_port}");

    let serve_config = root.join("serve.toml");
    std::fs::write(
        &serve_config,
        format!(
            "listen_addr = \"127.0.0.1:{serve_port}\"\n\
             node_timeout_secs = 10\n"
        ),
    )
    .unwrap();

    let node_config = root.join("node.toml");
    std::fs::write(
        &node_config,
        format!(
            "cp_url = \"{cp_url}\"\n\
             node_name = \"e2e-node\"\n\
             work_dir = \"{}\"\n\
             browse_roots = [\"{}\"]\n",
            root.join("work").display(),
            root.display()
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

    // The node lists the existing repository as a browsable directory.
    let canonical_root = root.canonicalize().unwrap();
    let listing: Value = reqwest::get(format!("{cp_url}/nodes/e2e-node/dirs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listing["entries"][0]["name"],
        canonical_root.display().to_string()
    );
    assert_eq!(listing["entries"][0]["is_repo"], false);

    let child_listing: Value = reqwest::get(format!(
        "{cp_url}/nodes/e2e-node/dirs?path={}",
        root.display()
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let entries = child_listing["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["name"] == "existing" && e["is_repo"] == true)
    );

    // Spawn a dev session in the existing directory.
    let request = serde_json::json!({ "node": "e2e-node", "dir": repo.display().to_string() });
    let dev_response: Value = reqwest::Client::new()
        .post(format!("{cp_url}/dev"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = dev_response["id"].as_str().unwrap().to_string();

    let health = wait_for_value(
        || async {
            let Ok(response) =
                reqwest::get(format!("{cp_url}/session/{session_id}/global/health")).await
            else {
                return None;
            };
            if !response.status().is_success() {
                return None;
            }
            response.json::<Value>().await.ok()
        },
        "session route to become live",
    )
    .await;
    assert_eq!(health["healthy"], true);

    // Stop the session; the existing directory stays.
    let stop_out = Command::new(BOSUN)
        .args(["stop", &session_id, "--cp-url", &cp_url])
        .output()
        .await
        .unwrap();
    assert!(stop_out.status.success());
    assert!(repo.is_dir(), "dev session must not delete the directory");

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
