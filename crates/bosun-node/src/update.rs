use std::cmp::Ordering;
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(windows, test))]
use std::sync::OnceLock;
#[cfg(any(windows, test))]
use std::time::Duration;

use anyhow::Context;
use bosun_common::types::UpdateStatus;
use bosun_common::update::UpdateError;
#[cfg(any(windows, test))]
use bosun_common::update::copy_staged_to_target;
use bosun_common::update::fetch_release_artifact;
#[cfg(any(windows, test))]
use bosun_common::update::move_target_to_previous;
use bosun_common::update::previous_path;
use bosun_common::update::reported_version;
#[cfg(unix)]
use bosun_common::update::swap_binary;
use bosun_common::update::verify_binary_version;
use bosun_common::version::compare;
use thiserror::Error;
use tracing::info;
use tracing::instrument;

/// Env var naming the staged file a Windows update process was spawned from.
#[cfg(windows)]
const UPDATE_MARKER: &str = "BOSUN_UPDATE_STAGED";
/// Env var naming the canonical path the staged file must be installed at.
#[cfg(windows)]
const UPDATE_TARGET: &str = "BOSUN_UPDATE_TARGET";

/// Env var naming the canonical path a Windows rollback finalizer must
/// restore the previous binary at.
#[cfg(windows)]
const ROLLBACK_TARGET: &str = "BOSUN_ROLLBACK";
/// Env var naming the previous binary a Windows rollback finalizer must
/// restore over the canonical path.
#[cfg(windows)]
const ROLLBACK_SOURCE: &str = "BOSUN_ROLLBACK_SOURCE";
/// Env var marking a rollback finalizer that must swap the previous binary
/// back without restarting into it, because no `--config` was given.
#[cfg(windows)]
const ROLLBACK_SWAP_ONLY: &str = "BOSUN_ROLLBACK_SWAP_ONLY";

/// How long the new process waits for the old process to release its image
/// file before giving up on the swap.
#[cfg(windows)]
const UPDATE_LOCK_WAIT: Duration = Duration::from_secs(10);
/// How long between swap retries while the old process's lock is pending.
#[cfg(windows)]
const UPDATE_LOCK_RETRY: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum RollbackError {
    #[error("no previous binary at {path}; nothing to roll back to")]
    NoPreviousBinary { path: PathBuf },

    #[error("the previous binary at {path} is not a valid bosun binary: {reason}")]
    InvalidPreviousBinary { path: PathBuf, reason: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// Whether the node should fetch and apply the released binary at the control
/// plane's version: updates enabled and the control plane strictly ahead of
/// the node. The node cannot know a release exists until it fetches, so
/// availability plays no part in the decision.
pub fn should_update(node_version: &str, cp_version: &str, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    matches!(compare(cp_version, node_version), Some(Ordering::Greater))
}

/// The status the node reports in its next poll: the last update attempt's
/// outcome while one is pending, Updating while a task runs, otherwise the
/// steady state from the versions and the update flag. A disabled node
/// reports Disabled even when ahead, and an idle node behind the control
/// plane reports UpToDate because the poll loop starts an update from the
/// very response that shows it behind. A fetch that finds no release reports
/// NoRelease through the failure outcome.
pub fn update_status(
    node_version: &str,
    cp_version: &str,
    enabled: bool,
    in_flight: bool,
    outcome: Option<&UpdateStatus>,
) -> UpdateStatus {
    if let Some(outcome) = outcome {
        return outcome.clone();
    }
    if in_flight {
        return UpdateStatus::Updating;
    }
    if !enabled {
        return UpdateStatus::Disabled;
    }
    match compare(cp_version, node_version) {
        Some(Ordering::Less) => UpdateStatus::Ahead,
        Some(Ordering::Equal) | None => UpdateStatus::UpToDate,
        Some(Ordering::Greater) => UpdateStatus::UpToDate,
    }
}

/// Whether the control plane's version may be installed: equal or newer
/// versions always may, an older one only when the downgrade is allowed.
fn gate_downgrade(
    node_version: &str,
    cp_version: &str,
    allow_downgrade: bool,
) -> Result<(), UpdateError> {
    if !allow_downgrade && matches!(compare(cp_version, node_version), Some(Ordering::Less)) {
        return Err(UpdateError::DowngradeRequiresForce {
            cli_version: node_version.to_string(),
            cp_version: cp_version.to_string(),
        });
    }
    Ok(())
}

/// The status a node reports for a failed update attempt. Internal failures
/// carry the top-level error message so the control plane shows why the
/// attempt failed.
pub fn status_from_error(error: &UpdateError) -> UpdateStatus {
    match error {
        UpdateError::SizeMismatch { .. } => UpdateStatus::Failed("size mismatch".into()),
        UpdateError::ChecksumMismatch => UpdateStatus::Failed("checksum mismatch".into()),
        UpdateError::VersionMismatch { .. } => UpdateStatus::Failed("version mismatch".into()),
        UpdateError::DowngradeRequiresForce { .. } => UpdateStatus::Ahead,
        UpdateError::UnparsableVersion { .. } => {
            UpdateStatus::Failed("invalid control plane version".into())
        }
        UpdateError::NoRelease { .. } => UpdateStatus::NoRelease,
        UpdateError::MalformedChecksum { .. } => {
            UpdateStatus::Failed("invalid release checksum file".into())
        }
        UpdateError::ExtractionFailed { .. } => {
            UpdateStatus::Failed("failed to extract the release archive".into())
        }
        UpdateError::Internal(error) => UpdateStatus::Failed(error.to_string()),
    }
}

/// The canonical install path recorded by a successful update in this
/// process, if any. After a Windows update the process keeps running from its
/// staged temp file, so `std::env::current_exe()` no longer names the
/// canonical binary; this cell keeps the canonical path for the next update.
/// On Unix the process always execs from the canonical path, so the cell is
/// never populated there and this is a no-op.
#[cfg(any(windows, test))]
static RECORDED_TARGET: OnceLock<PathBuf> = OnceLock::new();

#[cfg(any(windows, test))]
fn recorded_target() -> Option<&'static Path> {
    RECORDED_TARGET.get().map(|target| target.as_path())
}

/// On Unix the running binary's path is already canonical.
#[cfg(not(any(windows, test)))]
fn recorded_target() -> Option<&'static Path> {
    None
}

/// The path the next update must be installed at: the canonical path recorded
/// by an earlier update in this process when present, otherwise the running
/// binary's path. A Windows process runs from its staged temp file after the
/// first update, so without the recorded path it would keep updating the temp
/// file instead of the canonical binary.
fn update_target(recorded: Option<&Path>, current_exe: &Path) -> PathBuf {
    match recorded {
        Some(recorded) => recorded.to_path_buf(),
        None => current_exe.to_path_buf(),
    }
}

/// Records the canonical install path after a successful swap so successive
/// updates within one process lifetime keep targeting it.
#[cfg(any(windows, test))]
fn record_target(target: &Path) {
    let _ = RECORDED_TARGET.set(target.to_path_buf());
}

/// Downloads, verifies, and restarts into the released binary at the control
/// plane's announced version, fetched from the release feed at
/// `update_base_url`. A control plane ahead of the node applies regardless; a
/// lower version is refused unless `allow_downgrade` is set. Returns only on
/// failure: on Unix the swap and execve happen in this process, on Windows
/// the staged binary is spawned and installs itself after this process exits.
#[instrument(skip_all)]
pub async fn apply(
    client: &reqwest::Client,
    update_base_url: &str,
    expected_version: &str,
    allow_downgrade: bool,
) -> Result<(), UpdateError> {
    gate_downgrade(
        bosun_common::version::VERSION,
        expected_version,
        allow_downgrade,
    )?;
    let current = std::env::current_exe().context("failed to find the running binary")?;
    let target = update_target(recorded_target(), &current);
    let dir = target
        .parent()
        .context("the update target has no parent directory")?;
    info!(
        target = %bosun_common::target::TARGET,
        version = %expected_version,
        "node update: downloading the release archive"
    );
    let staged = fetch_release_artifact(
        client,
        update_base_url,
        expected_version,
        bosun_common::target::TARGET,
        dir,
    )
    .await?;
    let outcome = async {
        verify_binary_version(&staged, expected_version).await?;
        launch_update(&target, &staged)?;
        Ok::<(), UpdateError>(())
    }
    .await;
    if outcome.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    outcome
}

/// Installs the staged binary and restarts into it. On Unix the running image
/// can be renamed, so the swap happens here before execve. On Windows the
/// running image is locked, so the staged binary is spawned and swaps itself
/// in after this process exits.
#[cfg(unix)]
fn launch_update(current: &Path, staged: &Path) -> Result<(), UpdateError> {
    swap_binary(current, staged)?;
    info!("node update: binary swapped, restarting");
    restart(current).context("failed to restart into the updated binary")?;
    Ok(())
}

#[cfg(windows)]
fn launch_update(target: &Path, staged: &Path) -> Result<(), UpdateError> {
    info!(
        target = %target.display(),
        "node update: launching the staged binary"
    );
    restart(staged, target).context("failed to restart into the updated binary")?;
    Ok(())
}

/// Replaces the running process with the new binary, same PID.
#[cfg(unix)]
fn restart(exe: &Path) -> std::io::Result<()> {
    restart_with_args(exe, std::env::args_os().skip(1))
}

/// Spawns the staged binary with the update marker, so it installs itself at
/// `target` after this process exits, then exits.
#[cfg(windows)]
fn restart(staged: &Path, target: &Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new(staged);
    command.args(std::env::args_os().skip(1));
    command.env(UPDATE_MARKER, staged);
    command.env(UPDATE_TARGET, target);
    command.spawn()?;
    std::process::exit(0)
}

/// The staged binary and the canonical path it must be installed at, parsed
/// from the update marker env vars. None on a normal boot.
#[cfg(any(windows, test))]
fn pending_update(
    staged: Option<&std::ffi::OsStr>,
    target: Option<&std::ffi::OsStr>,
) -> Option<PendingUpdate> {
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

/// The pending Windows update: the staged binary this process runs from and
/// the canonical path it must be installed at.
#[cfg(any(windows, test))]
struct PendingUpdate {
    staged: PathBuf,
    target: PathBuf,
}

/// Installs the staged update this process was spawned as: moves the
/// installed binary aside and copies the staged binary over it. No-op on a
/// normal boot. Runs in the new process, after the old one has exited,
/// because the old process's running image is locked.
#[cfg(windows)]
pub async fn finalize_staged_update() -> Result<(), UpdateError> {
    let Some(pending) = pending_update(
        std::env::var_os(UPDATE_MARKER).as_deref(),
        std::env::var_os(UPDATE_TARGET).as_deref(),
    ) else {
        return Ok(());
    };
    finalize_pending_update(
        &pending,
        UPDATE_LOCK_WAIT,
        UPDATE_LOCK_RETRY,
        clear_update_marker,
    )
    .await
}

/// Installs the pending update: moves the installed binary aside and copies
/// the staged binary over it. The old process's image file stays locked
/// briefly after it exits, so the move is retried for up to `wait` at
/// `retry` intervals. Clears the update markers through `clear` on both
/// success and failure so child processes do not inherit them, and records
/// the canonical target after a successful swap so later updates in this
/// process keep installing at the canonical path.
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
        copy_staged_to_target(&pending.target, &pending.staged)
    }
    .await;

    clear();
    if outcome.is_ok() {
        record_target(&pending.target);
    }
    outcome?;
    info!(
        target = %pending.target.display(),
        "node update: staged binary installed"
    );
    Ok(())
}

/// Removes the update marker, so a later restart of this process does not
/// replay the swap.
#[cfg(windows)]
fn clear_update_marker() {
    // SAFETY: the marker env vars are only read at startup, before this
    // point, and no other thread touches them.
    unsafe {
        std::env::remove_var(UPDATE_MARKER);
        std::env::remove_var(UPDATE_TARGET);
    }
}

/// The message printed when a rollback restored the previous binary but did
/// not restart the node, because no `--config` was given.
const ROLLBACK_SWAP_ONLY_MESSAGE: &str =
    "the previous binary was restored; start the node manually with `bosun node --config <path>`";

/// Swaps the `.previous` binary back into the canonical path and restarts
/// into it, unless `swap_only` is set: a rollback started without a
/// `--config` swaps and stops, printing how to start the node manually. On
/// Unix the swap and execve happen in this process; on Windows the finalizer
/// spawned here restores the previous binary after this process exits.
/// Returns on failure, and on a swap-only rollback.
#[instrument(skip_all)]
pub async fn rollback(swap_only: bool) -> Result<(), RollbackError> {
    let current = std::env::current_exe().context("failed to find the running binary")?;
    let target = update_target(recorded_target(), &current);
    let previous = rollback_source(&target).await?;
    info!(
        target = %target.display(),
        previous = %previous.display(),
        "node rollback: reverting to the previous binary"
    );
    launch_rollback(&target, &previous, swap_only)?;
    Ok(())
}

/// The validated `.previous` path a rollback can restore: the file exists and
/// runs as a bosun binary.
async fn rollback_source(target: &Path) -> Result<PathBuf, RollbackError> {
    let previous = previous_path(target);
    if !previous.exists() {
        return Err(RollbackError::NoPreviousBinary { path: previous });
    }
    if let Err(error) = reported_version(&previous).await {
        return Err(RollbackError::InvalidPreviousBinary {
            path: previous,
            reason: error.to_string(),
        });
    }
    Ok(previous)
}

/// The process args without the `--rollback` flag, so the process a rollback
/// restarts into boots normally instead of rolling back again.
fn env_args_without_rollback() -> Vec<std::ffi::OsString> {
    args_without_rollback(std::env::args_os().skip(1))
}

fn args_without_rollback(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    args.filter(|arg| !arg.to_string_lossy().starts_with("--rollback"))
        .collect()
}

/// Moves the previous binary over the canonical path, consuming it. The
/// running image can be renamed on Unix, and the canonical path already holds
/// the binary being replaced, so a plain rename suffices.
fn swap_previous_to_target(previous: &Path, target: &Path) -> Result<(), RollbackError> {
    std::fs::rename(previous, target).with_context(|| {
        format!(
            "failed to move the previous binary {} into {}",
            previous.display(),
            target.display()
        )
    })?;
    Ok(())
}

/// Moves the installed binary to `<name>.rolled`, freeing the canonical path
/// for the previous binary. The old process's image file stays locked briefly
/// after it exits, so callers retry this rename. A leftover `.rolled` file is
/// harmless; do not delete it, the file may be a running image.
#[cfg(any(windows, test))]
fn move_target_to_rolled(target: &Path) -> Result<(), RollbackError> {
    let rolled = rolled_path(target);
    std::fs::rename(target, &rolled).with_context(|| {
        format!(
            "failed to move the installed binary to {}",
            rolled.display()
        )
    })?;
    Ok(())
}

#[cfg(any(windows, test))]
fn rolled_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(".rolled");
    target.with_file_name(name)
}

/// Installs the previous binary: swap, then execve from the canonical path
/// without the `--rollback` flag. A swap-only rollback prints the
/// manual-start message and stops after the swap instead of restarting.
#[cfg(unix)]
fn launch_rollback(target: &Path, previous: &Path, swap_only: bool) -> Result<(), RollbackError> {
    swap_previous_to_target(previous, target)?;
    if swap_only {
        println!("{ROLLBACK_SWAP_ONLY_MESSAGE}");
        return Ok(());
    }
    info!("node rollback: previous binary restored, restarting");
    restart_after_rollback(target).with_context(|| {
        format!(
            "the previous binary was restored at {} but the restart failed; start it manually — the previous binary was consumed, so retrying is not possible",
            target.display()
        )
    })?;
    Ok(())
}

/// Replaces the running process with the reverted binary, same PID.
#[cfg(unix)]
fn restart_after_rollback(exe: &Path) -> std::io::Result<()> {
    restart_with_args(exe, env_args_without_rollback())
}

/// Spawns the restored binary from the canonical path without the
/// `--rollback` flag and exits, so it boots normally.
#[cfg(windows)]
fn restart_after_rollback(exe: &Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new(exe);
    command.args(env_args_without_rollback());
    command.spawn()?;
    std::process::exit(0)
}

#[cfg(unix)]
fn restart_with_args(
    exe: &Path,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(exe);
    command.args(args);
    Err(command.exec())
}

/// Copies the running S5+ binary to a finalizer path and spawns it with the
/// rollback markers, so the finalizer restores the previous binary after this
/// process exits. The previous binary cannot do the swap itself: on the first
/// rollback after this feature ships, it predates the markers. A swap-only
/// rollback also sets the swap-only marker, so the finalizer stops after the
/// swap instead of restarting.
#[cfg(windows)]
fn launch_rollback(target: &Path, previous: &Path, swap_only: bool) -> Result<(), RollbackError> {
    info!(
        target = %target.display(),
        previous = %previous.display(),
        "node rollback: launching the rollback finalizer"
    );
    spawn_rollback_finalizer(target, previous, swap_only)
        .context("failed to launch the rollback finalizer")?;
    Ok(())
}

#[cfg(windows)]
fn spawn_rollback_finalizer(
    target: &Path,
    previous: &Path,
    swap_only: bool,
) -> std::io::Result<()> {
    let finalizer = rollback_finalizer_path(target);
    std::fs::copy(std::env::current_exe()?, &finalizer)?;
    let mut command = std::process::Command::new(&finalizer);
    command.args(env_args_without_rollback());
    command.env(ROLLBACK_TARGET, target);
    command.env(ROLLBACK_SOURCE, previous);
    if swap_only {
        command.env(ROLLBACK_SWAP_ONLY, "1");
    }
    command.spawn()?;
    std::process::exit(0)
}

/// The path the rollback finalizer is copied to: the running binary next to
/// the canonical target, under a name no running image uses.
#[cfg(windows)]
fn rollback_finalizer_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bosun.exe");
    let stem = file_name.strip_suffix(".exe").unwrap_or(file_name);
    target.with_file_name(format!("{stem}.rollback.exe"))
}

/// Finalizes the rollback this process was spawned as: restores the previous
/// binary over the canonical path and restarts into it, or prints the
/// manual-start message and exits when the swap-only marker is set. No-op on
/// a normal boot. Runs in the finalizer process, after the old one has
/// exited, because the old process's running image is locked. Never returns
/// on success: the restored binary is spawned from the canonical path (or
/// the swap-only message is printed) and this process exits.
#[cfg(windows)]
pub async fn finalize_rollback() -> Result<(), RollbackError> {
    let Some(pending) = pending_rollback(
        std::env::var_os(ROLLBACK_TARGET).as_deref(),
        std::env::var_os(ROLLBACK_SOURCE).as_deref(),
        std::env::var_os(ROLLBACK_SWAP_ONLY).as_deref(),
    ) else {
        return Ok(());
    };
    finalize_pending_rollback(
        &pending,
        UPDATE_LOCK_WAIT,
        UPDATE_LOCK_RETRY,
        clear_rollback_marker,
        record_target,
    )
    .await?;
    if pending.swap_only {
        println!("{ROLLBACK_SWAP_ONLY_MESSAGE}");
        std::process::exit(0);
    }
    restart_after_rollback(&pending.target)
        .context("failed to restart into the restored binary")?;
    Ok(())
}

/// The pending Windows rollback: the previous binary to restore and the
/// canonical path to restore it at, plus whether the finalizer must stop
/// after the swap instead of restarting. Parsed from the rollback marker env
/// vars. None on a normal boot.
#[cfg(any(windows, test))]
struct PendingRollback {
    source: PathBuf,
    target: PathBuf,
    swap_only: bool,
}

#[cfg(any(windows, test))]
fn pending_rollback(
    target: Option<&std::ffi::OsStr>,
    source: Option<&std::ffi::OsStr>,
    swap_only: Option<&std::ffi::OsStr>,
) -> Option<PendingRollback> {
    let (target, source) = match (target, source) {
        (Some(target), Some(source)) if !target.is_empty() && !source.is_empty() => {
            (target, source)
        }
        _ => return None,
    };
    Some(PendingRollback {
        source: PathBuf::from(source),
        target: PathBuf::from(target),
        swap_only: swap_only.is_some_and(|value| !value.is_empty()),
    })
}

/// Restores the previous binary over the canonical path: moves the installed
/// binary aside and moves the previous binary into its place, consuming it.
/// The old process's image file stays locked briefly after it exits, so the
/// move aside is retried for up to `wait` at `retry` intervals. Refuses when
/// the previous binary is missing, leaving the target untouched. Clears the
/// rollback markers through `clear` and records the canonical target through
/// `record` on both success and failure: the process keeps running from the
/// finalizer path, so a later update must still target the canonical binary.
#[cfg(any(windows, test))]
async fn finalize_pending_rollback(
    pending: &PendingRollback,
    wait: Duration,
    retry: Duration,
    clear: impl FnOnce(),
    record: impl FnOnce(&Path),
) -> Result<(), RollbackError> {
    record(&pending.target);
    if !pending.source.exists() {
        clear();
        return Err(RollbackError::Internal(anyhow::anyhow!(
            "the previous binary {} is missing",
            pending.source.display()
        )));
    }
    let outcome = async {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match move_target_to_rolled(&pending.target) {
                Ok(()) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(retry).await;
                }
                Err(error) => return Err(error),
            }
        }
        swap_previous_to_target(&pending.source, &pending.target)
    }
    .await;

    clear();
    outcome?;
    info!(
        target = %pending.target.display(),
        "node rollback: previous binary restored at the canonical path"
    );
    Ok(())
}

/// Whether this process was spawned as a rollback finalizer: one of the
/// rollback markers is set. The markers are only the parent-to-finalizer
/// handoff, not a guard against concurrent rollbacks; a second rollback is
/// self-limiting because the previous binary is validated and consumed by
/// the swap.
#[cfg(windows)]
pub fn rollback_marker_is_set() -> bool {
    std::env::var_os(ROLLBACK_TARGET).is_some_and(|value| !value.is_empty())
        || std::env::var_os(ROLLBACK_SOURCE).is_some_and(|value| !value.is_empty())
}

/// Removes the rollback markers, so a later restart of this process does not
/// replay the swap.
#[cfg(windows)]
fn clear_rollback_marker() {
    // SAFETY: the rollback marker env vars are only read at startup, before
    // this point, and no other thread touches them.
    unsafe {
        std::env::remove_var(ROLLBACK_TARGET);
        std::env::remove_var(ROLLBACK_SOURCE);
        std::env::remove_var(ROLLBACK_SWAP_ONLY);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tempfile::tempdir;

    use super::*;
    use crate::test_feed::ArchiveBehavior;
    use crate::test_feed::serve as serve_feed;

    #[test]
    fn should_update_when_behind_and_enabled() {
        assert!(should_update("0.5.4", "0.5.5", true));
    }

    #[test]
    fn no_update_when_versions_match() {
        assert!(!should_update("0.5.5", "0.5.5", true));
    }

    #[test]
    fn no_update_when_the_node_is_ahead() {
        assert!(!should_update("0.6.0", "0.5.5", true));
    }

    #[test]
    fn no_update_when_updates_are_disabled() {
        assert!(!should_update("0.5.4", "0.5.5", false));
    }

    #[test]
    fn no_update_when_versions_do_not_parse() {
        assert!(!should_update("banana", "0.5.5", true));
        assert!(!should_update("0.5.4", "", true));
    }

    #[test]
    fn gate_allows_equal_or_newer_control_plane_versions() {
        assert_eq!(gate_downgrade("0.5.5", "0.5.5", false).unwrap(), ());
        assert_eq!(gate_downgrade("0.5.5", "0.5.6", false).unwrap(), ());
    }

    #[test]
    fn gate_refuses_a_downgrade_unless_allowed() {
        assert!(matches!(
            gate_downgrade("0.5.6", "0.5.5", false),
            Err(UpdateError::DowngradeRequiresForce { .. })
        ));
        assert_eq!(gate_downgrade("0.5.6", "0.5.5", true).unwrap(), ());
    }

    #[test]
    fn gate_passes_when_the_control_plane_version_does_not_parse() {
        assert_eq!(gate_downgrade("0.5.5", "banana", false).unwrap(), ());
    }

    #[tokio::test]
    async fn apply_refuses_a_downgrade_without_allow_before_downloading() {
        let version = bosun_test_support::older_than(bosun_common::version::VERSION);
        let feed = serve_feed(&version, ArchiveBehavior::Mismatch).await;
        let base_url = feed.base_url();

        let err = apply(&reqwest::Client::new(), &base_url, &version, false)
            .await
            .expect_err("a downgrade without allow must be refused");

        assert!(matches!(err, UpdateError::DowngradeRequiresForce { .. }));
        assert_eq!(
            feed.requests(),
            0,
            "the gate must refuse before any fetch starts"
        );
    }

    #[tokio::test]
    async fn apply_allows_a_forced_downgrade_to_download() {
        let version = bosun_test_support::older_than(bosun_common::version::VERSION);
        let feed = serve_feed(&version, ArchiveBehavior::Mismatch).await;
        let base_url = feed.base_url();

        let err = apply(&reqwest::Client::new(), &base_url, &version, true)
            .await
            .expect_err("the mismatched release must fail the download");

        assert!(matches!(err, UpdateError::ChecksumMismatch));
        assert_eq!(
            feed.requests(),
            2,
            "a forced downgrade must get past the gate and fetch the archive and its checksum"
        );
    }

    #[tokio::test]
    async fn apply_downloads_a_newer_version_without_allow() {
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let feed = serve_feed(&version, ArchiveBehavior::Mismatch).await;
        let base_url = feed.base_url();

        let err = apply(&reqwest::Client::new(), &base_url, &version, false)
            .await
            .expect_err("the mismatched release must fail the download");

        assert!(matches!(err, UpdateError::ChecksumMismatch));
        assert_eq!(
            feed.requests(),
            2,
            "an upgrade must not be blocked by the downgrade gate"
        );
    }

    #[test]
    fn status_reports_the_last_outcome_over_everything_else() {
        let outcome = UpdateStatus::Failed("checksum mismatch".into());
        assert_eq!(
            update_status("0.5.4", "0.5.5", true, true, Some(&outcome)),
            outcome
        );
    }

    #[test]
    fn status_is_updating_while_a_task_is_in_flight() {
        assert_eq!(
            update_status("0.5.4", "0.5.5", true, true, None),
            UpdateStatus::Updating
        );
    }

    #[test]
    fn status_is_disabled_when_updates_are_off_even_when_ahead() {
        assert_eq!(
            update_status("0.5.5", "0.5.4", false, false, None),
            UpdateStatus::Disabled
        );
        assert_eq!(
            update_status("0.6.0", "0.5.5", false, false, None),
            UpdateStatus::Disabled
        );
    }

    #[test]
    fn status_is_ahead_when_the_node_is_newer() {
        assert_eq!(
            update_status("0.6.0", "0.5.5", true, false, None),
            UpdateStatus::Ahead
        );
    }

    #[test]
    fn status_is_up_to_date_when_behind_and_idle_or_equal() {
        assert_eq!(
            update_status("0.5.4", "0.5.5", true, false, None),
            UpdateStatus::UpToDate
        );
        assert_eq!(
            update_status("0.5.5", "0.5.5", true, false, None),
            UpdateStatus::UpToDate
        );
    }

    #[test]
    fn status_is_up_to_date_when_versions_do_not_parse() {
        assert_eq!(
            update_status("banana", "0.5.5", true, false, None),
            UpdateStatus::UpToDate
        );
        assert_eq!(
            update_status("0.5.5", "", true, false, None),
            UpdateStatus::UpToDate
        );
    }

    #[test]
    fn status_from_error_maps_each_variant() {
        assert_eq!(
            status_from_error(&UpdateError::SizeMismatch {
                expected: 10,
                actual: 9,
            }),
            UpdateStatus::Failed("size mismatch".into())
        );
        assert_eq!(
            status_from_error(&UpdateError::ChecksumMismatch),
            UpdateStatus::Failed("checksum mismatch".into())
        );
        assert_eq!(
            status_from_error(&UpdateError::VersionMismatch {
                expected: "0.5.5".into(),
                actual: "0.5.4".into(),
            }),
            UpdateStatus::Failed("version mismatch".into())
        );
        assert_eq!(
            status_from_error(&UpdateError::DowngradeRequiresForce {
                cli_version: "0.5.6".into(),
                cp_version: "0.5.5".into(),
            }),
            UpdateStatus::Ahead
        );
        assert_eq!(
            status_from_error(&UpdateError::UnparsableVersion {
                version: "banana".into(),
            }),
            UpdateStatus::Failed("invalid control plane version".into())
        );
        assert_eq!(
            status_from_error(&UpdateError::NoRelease {
                version: "0.6.0".into(),
                url: "https://example.invalid/v0.6.0/archive".into(),
            }),
            UpdateStatus::NoRelease
        );
        assert_eq!(
            status_from_error(&UpdateError::MalformedChecksum {
                url: "https://example.invalid/checksum".into(),
            }),
            UpdateStatus::Failed("invalid release checksum file".into())
        );
        assert_eq!(
            status_from_error(&UpdateError::ExtractionFailed {
                reason: "corrupt archive".into(),
            }),
            UpdateStatus::Failed("failed to extract the release archive".into())
        );
        assert!(matches!(
            status_from_error(&UpdateError::Internal(anyhow::anyhow!("boom"))),
            UpdateStatus::Failed(_)
        ));
    }

    #[test]
    fn update_target_uses_the_recorded_canonical_path() {
        let recorded = Path::new(r"C:\bosun\bosun.exe");
        let current = Path::new(r"C:\bosun\bosun.update.tmp.1.0");
        assert_eq!(
            update_target(Some(recorded), current),
            PathBuf::from(r"C:\bosun\bosun.exe")
        );
    }

    #[test]
    fn update_target_falls_back_to_the_running_binary() {
        let current = Path::new(r"C:\bosun\bosun.exe");
        assert_eq!(
            update_target(None, current),
            PathBuf::from(r"C:\bosun\bosun.exe")
        );
    }

    #[tokio::test]
    async fn finalize_installs_the_staged_binary_and_clears_the_markers_on_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.1.0");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        let markers_cleared = Cell::new(false);

        finalize_pending_update(
            &PendingUpdate {
                staged: staged.clone(),
                target: target.clone(),
            },
            Duration::from_secs(1),
            Duration::from_millis(10),
            || markers_cleared.set(true),
        )
        .await
        .expect("the staged binary should install");

        assert!(
            markers_cleared.get(),
            "the markers must be cleared on success"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join("bosun.exe.previous")).unwrap(),
            b"old"
        );
        assert!(
            staged.exists(),
            "the staged file is the running image and must not be deleted"
        );
    }

    #[tokio::test]
    async fn finalize_clears_the_markers_and_restores_when_the_copy_fails() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.missing");
        std::fs::write(&target, b"old").unwrap();
        let markers_cleared = Cell::new(false);

        let err = finalize_pending_update(
            &PendingUpdate {
                staged,
                target: target.clone(),
            },
            Duration::from_secs(1),
            Duration::from_millis(10),
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
            "the previous binary must be restored over the failed copy"
        );
        assert!(
            !dir.path().join("bosun.exe.previous").exists(),
            "the restore must consume the previous binary"
        );
    }

    #[tokio::test]
    async fn finalize_retries_the_move_until_the_old_image_lock_releases() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.1.0");
        std::fs::write(&staged, b"new").unwrap();
        let target_for_task = target.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(&target_for_task, b"old").unwrap();
        });

        finalize_pending_update(
            &PendingUpdate {
                staged,
                target: target.clone(),
            },
            Duration::from_secs(2),
            Duration::from_millis(20),
            || {},
        )
        .await
        .expect("the move should succeed once the lock releases");

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn pending_update_parses_both_markers() {
        let staged = std::ffi::OsStr::new(r"C:\bosun\bosun.update.tmp.1.0");
        let target = std::ffi::OsStr::new(r"C:\bosun\bosun.exe");
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
        let staged = std::ffi::OsStr::new("staged");
        let target = std::ffi::OsStr::new("target");
        assert!(pending_update(None, Some(target)).is_none());
        assert!(pending_update(Some(staged), None).is_none());
        assert!(pending_update(None, None).is_none());
        assert!(pending_update(Some(std::ffi::OsStr::new("")), Some(target)).is_none());
    }

    #[test]
    fn args_without_rollback_drops_the_flag_and_keeps_the_rest() {
        let kept = args_without_rollback(
            [
                "bosun",
                "node",
                "--rollback",
                "--config",
                "x.toml",
                "--rollback=true",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        );
        assert_eq!(
            kept,
            ["bosun", "node", "--config", "x.toml"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rollback_source_accepts_a_valid_previous_binary() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        let previous = dir.path().join("bosun.previous");
        std::fs::write(&previous, "#!/bin/sh\necho 'bosun 0.5.4'\n").unwrap();
        std::fs::set_permissions(&previous, std::fs::Permissions::from_mode(0o755)).unwrap();

        let source = rollback_source(&target)
            .await
            .expect("the previous version script should be accepted as a rollback source");

        assert_eq!(source, previous);
    }

    #[tokio::test]
    async fn rollback_source_refuses_when_there_is_no_previous_binary() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");

        let err = rollback_source(&target)
            .await
            .expect_err("a missing previous binary must fail the rollback");

        assert!(matches!(err, RollbackError::NoPreviousBinary { .. }));
    }

    #[tokio::test]
    async fn rollback_source_refuses_a_previous_that_is_not_a_bosun_binary() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        let previous = dir.path().join("bosun.previous");
        std::fs::write(&previous, b"not a binary").unwrap();

        let err = rollback_source(&target)
            .await
            .expect_err("a previous file that is not a bosun binary must be refused");

        assert!(matches!(err, RollbackError::InvalidPreviousBinary { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rollback_swap_moves_the_previous_binary_over_the_target() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        let previous = dir.path().join("bosun.previous");
        std::fs::write(&target, b"bad").unwrap();
        std::fs::write(&previous, b"good").unwrap();

        swap_previous_to_target(&previous, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"good");
        assert!(
            !previous.exists(),
            "the previous file must be consumed by the swap"
        );
    }

    #[tokio::test]
    async fn rollback_finalize_swaps_the_previous_binary_over_the_target_and_clears_the_markers() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let previous = dir.path().join("bosun.exe.previous");
        std::fs::write(&target, b"bad").unwrap();
        std::fs::write(&previous, b"good").unwrap();
        let markers_cleared = Cell::new(false);
        let recorded = Cell::new(None);

        finalize_pending_rollback(
            &PendingRollback {
                source: previous.clone(),
                target: target.clone(),
                swap_only: false,
            },
            Duration::from_secs(1),
            Duration::from_millis(10),
            || markers_cleared.set(true),
            |target| recorded.set(Some(target.to_path_buf())),
        )
        .await
        .expect("the previous binary should be swapped over the target");

        assert!(markers_cleared.get(), "the markers must be cleared");
        assert_eq!(
            recorded.take().as_deref(),
            Some(target.as_path()),
            "the canonical target must be recorded after a successful swap"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"good");
        assert_eq!(
            std::fs::read(dir.path().join("bosun.exe.rolled")).unwrap(),
            b"bad",
            "the installed binary must be moved aside before the swap"
        );
        assert!(
            !previous.exists(),
            "the previous file must be consumed by the swap"
        );
    }

    #[tokio::test]
    async fn rollback_finalize_clears_the_markers_and_errors_when_the_previous_is_missing() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        std::fs::write(&target, b"bad").unwrap();
        let markers_cleared = Cell::new(false);
        let recorded = Cell::new(None);

        let err = finalize_pending_rollback(
            &PendingRollback {
                source: dir.path().join("bosun.exe.previous"),
                target: target.clone(),
                swap_only: false,
            },
            Duration::from_secs(1),
            Duration::from_millis(10),
            || markers_cleared.set(true),
            |target| recorded.set(Some(target.to_path_buf())),
        )
        .await
        .expect_err("a missing previous binary must fail the finalize");

        assert!(matches!(err, RollbackError::Internal(_)));
        assert!(
            markers_cleared.get(),
            "the markers must be cleared on failure"
        );
        assert_eq!(
            recorded.take().as_deref(),
            Some(target.as_path()),
            "the canonical target must be recorded even when the swap fails"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"bad",
            "the target must be left untouched when the previous binary is missing"
        );
        assert!(
            !dir.path().join("bosun.exe.rolled").exists(),
            "nothing must be moved aside when the previous binary is missing"
        );
    }

    #[tokio::test]
    async fn rollback_finalize_retries_the_target_rename_until_the_lock_releases() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let previous = dir.path().join("bosun.exe.previous");
        std::fs::write(&previous, b"good").unwrap();
        let target_for_task = target.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(&target_for_task, b"bad").unwrap();
        });

        finalize_pending_rollback(
            &PendingRollback {
                source: previous,
                target: target.clone(),
                swap_only: false,
            },
            Duration::from_secs(2),
            Duration::from_millis(20),
            || {},
            |_| {},
        )
        .await
        .expect("the rename should succeed once the lock releases");

        assert_eq!(std::fs::read(&target).unwrap(), b"good");
        assert_eq!(
            std::fs::read(dir.path().join("bosun.exe.rolled")).unwrap(),
            b"bad"
        );
    }

    #[tokio::test]
    async fn rollback_finalize_records_the_canonical_target_even_when_the_swap_fails() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let previous = dir.path().join("bosun.exe.previous");
        std::fs::write(&previous, b"good").unwrap();
        let recorded = Cell::new(None);

        let err = finalize_pending_rollback(
            &PendingRollback {
                source: previous,
                target: target.clone(),
                swap_only: false,
            },
            Duration::ZERO,
            Duration::from_millis(10),
            || {},
            |target| recorded.set(Some(target.to_path_buf())),
        )
        .await
        .expect_err("a missing installed binary must fail the swap");

        assert!(matches!(err, RollbackError::Internal(_)));
        assert_eq!(
            recorded.take().as_deref(),
            Some(target.as_path()),
            "the canonical target must be recorded even when the swap fails"
        );
    }

    #[cfg(unix)]
    #[test]
    fn swap_only_rollback_swaps_the_previous_binary_and_stops() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun");
        let previous = dir.path().join("bosun.previous");
        std::fs::write(&target, b"bad").unwrap();
        std::fs::write(&previous, b"good").unwrap();

        launch_rollback(&target, &previous, true)
            .expect("a swap-only rollback should swap and stop without restarting");

        assert_eq!(std::fs::read(&target).unwrap(), b"good");
        assert!(
            !previous.exists(),
            "the previous file must be consumed by the swap"
        );
    }

    #[test]
    fn pending_rollback_parses_both_markers() {
        let target = std::ffi::OsStr::new(r"C:\bosun\bosun.exe");
        let source = std::ffi::OsStr::new(r"C:\bosun\bosun.exe.previous");
        let parsed = pending_rollback(Some(target), Some(source), None)
            .expect("both markers set should yield a pending rollback");
        assert_eq!(parsed.target, PathBuf::from(r"C:\bosun\bosun.exe"));
        assert_eq!(parsed.source, PathBuf::from(r"C:\bosun\bosun.exe.previous"));
        assert!(!parsed.swap_only);
    }

    #[test]
    fn pending_rollback_parses_the_swap_only_marker() {
        let target = std::ffi::OsStr::new(r"C:\bosun\bosun.exe");
        let source = std::ffi::OsStr::new(r"C:\bosun\bosun.exe.previous");

        let swap_only =
            pending_rollback(Some(target), Some(source), Some(std::ffi::OsStr::new("1")))
                .expect("both markers set should yield a pending rollback");
        assert!(
            swap_only.swap_only,
            "the swap-only marker must make the finalizer stop after the swap"
        );

        let restart = pending_rollback(Some(target), Some(source), None)
            .expect("both markers set should yield a pending rollback");
        assert!(
            !restart.swap_only,
            "without the swap-only marker the finalizer must restart"
        );
    }

    #[test]
    fn pending_rollback_is_none_when_a_marker_is_missing_or_empty() {
        let target = std::ffi::OsStr::new("target");
        let source = std::ffi::OsStr::new("source");
        assert!(pending_rollback(None, Some(source), None).is_none());
        assert!(pending_rollback(Some(target), None, None).is_none());
        assert!(pending_rollback(None, None, None).is_none());
        assert!(pending_rollback(Some(std::ffi::OsStr::new("")), Some(source), None).is_none());
        assert!(pending_rollback(Some(target), Some(std::ffi::OsStr::new("")), None).is_none());
    }
}
