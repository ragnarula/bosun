//! The CLI's update commands: self-update, and demanding an update from a
//! named node. The self-update reads the control plane's announced version
//! off an existing control-plane route, downloads the released binary for
//! that version and this platform from the release feed, verifies it, and
//! installs it over the running binary; it does not restart itself, the next
//! invocation runs the new binary. A demanded node update is only enqueued —
//! the control plane fills in its version and the node applies it on its next
//! poll.

use std::cmp::Ordering;
#[cfg(any(windows, test))]
use std::ffi::OsStr;
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use bosun_common::target::TARGET;
use bosun_common::types::NodeUpdateRequest;
use bosun_common::types::X_BOSUN_VERSION;
use bosun_common::update::UpdateError;
use bosun_common::update::fetch_release_artifact;
#[cfg(any(windows, test))]
use bosun_common::update::move_target_to_previous;
#[cfg(any(windows, test))]
use bosun_common::update::rename_staged_to_target;
use bosun_common::update::resolve_update_base_url;
#[cfg(unix)]
use bosun_common::update::swap_binary;
use bosun_common::update::verify_binary_version;
use bosun_common::version::VERSION;
use bosun_common::version::compare;
use tracing::info;

/// How long the version probe waits for the control plane to answer before
/// the update gives up on an unreachable control plane.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Env var naming the staged file a Windows CLI update finalizer must
/// install. The `CLI_` prefix keeps these markers distinct from the node's
/// self-update markers (`BOSUN_UPDATE_STAGED`): the node and the CLI are one
/// binary, and this CLI finalize hook runs before `run_node`, so a staged
/// node boot must never match the CLI markers.
#[cfg(any(windows, test))]
const UPDATE_MARKER: &str = "BOSUN_CLI_UPDATE_STAGED";
/// Env var naming the canonical path the staged file must be installed at.
#[cfg(any(windows, test))]
const UPDATE_TARGET: &str = "BOSUN_CLI_UPDATE_TARGET";

/// How long the finalizer waits for the old process to release its image
/// file before giving up on the swap.
#[cfg(windows)]
const UPDATE_LOCK_WAIT: Duration = Duration::from_secs(10);
/// How long between swap retries while the old process's lock is pending.
#[cfg(windows)]
const UPDATE_LOCK_RETRY: Duration = Duration::from_millis(250);

/// The version gate's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Apply,
    UpToDate,
}

/// Decides whether to download: equal versions are a no-op even with
/// --force, a CLI ahead of the control plane needs --force, an unparsable
/// control-plane version is a hard error.
fn gate(cli_version: &str, cp_version: &str, force: bool) -> Result<Gate, UpdateError> {
    match compare(cp_version, cli_version) {
        Some(Ordering::Equal) => Ok(Gate::UpToDate),
        Some(Ordering::Less) if !force => Err(UpdateError::DowngradeRequiresForce {
            cli_version: cli_version.to_string(),
            cp_version: cp_version.to_string(),
        }),
        Some(_) => Ok(Gate::Apply),
        None => Err(UpdateError::UnparsableVersion {
            version: cp_version.to_string(),
        }),
    }
}

/// Updates the running binary to the control plane's announced version,
/// fetched from the release feed: the `BOSUN_UPDATE_BASE_URL` mirror when
/// set, else GitHub Releases. The swap happens in this process on Unix; on
/// Windows a finalizer copy of this binary installs the staged file after
/// this process exits.
pub(crate) async fn run_update(
    client: &reqwest::Client,
    cp_url: &str,
    force: bool,
) -> Result<(), UpdateError> {
    let current = std::env::current_exe().context("failed to find the running binary")?;
    // The CLI config stores no update base URL, so the env override and the
    // GitHub default decide the feed.
    let base_url = resolve_update_base_url(None);
    apply_update(client, cp_url, &base_url, &current, force).await
}

/// The version the control plane announces on its nodes response header. The
/// control plane serves no update assets; it only announces its version, so
/// the self-update reads the header off an existing route and fetches the
/// announced binary from the release feed. The reachability error says what
/// the update depends on, because a control plane that cannot be reached
/// leaves the CLI with no target version.
async fn control_plane_version(
    client: &reqwest::Client,
    cp_url: &str,
) -> Result<String, UpdateError> {
    let url = format!("{}/nodes", cp_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .timeout(VERSION_TIMEOUT)
        .send()
        .await
        .with_context(|| {
            format!(
                "failed to reach the control plane at {cp_url}: bosun update syncs this CLI to the control plane's version, so the control plane must be reachable"
            )
        })?;
    let response = response
        .error_for_status()
        .with_context(|| format!("the control plane at {cp_url} returned an error"))?;
    let version = response
        .headers()
        .get(X_BOSUN_VERSION)
        .and_then(|value| value.to_str().ok())
        .with_context(|| {
            format!(
                "the control plane at {cp_url} did not announce its version on the {url} response"
            )
        })?;
    Ok(version.to_string())
}

/// Demands an update from each named node: enqueues an `Update` command that
/// carries `force`; the control plane fills in its own version. The command
/// is applied on the node's next poll, and the outcome appears in
/// `bosun nodes` — an ahead or disabled node may refuse asynchronously. The
/// update notice is deliberately not printed here: the S6 spec says the
/// `update` command itself never announces a newer control plane.
pub(crate) async fn update_nodes(
    client: &reqwest::Client,
    cp_url: &str,
    nodes: &[String],
    force: bool,
) -> anyhow::Result<()> {
    for node in nodes {
        let response = client
            .post(format!("{cp_url}/nodes/{node}/update"))
            .json(&NodeUpdateRequest { force })
            .send()
            .await
            .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .with_context(|| format!("failed to read response from {cp_url}"))?;
            return Err(anyhow::anyhow!("update failed for node {node}: {text}"));
        }
        println!("{}", enqueued_message(node));
    }
    Ok(())
}

/// The message printed once a node update is enqueued. The enqueue is not the
/// outcome: the node applies the command on its next poll and reports through
/// `bosun nodes`, where a refusal shows up asynchronously.
fn enqueued_message(node: &str) -> String {
    format!("update command queued for node {node}; the outcome appears in \"bosun nodes\"")
}

/// Reads the control plane's announced version, gates on the versions, fetches
/// and verifies the matching release artifact, and installs it at `target`.
/// Testable against stub servers and a fake target path. The gate runs before
/// the release fetch, so equal versions or an ahead-without-`--force` CLI
/// report their own outcome even when the release feed serves no artifact for
/// this version.
async fn apply_update(
    client: &reqwest::Client,
    cp_url: &str,
    base_url: &str,
    target: &Path,
    force: bool,
) -> Result<(), UpdateError> {
    let cp_version = control_plane_version(client, cp_url).await?;
    if gate(VERSION, &cp_version, force)? == Gate::UpToDate {
        println!("already up to date at version {VERSION}");
        return Ok(());
    }
    let dir = target
        .parent()
        .context("the update target has no parent directory")?;
    info!(
        target = %TARGET,
        version = %cp_version,
        "cli update: downloading the release archive"
    );
    let staged = fetch_release_artifact(client, base_url, &cp_version, TARGET, dir).await?;
    let outcome = async {
        verify_binary_version(&staged, &cp_version).await?;
        install(target, &staged)?;
        Ok::<(), UpdateError>(())
    }
    .await;
    if outcome.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    outcome?;
    println!("updated bosun to {cp_version}; the next invocation runs the new binary");
    Ok(())
}

/// Installs the staged binary over the running binary.
#[cfg(unix)]
fn install(target: &Path, staged: &Path) -> Result<(), UpdateError> {
    swap_binary(target, staged)
}

/// Copies this binary to a finalizer path and spawns it with the update
/// markers, so the finalizer swaps the staged file into place after this
/// process exits and its image file unlocks.
#[cfg(windows)]
fn install(target: &Path, staged: &Path) -> Result<(), UpdateError> {
    info!(
        target = %target.display(),
        "cli update: launching the update finalizer"
    );
    spawn_finalizer(target, staged).context("failed to launch the update finalizer")?;
    Ok(())
}

#[cfg(windows)]
fn spawn_finalizer(target: &Path, staged: &Path) -> std::io::Result<()> {
    let finalizer = finalizer_path(target);
    std::fs::copy(std::env::current_exe()?, &finalizer)?;
    let mut command = std::process::Command::new(&finalizer);
    command.env(UPDATE_MARKER, staged);
    command.env(UPDATE_TARGET, target);
    command.spawn()?;
    Ok(())
}

/// The path the finalizer is copied to: the running binary next to the
/// canonical target, under a name no running image uses.
#[cfg(windows)]
fn finalizer_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bosun.exe");
    let stem = file_name.strip_suffix(".exe").unwrap_or(file_name);
    target.with_file_name(format!("{stem}.update-finalizer.exe"))
}

/// Whether this process was spawned as an update finalizer: one of the update
/// markers is set. The markers are only the parent-to-finalizer handoff; a
/// normal boot has neither.
#[cfg(windows)]
pub(crate) fn update_marker_is_set() -> bool {
    std::env::var_os(UPDATE_MARKER).is_some_and(|value| !value.is_empty())
        || std::env::var_os(UPDATE_TARGET).is_some_and(|value| !value.is_empty())
}

/// Installs the staged update this process was spawned as: moves the
/// installed binary aside and renames the staged file over it. No-op on a
/// normal boot. Runs in the finalizer process, after the old one has exited,
/// because the old process's running image is locked.
#[cfg(windows)]
pub(crate) async fn finalize_update() -> Result<(), UpdateError> {
    let Some(pending) = pending_update(
        std::env::var_os(UPDATE_MARKER).as_deref(),
        std::env::var_os(UPDATE_TARGET).as_deref(),
    ) else {
        return Ok(());
    };
    finalize_pending_update(&pending, UPDATE_LOCK_WAIT, UPDATE_LOCK_RETRY, clear_markers).await
}

/// The pending update: the staged binary to install and the canonical path
/// to install it at. Parsed from the update marker env vars.
#[cfg(any(windows, test))]
struct PendingUpdate {
    staged: PathBuf,
    target: PathBuf,
}

#[cfg(any(windows, test))]
fn pending_update(staged: Option<&OsStr>, target: Option<&OsStr>) -> Option<PendingUpdate> {
    let (staged, target) = match (staged, target) {
        (Some(staged), Some(target)) if !staged.is_empty() && !target.is_empty() => {
            (staged, target)
        }
        _ => return None,
    };
    Some(PendingUpdate {
        staged: PathBuf::from(staged),
        target: PathBuf::from(target),
    })
}

/// Installs the pending update: moves the installed binary aside and renames
/// the staged file over it. The old process's image file stays locked briefly
/// after it exits, so the move is retried for up to `wait` at `retry`
/// intervals. Clears the markers through `clear` on both success and failure
/// so a later boot does not replay the swap.
#[cfg(any(windows, test))]
async fn finalize_pending_update(
    pending: &PendingUpdate,
    wait: Duration,
    retry: Duration,
    clear: impl FnOnce(),
) -> Result<(), UpdateError> {
    let outcome = async {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match move_target_to_previous(&pending.target) {
                Ok(()) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(retry).await;
                }
                Err(error) => return Err(error),
            }
        }
        rename_staged_to_target(&pending.target, &pending.staged)
    }
    .await;

    clear();
    outcome
}

/// Removes the update markers, so a later restart of this process does not
/// replay the swap.
#[cfg(windows)]
fn clear_markers() {
    // SAFETY: the marker env vars are only read at startup, before this
    // point, and no other thread touches them.
    unsafe {
        std::env::remove_var(UPDATE_MARKER);
        std::env::remove_var(UPDATE_TARGET);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path as AxumPath;
    use axum::extract::State;
    use axum::http::HeaderValue;
    use axum::http::StatusCode;
    use axum::http::Uri;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::get;
    use axum::routing::post;
    use serde_json::Value;
    use sha2::Digest;
    use sha2::Sha256;
    use tempfile::tempdir;

    use super::*;

    /// A version strictly newer than `version`: the patch bumped and the
    /// prerelease dropped, so a zero patch or a prerelease `version` cannot
    /// break the arithmetic.
    fn newer_than(version: &str) -> String {
        let mut parsed = semver::Version::parse(version).expect("version must parse as semver");
        if parsed.patch == u64::MAX {
            parsed.minor += 1;
            parsed.patch = 0;
        } else {
            parsed.patch += 1;
        }
        parsed.pre = semver::Prerelease::EMPTY;
        parsed.to_string()
    }

    /// A version strictly older than `version`: the patch dropped, or the
    /// previous minor or major release when the patch is 0, with the
    /// prerelease dropped. `0.0.0` falls back to a prerelease, which sorts
    /// below every release.
    fn older_than(version: &str) -> String {
        let mut parsed = semver::Version::parse(version).expect("version must parse as semver");
        if parsed.patch > 0 {
            parsed.patch -= 1;
        } else if parsed.minor > 0 {
            parsed.minor -= 1;
        } else if parsed.major > 0 {
            parsed.major -= 1;
        } else {
            return "0.0.0-0".to_string();
        }
        parsed.pre = semver::Prerelease::EMPTY;
        parsed.to_string()
    }

    fn newer_version() -> String {
        newer_than(VERSION)
    }

    fn older_version() -> String {
        older_than(VERSION)
    }

    #[test]
    fn version_helpers_are_strictly_newer_and_older() {
        assert_eq!(compare(&newer_version(), VERSION), Some(Ordering::Greater));
        assert_eq!(compare(&older_version(), VERSION), Some(Ordering::Less));
    }

    #[test]
    fn version_helpers_survive_a_zero_patch_and_prerelease_versions() {
        for version in ["0.5.0", "0.5.5-alpha.1", "0.5.5-alpha"] {
            assert_eq!(
                compare(&newer_than(version), version),
                Some(Ordering::Greater),
                "newer_than({version:?}) must stay strictly newer"
            );
            assert_eq!(
                compare(&older_than(version), version),
                Some(Ordering::Less),
                "older_than({version:?}) must stay strictly older"
            );
        }
    }

    #[test]
    fn gate_applies_when_the_control_plane_is_newer() {
        assert_eq!(gate(VERSION, &newer_version(), false).unwrap(), Gate::Apply);
        assert_eq!(gate(VERSION, &newer_version(), true).unwrap(), Gate::Apply);
    }

    #[test]
    fn gate_is_up_to_date_when_versions_match_even_with_force() {
        assert_eq!(gate(VERSION, VERSION, false).unwrap(), Gate::UpToDate);
        assert_eq!(gate(VERSION, VERSION, true).unwrap(), Gate::UpToDate);
    }

    #[test]
    fn gate_refuses_a_downgrade_without_force() {
        assert!(matches!(
            gate(VERSION, &older_version(), false),
            Err(UpdateError::DowngradeRequiresForce { .. })
        ));
    }

    #[test]
    fn gate_allows_a_downgrade_with_force() {
        assert_eq!(gate(VERSION, &older_version(), true).unwrap(), Gate::Apply);
    }

    #[test]
    fn gate_refuses_an_unparsable_control_plane_version() {
        assert!(matches!(
            gate(VERSION, "banana", false),
            Err(UpdateError::UnparsableVersion { .. })
        ));
    }

    #[cfg(unix)]
    fn write_version_script(dir: &Path, version: &str) -> Vec<u8> {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("bosun.staged");
        let content = format!("#!/bin/sh\necho 'bosun {version}'\n");
        std::fs::write(&path, &content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        content.into_bytes()
    }

    /// The archive a cargo-dist release ships for the current target: a
    /// `.tar.xz` holding the binary at `bosun-<target>/bosun`.
    #[cfg(unix)]
    fn release_archive(binary: &[u8]) -> Vec<u8> {
        use std::io::Cursor;

        let mut uncompressed = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut uncompressed);
            let mut header = tar::Header::new_gnu();
            header.set_size(binary.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    format!("bosun-{TARGET}/bosun"),
                    Cursor::new(binary),
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(&uncompressed), &mut compressed).unwrap();
        compressed
    }

    /// The archive file name a cargo-dist release ships for the current
    /// target.
    #[cfg(unix)]
    fn release_archive_name() -> String {
        format!("bosun-{TARGET}.tar.xz")
    }

    /// The two release assets a fetch needs for `version` on the current
    /// target, keyed by request path in cargo-dist's layout: the archive and
    /// its per-asset sha256.
    #[cfg(unix)]
    fn release_routes(version: &str, binary: &[u8]) -> HashMap<String, Vec<u8>> {
        let name = release_archive_name();
        let archive_path = format!("/v{version}/{name}");
        let archive = release_archive(binary);
        let digest = format!("{:x}", Sha256::digest(&archive));
        HashMap::from([
            (
                format!("{archive_path}.sha256"),
                format!("{digest} *{name}\n").into_bytes(),
            ),
            (archive_path, archive),
        ])
    }

    /// A control plane that answers its nodes route with `version` on the
    /// version header and records the paths it served. `None` serves no
    /// header. The body deliberately does not carry the version: the flow
    /// must read the header only.
    #[cfg(unix)]
    async fn control_plane(version: Option<&str>) -> (String, Arc<Mutex<Vec<String>>>) {
        #[derive(Clone)]
        struct ServerState {
            paths: Arc<Mutex<Vec<String>>>,
            version: Option<String>,
        }

        async fn serve_nodes(State(state): State<ServerState>, uri: Uri) -> Response {
            state.paths.lock().unwrap().push(uri.path().to_string());
            let mut response = Response::new(Body::from("the body is not read"));
            if let Some(version) = &state.version {
                response.headers_mut().insert(
                    bosun_common::types::X_BOSUN_VERSION,
                    HeaderValue::from_str(version).unwrap(),
                );
            }
            response
        }

        let paths = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/nodes", get(serve_nodes))
            .with_state(ServerState {
                paths: paths.clone(),
                version: version.map(str::to_string),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{}", addr.port()), paths)
    }

    /// A release feed serving `routes` and recording how many requests it
    /// answered. Everything unserved is a 404, so a request for a wrong asset
    /// or version fails the flow loudly.
    #[cfg(unix)]
    async fn release_feed(routes: HashMap<String, Vec<u8>>) -> (String, Arc<AtomicUsize>) {
        #[derive(Clone)]
        struct ServerState {
            routes: Arc<HashMap<String, Vec<u8>>>,
            requests: Arc<AtomicUsize>,
        }

        async fn serve_asset(
            State(state): State<ServerState>,
            AxumPath(path): AxumPath<String>,
        ) -> Response {
            state.requests.fetch_add(1, AtomicOrdering::Relaxed);
            let body = state
                .routes
                .get(&path)
                .or_else(|| state.routes.get(&format!("/{path}")))
                .cloned();
            match body {
                Some(body) => body.into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }

        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/{*path}", get(serve_asset))
            .with_state(ServerState {
                routes: Arc::new(routes),
                requests: requests.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{}", addr.port()), requests)
    }

    fn assert_target_untouched(dir: &tempfile::TempDir) {
        assert_eq!(std::fs::read(dir.path().join("bosun")).unwrap(), b"old");
        assert!(!dir.path().join("bosun.previous").exists());
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("bosun.update.tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_downloads_verifies_and_swaps() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let version = newer_version();
        let content = write_version_script(dir.path(), &version);
        let (cp_url, cp_paths) = control_plane(Some(&version)).await;
        let (feed_url, feed_requests) = release_feed(release_routes(&version, &content)).await;

        apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect("the update should apply");

        assert_eq!(std::fs::read(&target).unwrap(), content);
        assert_eq!(
            std::fs::read(dir.path().join("bosun.previous")).unwrap(),
            b"old"
        );
        assert_eq!(
            *cp_paths.lock().unwrap(),
            ["/nodes"],
            "the update must probe only the nodes route, never a /update/* endpoint"
        );
        assert_eq!(
            feed_requests.load(AtomicOrdering::Relaxed),
            2,
            "the update must fetch the checksum file and the archive from the release feed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_allows_a_forced_downgrade() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let version = older_version();
        let content = write_version_script(dir.path(), &version);
        let (cp_url, cp_paths) = control_plane(Some(&version)).await;
        let (feed_url, feed_requests) = release_feed(release_routes(&version, &content)).await;

        apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, true)
            .await
            .expect("--force should allow the downgrade");

        assert_eq!(std::fs::read(&target).unwrap(), content);
        assert_eq!(*cp_paths.lock().unwrap(), ["/nodes"]);
        assert_eq!(feed_requests.load(AtomicOrdering::Relaxed), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_refuses_a_downgrade_without_force() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let version = older_version();
        let (cp_url, cp_paths) = control_plane(Some(&version)).await;
        let (feed_url, feed_requests) = release_feed(HashMap::new()).await;

        let err = apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect_err("a downgrade without --force must be refused");

        assert!(matches!(err, UpdateError::DowngradeRequiresForce { .. }));
        assert_eq!(*cp_paths.lock().unwrap(), ["/nodes"]);
        assert_eq!(
            feed_requests.load(AtomicOrdering::Relaxed),
            0,
            "the gate must refuse before any release fetch starts"
        );
        assert_target_untouched(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_is_a_no_op_when_versions_match_even_without_a_release() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let (cp_url, cp_paths) = control_plane(Some(VERSION)).await;
        let (feed_url, feed_requests) = release_feed(HashMap::new()).await;

        apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect("equal versions must short-circuit before the release fetch");

        assert_eq!(*cp_paths.lock().unwrap(), ["/nodes"]);
        assert_eq!(
            feed_requests.load(AtomicOrdering::Relaxed),
            0,
            "an equal version must not fetch the release feed"
        );
        assert_target_untouched(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_reports_a_release_missing_from_the_feed() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let version = newer_version();
        let (cp_url, _cp_paths) = control_plane(Some(&version)).await;
        let (feed_url, feed_requests) = release_feed(HashMap::new()).await;

        let err = apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect_err("a version the release feed does not serve must fail the update");

        assert!(matches!(err, UpdateError::NoRelease { .. }));
        assert_eq!(
            feed_requests.load(AtomicOrdering::Relaxed),
            1,
            "the missing checksum must fail before the archive download"
        );
        assert_target_untouched(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_rejects_a_version_mismatch_and_cleans_up() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let claimed = newer_version();
        let content = write_version_script(dir.path(), VERSION);
        let (cp_url, _cp_paths) = control_plane(Some(&claimed)).await;
        let (feed_url, feed_requests) = release_feed(release_routes(&claimed, &content)).await;

        let err = apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect_err("the staged binary must report the control plane's version");

        assert!(matches!(err, UpdateError::VersionMismatch { .. }));
        assert_eq!(feed_requests.load(AtomicOrdering::Relaxed), 2);
        assert_target_untouched(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_refuses_an_unparsable_control_plane_version() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let (cp_url, _cp_paths) = control_plane(Some("banana")).await;
        let (feed_url, feed_requests) = release_feed(HashMap::new()).await;

        let err = apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect_err("an unparsable control-plane version must fail the update");

        assert!(matches!(err, UpdateError::UnparsableVersion { .. }));
        assert_eq!(
            feed_requests.load(AtomicOrdering::Relaxed),
            0,
            "the gate must refuse before any release fetch starts"
        );
        assert_target_untouched(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_fails_when_the_control_plane_announces_no_version() {
        use bosun_common::error::ErrorExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        let (cp_url, _cp_paths) = control_plane(None).await;
        let (feed_url, feed_requests) = release_feed(HashMap::new()).await;

        let err = apply_update(&reqwest::Client::new(), &cp_url, &feed_url, &target, false)
            .await
            .expect_err("a control plane without the version header must fail the update");

        assert!(matches!(err, UpdateError::Internal(_)));
        let chain = err.display_chain();
        assert!(
            chain.contains("did not announce its version"),
            "the error must name the missing header: {chain}"
        );
        assert_eq!(feed_requests.load(AtomicOrdering::Relaxed), 0);
        assert_target_untouched(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_update_fails_clearly_when_the_control_plane_is_unreachable() {
        use bosun_common::error::ErrorExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        std::fs::write(&target, b"old").unwrap();
        // Reserve a loopback port and release it, so the version probe has
        // nothing to reach.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cp_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener);

        let err = apply_update(
            &reqwest::Client::new(),
            &cp_url,
            "http://127.0.0.1:1",
            &target,
            false,
        )
        .await
        .expect_err("an unreachable control plane must fail the update");

        assert!(matches!(err, UpdateError::Internal(_)));
        let chain = err.display_chain();
        assert!(
            chain.contains("failed to reach the control plane")
                && chain.contains("must be reachable")
                && chain.contains("bosun update syncs this CLI to the control plane's version"),
            "the error must say why the control plane must be reachable: {chain}"
        );
        assert_target_untouched(&dir);
    }

    #[tokio::test]
    async fn finalize_installs_the_staged_binary_and_clears_the_markers_on_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.1.0");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        let markers_cleared = std::cell::Cell::new(false);

        finalize_pending_update(
            &PendingUpdate {
                staged: staged.clone(),
                target: target.clone(),
            },
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(10),
            || markers_cleared.set(true),
        )
        .await
        .expect("the staged binary should install");

        assert!(markers_cleared.get(), "the markers must be cleared");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join("bosun.exe.previous")).unwrap(),
            b"old"
        );
        assert!(!staged.exists(), "the staged file must be consumed");
    }

    #[tokio::test]
    async fn finalize_clears_the_markers_and_restores_when_the_staged_rename_fails() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.missing");
        std::fs::write(&target, b"old").unwrap();
        let markers_cleared = std::cell::Cell::new(false);

        let err = finalize_pending_update(
            &PendingUpdate {
                staged,
                target: target.clone(),
            },
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(10),
            || markers_cleared.set(true),
        )
        .await
        .expect_err("the missing staged file must fail the finalize");

        assert!(matches!(err, UpdateError::Internal(_)));
        assert!(
            markers_cleared.get(),
            "the markers must be cleared on failure"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old",
            "the previous binary must be restored over the failed rename"
        );
        assert!(!dir.path().join("bosun.exe.previous").exists());
    }

    #[tokio::test]
    async fn finalize_retries_the_move_until_the_old_image_lock_releases() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.1.0");
        std::fs::write(&staged, b"new").unwrap();
        let target_for_task = target.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            std::fs::write(&target_for_task, b"old").unwrap();
        });

        finalize_pending_update(
            &PendingUpdate {
                staged,
                target: target.clone(),
            },
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(20),
            || {},
        )
        .await
        .expect("the move should succeed once the lock releases");

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn pending_update_parses_both_markers() {
        let staged = OsStr::new(r"C:\bosun\bosun.update.tmp.1.0");
        let target = OsStr::new(r"C:\bosun\bosun.exe");
        let pending = pending_update(Some(staged), Some(target))
            .expect("both markers set should yield a pending update");
        assert_eq!(
            pending.staged,
            PathBuf::from(r"C:\bosun\bosun.update.tmp.1.0")
        );
        assert_eq!(pending.target, PathBuf::from(r"C:\bosun\bosun.exe"));
    }

    #[test]
    fn pending_update_is_none_when_a_marker_is_missing_or_empty() {
        let staged = OsStr::new("staged");
        let target = OsStr::new("target");
        assert!(pending_update(None, Some(target)).is_none());
        assert!(pending_update(Some(staged), None).is_none());
        assert!(pending_update(None, None).is_none());
        assert!(pending_update(Some(OsStr::new("")), Some(target)).is_none());
    }

    #[test]
    fn cli_update_markers_are_distinct_from_the_nodes() {
        // The CLI finalizer and the node self-update share one binary, and
        // the CLI finalize hook runs before `run_node`; a staged node boot
        // must never match the CLI markers. The node's marker names are
        // fixed.
        assert_ne!(UPDATE_MARKER, "BOSUN_UPDATE_STAGED");
        assert_ne!(UPDATE_TARGET, "BOSUN_UPDATE_TARGET");
    }

    /// A control plane that records each demanded-update request and answers
    /// with `status`, announcing `version` in the response header when set.
    async fn node_update_server(
        status: StatusCode,
        announced_version: Option<&str>,
    ) -> (u16, std::sync::Arc<Mutex<Vec<(String, Value)>>>) {
        use std::sync::Arc;

        #[derive(Clone)]
        struct ServerState {
            requests: Arc<Mutex<Vec<(String, Value)>>>,
            status: StatusCode,
            announced_version: Option<String>,
        }

        async fn serve_update(
            State(state): State<ServerState>,
            AxumPath(node): AxumPath<String>,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            state.requests.lock().unwrap().push((node, body));
            let mut response = axum::response::Response::new(axum::body::Body::empty());
            *response.status_mut() = state.status;
            if let Some(version) = &state.announced_version {
                response.headers_mut().insert(
                    bosun_common::types::X_BOSUN_VERSION,
                    axum::http::HeaderValue::from_str(version).unwrap(),
                );
            }
            response
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/nodes/{node}/update", post(serve_update))
            .with_state(ServerState {
                requests: requests.clone(),
                status,
                announced_version: announced_version.map(ToString::to_string),
            });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr.port(), requests)
    }

    #[tokio::test]
    async fn update_nodes_enqueues_an_update_for_each_named_node() {
        let (port, requests) = node_update_server(StatusCode::ACCEPTED, None).await;

        update_nodes(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            &["node-a".into(), "node-b".into()],
            true,
        )
        .await
        .expect("both updates should be accepted");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "node-a");
        assert_eq!(requests[0].1, serde_json::json!({ "force": true }));
        assert_eq!(requests[1].0, "node-b");
        assert_eq!(requests[1].1, serde_json::json!({ "force": true }));
    }

    #[tokio::test]
    async fn update_nodes_carries_force_false_for_a_plain_update() {
        let (port, requests) = node_update_server(StatusCode::ACCEPTED, None).await;

        update_nodes(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            &["node-a".into()],
            false,
        )
        .await
        .expect("the update should be accepted");

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].1, serde_json::json!({ "force": false }));
    }

    #[tokio::test]
    async fn update_nodes_fails_when_the_control_plane_rejects_a_node() {
        let (port, _requests) = node_update_server(StatusCode::BAD_REQUEST, None).await;

        let err = update_nodes(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            &["ghost".into()],
            false,
        )
        .await
        .expect_err("a rejected node must fail the update");

        assert!(
            err.to_string().contains("ghost"),
            "the error must name the rejected node: {err}"
        );
    }

    #[tokio::test]
    async fn update_nodes_does_not_print_the_update_notice() {
        // A control plane announcing a newer version would print the notice
        // on any other command; the S6 spec says `update` itself never does.
        let (port, _requests) =
            node_update_server(StatusCode::ACCEPTED, Some(&newer_version())).await;
        let previous = crate::UPDATE_NOTICE_PRINTED.load(AtomicOrdering::Relaxed);
        crate::UPDATE_NOTICE_PRINTED.store(false, AtomicOrdering::Relaxed);

        update_nodes(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            &["node-a".into()],
            true,
        )
        .await
        .expect("the update should be accepted");

        assert!(
            !crate::UPDATE_NOTICE_PRINTED.load(AtomicOrdering::Relaxed),
            "the update command itself must never print the update notice"
        );
        crate::UPDATE_NOTICE_PRINTED.store(previous, AtomicOrdering::Relaxed);
    }

    #[test]
    fn enqueued_message_points_at_bosun_nodes_for_the_outcome() {
        let message = enqueued_message("node-a");
        assert!(
            message.contains("update command queued for node node-a"),
            "{message}"
        );
        assert!(
            message.contains("bosun nodes"),
            "the message must say where the outcome appears: {message}"
        );
    }
}
