//! Shared download/verify/swap primitives for update clients: the node and
//! the CLI both fetch the control plane's artifact, verify it, and install it
//! over the running binary.

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::types::Artifact;
use crate::types::Manifest;
use crate::version::compare;

const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("the control plane has no artifact for target {target}")]
    NoArtifact { target: String },

    #[error("downloaded {actual} bytes, the manifest promised {expected}")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("downloaded binary does not match the manifest sha256")]
    ChecksumMismatch,

    #[error("downloaded binary reports version {actual}, expected {expected}")]
    VersionMismatch { expected: String, actual: String },

    #[error(
        "downgrade requires --force: the client is at {cli_version} and the control plane is at {cp_version}"
    )]
    DowngradeRequiresForce {
        cli_version: String,
        cp_version: String,
    },

    #[error("the control plane reports an unparsable version {version:?}")]
    UnparsableVersion { version: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

pub async fn fetch_manifest(
    client: &reqwest::Client,
    cp_url: &str,
) -> Result<Manifest, UpdateError> {
    let url = format!("{}/update/manifest", cp_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("failed to fetch the update manifest from {url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("the control plane at {url} returned an error"))?;
    response
        .json()
        .await
        .with_context(|| format!("failed to parse the update manifest from {url}"))
        .map_err(UpdateError::Internal)
}

/// Streams the artifact into a unique temp file next to the running binary
/// and verifies its size and sha256 against the manifest. Returns the staged
/// file's path, removing it again on any failure.
pub async fn download_artifact(
    client: &reqwest::Client,
    cp_url: &str,
    dir: &Path,
    target: &str,
    artifact: &Artifact,
) -> Result<PathBuf, UpdateError> {
    let url = format!("{}/update/artifact/{target}", cp_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("failed to download the update artifact from {url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("the control plane at {url} returned an error"))?;

    let staged = staged_path(dir);
    let downloaded = async {
        let mut file = tokio::fs::File::create(&staged)
            .await
            .with_context(|| format!("failed to create {}", staged.display()))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.with_context(|| format!("failed to read the artifact body from {url}"))?;
            size += chunk.len() as u64;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .with_context(|| format!("failed to write {}", staged.display()))?;
        }
        file.flush()
            .await
            .with_context(|| format!("failed to flush {}", staged.display()))?;
        drop(file);

        if size != artifact.size {
            return Err(UpdateError::SizeMismatch {
                expected: artifact.size,
                actual: size,
            });
        }
        if format!("{:x}", hasher.finalize()) != artifact.sha256 {
            return Err(UpdateError::ChecksumMismatch);
        }
        #[cfg(unix)]
        make_executable(&staged)?;
        Ok(())
    }
    .await;

    match downloaded {
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
        Ok(()) => Ok(staged),
    }
}

/// A staged download path unique to this attempt, so two update tasks can
/// never write the same file.
fn staged_path(dir: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    dir.join(format!(
        "bosun.update.tmp.{}.{}",
        std::process::id(),
        sequence
    ))
}

/// Runs the binary's `--version` and returns the printed version when the
/// output ends in a parseable semver version.
pub async fn reported_version(exe: &Path) -> Result<String, anyhow::Error> {
    let output = tokio::process::Command::new(exe)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("failed to run the binary {}", exe.display()))?;
    if !output.status.success() {
        anyhow::bail!("the binary exited with {}", output.status);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    version_from_output(&stdout)
        .map(str::to_string)
        .with_context(|| {
            format!(
                "the binary {} did not print a semver version",
                exe.display()
            )
        })
}

/// Runs the staged binary's `--version` and requires it to match the control
/// plane's version before the running binary is touched.
pub async fn verify_binary_version(staged: &Path, expected: &str) -> Result<(), UpdateError> {
    let actual = reported_version(staged)
        .await
        .map_err(UpdateError::Internal)?;
    if compare(&actual, expected) != Some(std::cmp::Ordering::Equal) {
        return Err(UpdateError::VersionMismatch {
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// The last whitespace-separated token of `--version` output, when it parses
/// as a semver version.
fn version_from_output(output: &str) -> Option<&str> {
    let version = output.split_whitespace().last()?;
    (semver::Version::parse(version).is_ok()).then_some(version)
}

pub fn previous_path(current: &Path) -> PathBuf {
    let mut name = current.as_os_str().to_os_string();
    name.push(".previous");
    current.with_file_name(name)
}

/// Moves the running binary to `<name>.previous` and the staged binary into
/// its place. Restores the running binary if the second rename fails, so a
/// failed swap never leaves the path empty.
#[cfg(unix)]
pub fn swap_binary(current: &Path, staged: &Path) -> Result<(), UpdateError> {
    let previous = previous_path(current);
    std::fs::rename(current, &previous).with_context(|| {
        format!(
            "failed to move the running binary to {}",
            previous.display()
        )
    })?;
    if let Err(error) = std::fs::rename(staged, current) {
        let _ = std::fs::rename(&previous, current);
        return Err(UpdateError::Internal(anyhow::Error::new(error).context(
            format!(
                "failed to move {} into {}",
                staged.display(),
                current.display()
            ),
        )));
    }
    Ok(())
}

/// Moves the installed binary to `<name>.previous`, leaving its path empty
/// for the copy that follows.
pub fn move_target_to_previous(target: &Path) -> Result<(), UpdateError> {
    let previous = previous_path(target);
    std::fs::rename(target, &previous).with_context(|| {
        format!(
            "failed to move the installed binary to {}",
            previous.display()
        )
    })?;
    Ok(())
}

/// Copies the staged binary over the installed path, restoring the previous
/// binary if the copy fails so the path is never empty. A failed restore is
/// logged because it breaks that guarantee and would otherwise go unnoticed.
/// Windows locks a running image against rename but allows reading it, so the
/// staged image is copied, never renamed; it stays locked until this process
/// exits and is left in place, and stray `bosun.update.tmp.*` files from
/// Windows updates are cleaned up on a later boot if ever needed.
pub fn copy_staged_to_target(target: &Path, staged: &Path) -> Result<(), UpdateError> {
    if let Err(error) = std::fs::copy(staged, target) {
        let previous = previous_path(target);
        if let Err(restore) = std::fs::rename(&previous, target) {
            tracing::error!(
                error = %restore,
                target = %target.display(),
                previous = %previous.display(),
                "failed to restore the previous binary after a failed copy; the install path is left empty"
            );
        }
        return Err(UpdateError::Internal(anyhow::Error::new(error).context(
            format!(
                "failed to copy {} into {}",
                staged.display(),
                target.display()
            ),
        )));
    }
    Ok(())
}

/// Moves the staged binary into the installed path, restoring the previous
/// binary if the move fails so the path is never empty. A failed restore is
/// logged because it breaks that guarantee and would otherwise go unnoticed.
/// Used where the staged file is not a running image, so a rename can install
/// it on Windows instead of the copy the node must use.
pub fn rename_staged_to_target(target: &Path, staged: &Path) -> Result<(), UpdateError> {
    if let Err(error) = std::fs::rename(staged, target) {
        let previous = previous_path(target);
        if let Err(restore) = std::fs::rename(&previous, target) {
            tracing::error!(
                error = %restore,
                target = %target.display(),
                previous = %previous.display(),
                "failed to restore the previous binary after a failed rename; the install path is left empty"
            );
        }
        return Err(UpdateError::Internal(anyhow::Error::new(error).context(
            format!(
                "failed to move {} into {}",
                staged.display(),
                target.display()
            ),
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions()
        .mode()
        | 0o111;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to make {} executable", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;

    fn artifact(content: &[u8]) -> Artifact {
        Artifact {
            sha256: format!("{:x}", Sha256::digest(content)),
            size: content.len() as u64,
        }
    }

    #[test]
    fn version_from_output_parses_the_last_token() {
        assert_eq!(version_from_output("bosun 0.5.5\n"), Some("0.5.5"));
        assert_eq!(version_from_output("bosun 0.5.5"), Some("0.5.5"));
        assert_eq!(
            version_from_output("bosun 0.5.5-alpha.1\n"),
            Some("0.5.5-alpha.1")
        );
        assert_eq!(version_from_output("no version here"), None);
        assert_eq!(version_from_output(""), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verify_accepts_a_matching_version() {
        let dir = tempdir().unwrap();
        let staged = write_version_script(dir.path(), "0.5.6");
        verify_binary_version(&staged, "0.5.6")
            .await
            .expect("the staged binary should report the expected version");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verify_rejects_a_mismatched_version() {
        let dir = tempdir().unwrap();
        let staged = write_version_script(dir.path(), "0.5.6");
        let err = verify_binary_version(&staged, "0.5.5")
            .await
            .expect_err("the reported version must match");
        assert!(matches!(err, UpdateError::VersionMismatch { .. }));
    }

    #[cfg(unix)]
    fn write_version_script(dir: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("bosun.staged");
        std::fs::write(&path, format!("#!/bin/sh\necho 'bosun {version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn previous_path_appends_after_the_name() {
        assert_eq!(
            previous_path(Path::new("/usr/local/bin/bosun")),
            PathBuf::from("/usr/local/bin/bosun.previous")
        );
        assert_eq!(
            previous_path(Path::new("/usr/bin/bosun.exe")),
            PathBuf::from("/usr/bin/bosun.exe.previous")
        );
    }

    #[cfg(unix)]
    #[test]
    fn swap_moves_the_current_binary_to_previous_and_stages_the_new_one() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("bosun");
        let staged = dir.path().join("bosun.update.tmp");
        std::fs::write(&current, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        swap_binary(&current, &staged).unwrap();

        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join("bosun.previous")).unwrap(),
            b"old"
        );
        assert!(!staged.exists());
    }

    #[cfg(unix)]
    #[test]
    fn swap_overwrites_an_existing_previous() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("bosun");
        let staged = dir.path().join("bosun.update.tmp");
        std::fs::write(&current, b"old").unwrap();
        std::fs::write(dir.path().join("bosun.previous"), b"stale").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        swap_binary(&current, &staged).unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("bosun.previous")).unwrap(),
            b"old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn swap_restores_the_previous_binary_when_the_staged_rename_fails() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("bosun");
        let staged = dir.path().join("bosun.update.tmp.missing");
        std::fs::write(&current, b"old").unwrap();

        let err =
            swap_binary(&current, &staged).expect_err("the missing staged file must fail the swap");

        assert!(matches!(err, UpdateError::Internal(_)));
        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"old",
            "the previous binary must be restored over the failed swap"
        );
        assert!(
            !dir.path().join("bosun.previous").exists(),
            "the restore must consume the previous binary"
        );
    }

    #[test]
    fn install_moves_the_target_to_previous_then_copies_the_staged_binary() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.1.0");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        move_target_to_previous(&target).unwrap();
        copy_staged_to_target(&target, &staged).unwrap();

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

    #[test]
    fn install_restores_the_previous_binary_when_the_staged_copy_fails() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.missing");
        std::fs::write(&target, b"old").unwrap();

        move_target_to_previous(&target).unwrap();
        let err = copy_staged_to_target(&target, &staged)
            .expect_err("the missing staged file must fail the copy");

        assert!(matches!(err, UpdateError::Internal(_)));
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

    #[test]
    fn rename_moves_the_staged_binary_over_the_target() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.1.0");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        move_target_to_previous(&target).unwrap();
        rename_staged_to_target(&target, &staged).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join("bosun.exe.previous")).unwrap(),
            b"old"
        );
        assert!(!staged.exists(), "the staged file must be consumed");
    }

    #[test]
    fn rename_restores_the_previous_binary_when_the_staged_rename_fails() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("bosun.exe");
        let staged = dir.path().join("bosun.update.tmp.missing");
        std::fs::write(&target, b"old").unwrap();

        move_target_to_previous(&target).unwrap();
        let err = rename_staged_to_target(&target, &staged)
            .expect_err("the missing staged file must fail the rename");

        assert!(matches!(err, UpdateError::Internal(_)));
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old",
            "the previous binary must be restored over the failed rename"
        );
        assert!(
            !dir.path().join("bosun.exe.previous").exists(),
            "the restore must consume the previous binary"
        );
    }

    fn assert_no_staged_files(dir: &Path) {
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("bosun.update.tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged files left behind: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn fetch_manifest_reads_the_served_manifest() {
        let content = b"fake binary";
        let artifact = artifact(content);
        let port = artifact_server(&artifact, content).await;

        let manifest = fetch_manifest(&reqwest::Client::new(), &format!("http://127.0.0.1:{port}"))
            .await
            .expect("the manifest should fetch");

        assert_eq!(manifest.version, "0.5.5");
        assert_eq!(
            manifest.artifacts[crate::target::TARGET].sha256,
            artifact.sha256
        );
    }

    #[tokio::test]
    async fn download_stages_and_verifies_the_artifact() {
        let content = b"fake binary";
        let artifact = artifact(content);
        let port = artifact_server(&artifact, content).await;
        let dir = tempdir().unwrap();

        let staged = download_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            dir.path(),
            crate::target::TARGET,
            &artifact,
        )
        .await
        .expect("the artifact should download and verify");

        assert_eq!(std::fs::read(&staged).unwrap(), content);
    }

    #[tokio::test]
    async fn download_rejects_a_checksum_mismatch() {
        let content = b"fake binary";
        let bad = Artifact {
            sha256: "0".repeat(64),
            size: content.len() as u64,
        };
        let port = artifact_server(&bad, content).await;
        let dir = tempdir().unwrap();

        let err = download_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            dir.path(),
            crate::target::TARGET,
            &bad,
        )
        .await
        .expect_err("the checksum must match");

        assert!(matches!(err, UpdateError::ChecksumMismatch));
        assert_no_staged_files(dir.path());
    }

    #[tokio::test]
    async fn download_rejects_a_size_mismatch() {
        let content = b"fake binary";
        let bad = Artifact {
            sha256: artifact(content).sha256,
            size: content.len() as u64 + 1,
        };
        let port = artifact_server(&bad, content).await;
        let dir = tempdir().unwrap();

        let err = download_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            dir.path(),
            crate::target::TARGET,
            &bad,
        )
        .await
        .expect_err("the size must match");

        assert!(matches!(err, UpdateError::SizeMismatch { .. }));
        assert_no_staged_files(dir.path());
    }

    async fn artifact_server(artifact: &Artifact, content: &[u8]) -> u16 {
        use std::sync::Arc;

        use axum::Json;
        use axum::Router;
        use axum::extract::State;
        use axum::routing::get;

        #[derive(Clone)]
        struct ServerState {
            manifest: Manifest,
            content: Arc<Vec<u8>>,
        }

        async fn serve_manifest(State(state): State<ServerState>) -> Json<Manifest> {
            Json(state.manifest)
        }

        async fn serve_artifact(State(state): State<ServerState>) -> Vec<u8> {
            (*state.content).clone()
        }

        let app = Router::new()
            .route("/update/manifest", get(serve_manifest))
            .route("/update/artifact/{target}", get(serve_artifact))
            .with_state(ServerState {
                manifest: Manifest {
                    version: "0.5.5".into(),
                    artifacts: HashMap::from([(
                        crate::target::TARGET.to_string(),
                        artifact.clone(),
                    )]),
                },
                content: Arc::new(content.to_vec()),
            });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr.port()
    }
}
