//! End-to-end tests: boot the control plane and a node over HTTPS with a
//! self-signed certificate, clone a session on a local repo, drive it through
//! the control-plane proxy, and stop it. A second test spawns a dev session in
//! an existing directory and checks the directory survives a stop.
//!
//! Needs `git` and the `opencode` binary on PATH.
//!
//! Run with:
//!   cargo test -p bosun --test e2e -- --ignored --nocapture

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::IsCa;
use rcgen::KeyPair;
use serde_json::Value;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::sleep;

const BOSUN: &str = env!("CARGO_BIN_EXE_bosun");

/// Writes a self-signed CA and a leaf certificate for `127.0.0.1`. Returns
/// the CA, leaf certificate, and leaf key paths.
fn write_tls_files(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf_params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
    leaf_params.is_ca = IsCa::NoCa;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

    let ca_path = root.join("ca.pem");
    let cert_path = root.join("cert.pem");
    let key_path = root.join("key.pem");
    std::fs::write(&ca_path, ca_cert.pem()).unwrap();
    std::fs::write(&cert_path, leaf_cert.pem()).unwrap();
    std::fs::write(&key_path, leaf_key.serialize_pem()).unwrap();
    (ca_path, cert_path, key_path)
}

/// An HTTP client that trusts the test CA, so it can reach the HTTPS control
/// plane.
fn test_client(ca_path: &Path) -> reqwest::Client {
    let ca = std::fs::read(ca_path).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .build()
        .unwrap()
}

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

    let (ca_path, cert_path, key_path) = write_tls_files(root);
    let client = test_client(&ca_path);

    let serve_port = free_port().await;
    let cp_url = format!("https://127.0.0.1:{serve_port}");

    let serve_config = root.join("serve.toml");
    std::fs::write(
        &serve_config,
        format!(
            "listen_addr = \"127.0.0.1:{serve_port}\"\n\
             node_timeout_secs = 10\n\
             tls_cert = \"{}\"\n\
             tls_key = \"{}\"\n",
            cert_path.display(),
            key_path.display()
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
             browse_roots = [\"{}\"]\n\
             ca_cert = \"{}\"\n",
            work_dir.display(),
            root.display(),
            ca_path.display()
        ),
    )
    .unwrap();

    let mut serve = spawn_bosun(
        &["serve", "--config", serve_config.to_str().unwrap()],
        &root.join("serve.log"),
        &ca_path,
    );
    let mut node = spawn_bosun(
        &["node", "--config", node_config.to_str().unwrap()],
        &root.join("node.log"),
        &ca_path,
    );

    wait_for_value(
        || async {
            let Ok(response) = client.get(format!("{cp_url}/nodes")).send().await else {
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
        .env("BOSUN_CA_CERT", &ca_path)
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
            let Ok(response) = client.get(format!("{cp_url}/sessions")).send().await else {
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
            let Ok(response) = client
                .get(format!("{cp_url}/session/{session_id}/global/health"))
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
        "session route to become live",
    )
    .await;
    assert_eq!(health["healthy"], true);

    // The client can create a session through the route, rooted in the clone.
    let created = wait_for_value(
        || async {
            let Ok(response) = client
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

    // The same session is reachable at its subdomain host without a reverse
    // proxy: the gateway routes purely on the Host header. The terminal API
    // and the web UI root both answer at the subdomain origin.
    let subdomain = format!("{session_id}.bosun.on.21cs.biz");
    let host_health = wait_for_value(
        || async {
            let Ok(response) = client
                .get(format!("{cp_url}/global/health"))
                .header("host", &subdomain)
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
        "host-routed health check",
    )
    .await;
    assert_eq!(host_health["healthy"], true);

    let host_created = wait_for_value(
        || async {
            let Ok(response) = client
                .post(format!("{cp_url}/session"))
                .header("host", &subdomain)
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
        "host-routed session creation",
    )
    .await;
    assert!(host_created["id"].as_str().is_some());

    let web_ui = client
        .get(format!("{cp_url}/"))
        .header("host", &subdomain)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        web_ui
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/html"),
        "the web UI must be served at the subdomain root"
    );

    // Stop the session.
    let stop_out = Command::new(BOSUN)
        .env("BOSUN_CA_CERT", &ca_path)
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
            let Ok(response) = client.get(format!("{cp_url}/sessions")).send().await else {
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
            match client
                .get(format!("{cp_url}/session/{session_id}/global/health"))
                .send()
                .await
            {
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

    let (ca_path, cert_path, key_path) = write_tls_files(root);
    let client = test_client(&ca_path);

    let serve_port = free_port().await;
    let cp_url = format!("https://127.0.0.1:{serve_port}");

    let serve_config = root.join("serve.toml");
    std::fs::write(
        &serve_config,
        format!(
            "listen_addr = \"127.0.0.1:{serve_port}\"\n\
             node_timeout_secs = 10\n\
             tls_cert = \"{}\"\n\
             tls_key = \"{}\"\n",
            cert_path.display(),
            key_path.display()
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
             browse_roots = [\"{}\"]\n\
             ca_cert = \"{}\"\n",
            root.join("work").display(),
            root.display(),
            ca_path.display()
        ),
    )
    .unwrap();

    let mut serve = spawn_bosun(
        &["serve", "--config", serve_config.to_str().unwrap()],
        &root.join("serve.log"),
        &ca_path,
    );
    let mut node = spawn_bosun(
        &["node", "--config", node_config.to_str().unwrap()],
        &root.join("node.log"),
        &ca_path,
    );

    wait_for_value(
        || async {
            let Ok(response) = client.get(format!("{cp_url}/nodes")).send().await else {
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
    let listing: Value = client
        .get(format!("{cp_url}/nodes/e2e-node/dirs"))
        .send()
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

    let child_listing: Value = client
        .get(format!(
            "{cp_url}/nodes/e2e-node/dirs?path={}",
            root.display()
        ))
        .send()
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
    let dev_response: Value = client
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
            let Ok(response) = client
                .get(format!("{cp_url}/session/{session_id}/global/health"))
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
        "session route to become live",
    )
    .await;
    assert_eq!(health["healthy"], true);

    // Stop the session; the existing directory stays.
    let stop_out = Command::new(BOSUN)
        .env("BOSUN_CA_CERT", &ca_path)
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

fn spawn_bosun(args: &[&str], log_path: &Path, ca_path: &Path) -> Child {
    let log = std::fs::File::create(log_path).unwrap();
    Command::new(BOSUN)
        .args(args)
        .env("BOSUN_CA_CERT", ca_path)
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
