//! Shared download/verify/swap primitives for update clients: the node and
//! the CLI both fetch a released archive, verify it, and install it over the
//! running binary. The fetch functions download the cargo-dist archive from
//! GitHub Releases (or a mirror) for a version announced in the protocol.

use std::ffi::OsStr;
use std::io::BufReader;
use std::io::Cursor;
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

use crate::version::compare;

const CHECKSUM_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Where release archives are downloaded from when the environment does not
/// name a mirror: GitHub Releases for this repository, in cargo-dist's layout.
pub const DEFAULT_UPDATE_BASE_URL: &str = "https://github.com/ragnarula/bosun/releases/download";

/// The environment variable naming an update mirror: a base URL served in the
/// same layout as GitHub Releases, for clients that cannot reach GitHub.
pub const UPDATE_BASE_URL_ENV: &str = "BOSUN_UPDATE_BASE_URL";

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("downloaded {actual} bytes, expected {expected}")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("downloaded bytes do not match the release sha256")]
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

    #[error("no release artifact for version {version}: {url} returned HTTP 404")]
    NoRelease { version: String, url: String },

    #[error("the release checksum file at {url} is missing or malformed")]
    MalformedChecksum { url: String },

    #[error("failed to extract the bosun binary from the release archive: {reason}")]
    ExtractionFailed { reason: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// Streams `response`'s body into a unique temp file next to the running
/// binary, verifying the sha256 and, when one is expected, the size. Returns
/// the staged file's path, removing it again on any failure.
async fn stream_verified(
    response: reqwest::Response,
    url: &str,
    dir: &Path,
    expected_size: Option<u64>,
    expected_sha256: &str,
) -> Result<PathBuf, UpdateError> {
    let staged = staged_path(dir);
    let downloaded = async {
        let mut file = tokio::fs::File::create(&staged)
            .await
            .with_context(|| format!("failed to create {}", staged.display()))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("failed to read the body from {url}"))?;
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

        if let Some(expected) = expected_size
            && size != expected
        {
            return Err(UpdateError::SizeMismatch {
                expected,
                actual: size,
            });
        }
        if format!("{:x}", hasher.finalize()) != expected_sha256 {
            return Err(UpdateError::ChecksumMismatch);
        }
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

/// Runs the staged binary's `--version` and requires it to report the
/// expected version before the running binary is touched.
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

/// cargo-dist ships every Windows target as a `.zip` and every other target
/// as a `.tar.xz`, so the archive format follows the requested target triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarXz,
    Zip,
}

fn archive_kind(target: &str) -> ArchiveKind {
    if target.contains("windows") {
        ArchiveKind::Zip
    } else {
        ArchiveKind::TarXz
    }
}

fn archive_file_name(target: &str) -> String {
    match archive_kind(target) {
        ArchiveKind::TarXz => format!("bosun-{target}.tar.xz"),
        ArchiveKind::Zip => format!("bosun-{target}.zip"),
    }
}

/// cargo-dist tags releases `v{version}` and names each archive after its
/// target triple, so the download URL joins the base URL, the version tag, and
/// the archive file name. A version that already carries its `v` prefix is
/// accepted and written back once.
fn release_archive_url(base_url: &str, version: &str, target: &str) -> String {
    let version = version.strip_prefix('v').unwrap_or(version);
    format!(
        "{}/v{version}/{}",
        base_url.trim_end_matches('/'),
        archive_file_name(target)
    )
}

/// cargo-dist writes each checksum as `{hex} *{archive-name}`, a sha256sum
/// layout where whitespace and the file name vary; the digest is the first
/// whitespace-separated token. Returns None when the file holds no
/// 64-character hex digest.
fn parse_sha256(checksum_file: &str) -> Option<&str> {
    let digest = checksum_file.split_whitespace().next()?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digest)
}

fn update_base_url_from(override_value: Option<&std::ffi::OsStr>) -> String {
    match override_value {
        Some(value) => {
            let value = value.to_string_lossy();
            let trimmed = value.trim();
            if trimmed.is_empty() {
                DEFAULT_UPDATE_BASE_URL.to_string()
            } else {
                trimmed.trim_end_matches('/').to_string()
            }
        }
        None => DEFAULT_UPDATE_BASE_URL.to_string(),
    }
}

/// The release feed base URL a client fetches from: the configured base URL
/// when set, else the `BOSUN_UPDATE_BASE_URL` override, else GitHub Releases.
pub fn resolve_update_base_url(config_base_url: Option<&str>) -> String {
    resolve_update_base_url_from(
        config_base_url,
        std::env::var_os(UPDATE_BASE_URL_ENV).as_deref(),
    )
}

fn resolve_update_base_url_from(
    config_base_url: Option<&str>,
    env_override: Option<&std::ffi::OsStr>,
) -> String {
    match config_base_url {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                update_base_url_from(env_override)
            } else {
                trimmed.trim_end_matches('/').to_string()
            }
        }
        None => update_base_url_from(env_override),
    }
}

/// Fetches the archive's per-asset sha256 file, mapping an HTTP 404 to
/// [`UpdateError::NoRelease`]: a release whose archive is not yet published
/// serves no checksum either.
async fn fetch_release_checksum(
    client: &reqwest::Client,
    checksum_url: &str,
    version: &str,
) -> Result<String, UpdateError> {
    let response = client
        .get(checksum_url)
        .timeout(CHECKSUM_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("failed to fetch the release checksum from {checksum_url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(UpdateError::NoRelease {
            version: version.to_string(),
            url: checksum_url.to_string(),
        });
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("the server at {checksum_url} returned an error"))?;
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read the release checksum from {checksum_url}"))?;
    let digest = parse_sha256(&text).ok_or_else(|| UpdateError::MalformedChecksum {
        url: checksum_url.to_string(),
    })?;
    Ok(digest.to_string())
}

/// Downloads, verifies, and extracts the release archive for `version` on
/// `target`, returning the staged `bosun` binary path in `dir` next to the
/// running binary. The download is async; the archive's decompression and
/// extraction run on a blocking thread so a large release never stalls the
/// async executor. Callers verify the staged binary's reported version and
/// install it with `swap_binary`, `copy_staged_to_target`, or
/// `rename_staged_to_target`.
pub async fn fetch_release_artifact(
    client: &reqwest::Client,
    base_url: &str,
    version: &str,
    target: &str,
    dir: &Path,
) -> Result<PathBuf, UpdateError> {
    let archive_url = release_archive_url(base_url, version, target);
    let checksum =
        fetch_release_checksum(client, &format!("{archive_url}.sha256"), version).await?;
    let archive = download_verified(client, &archive_url, version, dir, &checksum).await?;
    let staged = staged_path(dir);
    let extracted = {
        let archive = archive.clone();
        let staged = staged.clone();
        let target = target.to_string();
        tokio::task::spawn_blocking(move || {
            extract_archive(&archive, &staged, &target)?;
            #[cfg(unix)]
            make_executable(&staged)?;
            Ok::<(), UpdateError>(())
        })
        .await
        .context("the release extraction task failed")?
    };
    let _ = std::fs::remove_file(&archive);
    match extracted {
        Ok(()) => Ok(staged),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

/// Downloads the release archive into a unique temp file next to the running
/// binary and verifies its sha256 against the release's checksum file. An HTTP
/// 404 maps to [`UpdateError::NoRelease`]: a release that serves its checksum
/// but not its archive is only partially published.
async fn download_verified(
    client: &reqwest::Client,
    url: &str,
    version: &str,
    dir: &Path,
    expected_sha256: &str,
) -> Result<PathBuf, UpdateError> {
    let response = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("failed to download the release archive from {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(UpdateError::NoRelease {
            version: version.to_string(),
            url: url.to_string(),
        });
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("the server at {url} returned an error"))?;
    stream_verified(response, url, dir, None, expected_sha256).await
}

fn extract_archive(archive: &Path, staged: &Path, target: &str) -> Result<(), UpdateError> {
    let extracted = match archive_kind(target) {
        ArchiveKind::TarXz => extract_tar_xz_binary(archive, staged),
        ArchiveKind::Zip => extract_zip_binary(archive, staged),
    };
    extracted.map_err(|error| UpdateError::ExtractionFailed {
        reason: format!("{error:#}"),
    })
}

fn extract_tar_xz_binary(archive: &Path, staged: &Path) -> Result<(), anyhow::Error> {
    let compressed = std::fs::File::open(archive).with_context(|| {
        format!(
            "failed to open the downloaded archive {}",
            archive.display()
        )
    })?;
    let mut tar_bytes = Vec::new();
    lzma_rs::xz_decompress(&mut BufReader::new(compressed), &mut tar_bytes)
        .context("failed to decompress the release archive")?;
    write_tar_binary(Cursor::new(&tar_bytes), staged)
}

fn write_tar_binary(reader: Cursor<&Vec<u8>>, staged: &Path) -> Result<(), anyhow::Error> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .context("failed to read the release archive")?;
    for entry in entries {
        let mut entry = entry.context("failed to read an entry of the release archive")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let is_binary = entry
            .path()
            .ok()
            .and_then(|path| path.file_name().map(|name| name == "bosun"))
            .unwrap_or(false);
        if !is_binary {
            continue;
        }
        let mut output = std::fs::File::create(staged)
            .with_context(|| format!("failed to create {}", staged.display()))?;
        std::io::copy(&mut entry, &mut output).with_context(|| {
            format!(
                "failed to extract the bosun binary into {}",
                staged.display()
            )
        })?;
        return Ok(());
    }
    anyhow::bail!("the release archive contains no bosun binary")
}

fn extract_zip_binary(archive: &Path, staged: &Path) -> Result<(), anyhow::Error> {
    let file = std::fs::File::open(archive).with_context(|| {
        format!(
            "failed to open the downloaded archive {}",
            archive.display()
        )
    })?;
    let mut zip = zip::ZipArchive::new(file).context("failed to read the release archive")?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .context("failed to read a release entry")?;
        if entry.is_dir() {
            continue;
        }
        let is_binary = Path::new(entry.name())
            .file_name()
            .is_some_and(|name| name == OsStr::new("bosun.exe"));
        if !is_binary {
            continue;
        }
        let mut output = std::fs::File::create(staged)
            .with_context(|| format!("failed to create {}", staged.display()))?;
        std::io::copy(&mut entry, &mut output).with_context(|| {
            format!(
                "failed to extract the bosun binary into {}",
                staged.display()
            )
        })?;
        return Ok(());
    }
    anyhow::bail!("the release archive contains no bosun binary")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

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
}

#[cfg(test)]
mod release_tests {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::io::Write;
    use std::sync::Arc;

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::get;
    use tempfile::TempDir;

    use super::*;

    const REAL_RELEASE_CHECKSUM: &str =
        "b4031e5e40417ba86b03f523fe7cefdc426fec97edd141b23ed0b98f9f6d6407";
    const REAL_ARCHIVE_NAME: &str = "bosun-aarch64-apple-darwin.tar.xz";

    #[test]
    fn release_archive_url_points_at_the_cargo_dist_asset() {
        assert_eq!(
            release_archive_url(
                "https://github.com/ragnarula/bosun/releases/download",
                "0.6.0",
                "aarch64-apple-darwin"
            ),
            "https://github.com/ragnarula/bosun/releases/download/v0.6.0/bosun-aarch64-apple-darwin.tar.xz"
        );
    }

    #[test]
    fn windows_release_archives_are_zips() {
        for target in [
            "x86_64-pc-windows-msvc",
            "i686-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-pc-windows-gnu",
        ] {
            assert_eq!(
                archive_file_name(target),
                format!("bosun-{target}.zip"),
                "every Windows triple must ship a zip, not a tar.xz"
            );
        }
        assert_eq!(
            archive_file_name("aarch64-apple-darwin"),
            "bosun-aarch64-apple-darwin.tar.xz"
        );
        assert_eq!(
            release_archive_url(DEFAULT_UPDATE_BASE_URL, "0.6.0", "x86_64-pc-windows-msvc"),
            "https://github.com/ragnarula/bosun/releases/download/v0.6.0/bosun-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn release_archive_url_normalizes_the_base_and_the_version_prefix() {
        assert_eq!(
            release_archive_url(
                "https://mirror.example/bosun/",
                "v0.6.0",
                "x86_64-apple-darwin"
            ),
            "https://mirror.example/bosun/v0.6.0/bosun-x86_64-apple-darwin.tar.xz"
        );
    }

    #[test]
    fn parse_sha256_reads_the_cargo_dist_checksum_layout() {
        let file = format!("{REAL_RELEASE_CHECKSUM} *{REAL_ARCHIVE_NAME}\n\n");
        assert_eq!(parse_sha256(&file), Some(REAL_RELEASE_CHECKSUM));
    }

    #[test]
    fn parse_sha256_tolerates_text_mode_and_crlf() {
        let file = format!("{REAL_RELEASE_CHECKSUM}  {REAL_ARCHIVE_NAME}\r\n");
        assert_eq!(parse_sha256(&file), Some(REAL_RELEASE_CHECKSUM));
    }

    #[test]
    fn parse_sha256_accepts_a_digest_without_a_file_name() {
        assert_eq!(
            parse_sha256(&format!("{REAL_RELEASE_CHECKSUM}\n")),
            Some(REAL_RELEASE_CHECKSUM)
        );
    }

    #[test]
    fn parse_sha256_rejects_malformed_files() {
        assert_eq!(parse_sha256(""), None);
        assert_eq!(parse_sha256("not a checksum\n"), None);
        assert_eq!(parse_sha256("abcd\n"), None);
        assert_eq!(parse_sha256(&format!("{}\n", "z".repeat(64))), None);
    }

    #[test]
    fn update_base_url_defaults_to_github_releases() {
        assert_eq!(update_base_url_from(None), DEFAULT_UPDATE_BASE_URL);
    }

    #[test]
    fn update_base_url_takes_the_override_and_strips_its_trailing_slash() {
        assert_eq!(
            update_base_url_from(Some(OsStr::new("https://mirror.example/bosun/"))),
            "https://mirror.example/bosun"
        );
    }

    #[test]
    fn update_base_url_ignores_an_empty_override() {
        assert_eq!(
            update_base_url_from(Some(OsStr::new(""))),
            DEFAULT_UPDATE_BASE_URL
        );
    }

    #[test]
    fn update_base_url_ignores_a_whitespace_only_override() {
        assert_eq!(
            update_base_url_from(Some(OsStr::new("   "))),
            DEFAULT_UPDATE_BASE_URL
        );
        assert_eq!(
            update_base_url_from(Some(OsStr::new(" \t "))),
            DEFAULT_UPDATE_BASE_URL
        );
    }

    #[test]
    fn update_base_url_trims_surrounding_whitespace_from_the_override() {
        assert_eq!(
            update_base_url_from(Some(OsStr::new("  https://mirror.example/bosun/  "))),
            "https://mirror.example/bosun"
        );
    }

    #[test]
    fn resolve_update_base_url_prefers_the_config_value() {
        assert_eq!(
            resolve_update_base_url_from(
                Some("https://config.example/bosun/"),
                Some(OsStr::new("https://env.example/bosun")),
            ),
            "https://config.example/bosun"
        );
    }

    #[test]
    fn resolve_update_base_url_falls_back_to_the_env_override() {
        assert_eq!(
            resolve_update_base_url_from(None, Some(OsStr::new("https://env.example/bosun/"))),
            "https://env.example/bosun"
        );
        assert_eq!(
            resolve_update_base_url_from(Some(""), Some(OsStr::new("https://env.example/bosun")),),
            "https://env.example/bosun",
            "an empty config value must not shadow the env override"
        );
        assert_eq!(
            resolve_update_base_url_from(
                Some("   "),
                Some(OsStr::new("https://env.example/bosun")),
            ),
            "https://env.example/bosun",
            "a whitespace-only config value must not shadow the env override"
        );
    }

    #[test]
    fn resolve_update_base_url_defaults_to_github_releases() {
        assert_eq!(
            resolve_update_base_url_from(None, None),
            DEFAULT_UPDATE_BASE_URL
        );
        assert_eq!(
            resolve_update_base_url_from(Some(""), None),
            DEFAULT_UPDATE_BASE_URL
        );
        assert_eq!(
            resolve_update_base_url_from(Some("  "), None),
            DEFAULT_UPDATE_BASE_URL
        );
    }

    #[test]
    fn resolve_update_base_url_trims_surrounding_whitespace_from_the_config_value() {
        assert_eq!(
            resolve_update_base_url_from(
                Some("  https://config.example/bosun/  "),
                Some(OsStr::new("https://env.example/bosun")),
            ),
            "https://config.example/bosun"
        );
    }

    #[tokio::test]
    async fn fetch_release_artifact_stages_the_extracted_tar_xz_binary() {
        let content = b"#!/bin/sh\necho 'bosun 0.6.0'\n";
        let target = "aarch64-apple-darwin";
        let dir = TempDir::new().unwrap();
        let routes = release_routes("0.6.0", target, &tar_xz_archive(target, content));
        let port = release_server(routes).await;

        let staged = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect("the release should download, verify, and extract");

        assert_eq!(std::fs::read(&staged).unwrap(), content);
        assert_no_temp_files(dir.path(), Some(&staged));
        #[cfg(unix)]
        assert_executable(&staged);
    }

    #[tokio::test]
    async fn fetch_release_artifact_stages_the_extracted_zip_binary() {
        let content = b"fake bosun.exe";
        let target = "x86_64-pc-windows-msvc";
        let dir = TempDir::new().unwrap();
        let routes = release_routes("0.6.0", target, &zip_archive(content));
        let port = release_server(routes).await;

        let staged = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect("the release should download, verify, and extract");

        assert_eq!(std::fs::read(&staged).unwrap(), content);
        assert_no_temp_files(dir.path(), Some(&staged));
    }

    #[tokio::test]
    async fn fetch_release_artifact_reports_a_missing_release() {
        let dir = TempDir::new().unwrap();
        let port = release_server(HashMap::new()).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            "aarch64-apple-darwin",
            dir.path(),
        )
        .await
        .expect_err("a release the server does not serve must fail");

        assert!(matches!(err, UpdateError::NoRelease { version, .. } if version == "0.6.0"));
        assert_no_temp_files(dir.path(), None);
    }

    #[tokio::test]
    async fn fetch_release_artifact_maps_a_missing_archive_asset_to_no_release() {
        let target = "aarch64-apple-darwin";
        let dir = TempDir::new().unwrap();
        let mut routes = HashMap::new();
        let path = format!("/v0.6.0/{}", archive_file_name(target));
        routes.insert(
            format!("{path}.sha256"),
            format!("{} *{}\n", "0".repeat(64), archive_file_name(target)).into_bytes(),
        );
        let port = release_server(routes).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect_err("a release that serves its checksum but not its archive must fail");

        assert!(
            matches!(err, UpdateError::NoRelease { version, url } if version == "0.6.0" && url == format!("http://127.0.0.1:{port}{path}"))
        );
        assert_no_temp_files(dir.path(), None);
    }

    #[tokio::test]
    async fn fetch_release_artifact_rejects_a_checksum_mismatch() {
        let target = "aarch64-apple-darwin";
        let archive = tar_xz_archive(target, b"#!/bin/sh\n");
        let checksum = format!("{} *{}\n", "0".repeat(64), archive_file_name(target));
        let dir = TempDir::new().unwrap();
        let mut routes = HashMap::new();
        let path = format!("/v0.6.0/{}", archive_file_name(target));
        routes.insert(format!("{path}.sha256"), checksum.into_bytes());
        routes.insert(path, archive);
        let port = release_server(routes).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect_err("the checksum must match");

        assert!(matches!(err, UpdateError::ChecksumMismatch));
        assert_no_temp_files(dir.path(), None);
    }

    #[tokio::test]
    async fn fetch_release_artifact_rejects_a_malformed_checksum_file() {
        let target = "aarch64-apple-darwin";
        let dir = TempDir::new().unwrap();
        let mut routes = HashMap::new();
        let path = format!("/v0.6.0/{}", archive_file_name(target));
        routes.insert(format!("{path}.sha256"), b"not a sha256 file".to_vec());
        routes.insert(path, tar_xz_archive(target, b"#!/bin/sh\n"));
        let port = release_server(routes).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect_err("the checksum file must hold a hex digest");

        assert!(matches!(err, UpdateError::MalformedChecksum { .. }));
        assert_no_temp_files(dir.path(), None);
    }

    #[tokio::test]
    async fn fetch_release_artifact_rejects_an_unreadable_archive() {
        let target = "aarch64-apple-darwin";
        let dir = TempDir::new().unwrap();
        let archive = b"this is not an xz archive".to_vec();
        let mut routes = HashMap::new();
        let path = format!("/v0.6.0/{}", archive_file_name(target));
        routes.insert(
            format!("{path}.sha256"),
            format!("{} *{}\n", hex(&archive), archive_file_name(target)).into_bytes(),
        );
        routes.insert(path, archive);
        let port = release_server(routes).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect_err("a corrupt archive must fail the extraction");

        assert!(matches!(err, UpdateError::ExtractionFailed { .. }));
        assert_no_temp_files(dir.path(), None);
    }

    #[tokio::test]
    async fn fetch_release_artifact_rejects_a_valid_tar_archive_without_the_binary() {
        let target = "aarch64-apple-darwin";
        let dir = TempDir::new().unwrap();
        let archive = tar_xz_entries(&[
            ("bosun-aarch64-apple-darwin/README.md", b"readme text"),
            ("bosun-aarch64-apple-darwin/LICENSE-MIT", b"license text"),
        ]);
        let routes = release_routes("0.6.0", target, &archive);
        let port = release_server(routes).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect_err("a well-formed archive without the bosun binary must fail");

        match err {
            UpdateError::ExtractionFailed { reason } => assert!(
                reason.contains("contains no bosun binary"),
                "unexpected reason: {reason}"
            ),
            other => panic!("expected ExtractionFailed, got {other:?}"),
        }
        assert_no_temp_files(dir.path(), None);
    }

    #[tokio::test]
    async fn fetch_release_artifact_rejects_a_valid_zip_archive_without_the_binary() {
        let target = "x86_64-pc-windows-msvc";
        let dir = TempDir::new().unwrap();
        let archive = zip_entries(&[("README.txt", b"readme text")]);
        let routes = release_routes("0.6.0", target, &archive);
        let port = release_server(routes).await;

        let err = fetch_release_artifact(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            "0.6.0",
            target,
            dir.path(),
        )
        .await
        .expect_err("a well-formed archive without the bosun.exe binary must fail");

        match err {
            UpdateError::ExtractionFailed { reason } => assert!(
                reason.contains("contains no bosun binary"),
                "unexpected reason: {reason}"
            ),
            other => panic!("expected ExtractionFailed, got {other:?}"),
        }
        assert_no_temp_files(dir.path(), None);
    }

    #[test]
    fn tar_extraction_skips_cargo_dist_metadata_entries() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("release.tar.xz");
        let staged = dir.path().join("bosun");
        let content = b"#!/bin/sh\necho 'bosun 0.6.0'\n";
        std::fs::write(
            &archive_path,
            tar_xz_entries(&[
                ("bosun-aarch64-apple-darwin/LICENSE-MIT", b"license text"),
                ("bosun-aarch64-apple-darwin/LICENSE-APACHE", b"license text"),
                ("bosun-aarch64-apple-darwin/README.md", b"readme text"),
                ("bosun-aarch64-apple-darwin/bosun", content),
            ]),
        )
        .unwrap();

        extract_tar_xz_binary(&archive_path, &staged).unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), content);
        assert!(
            !dir.path().join("bosun-aarch64-apple-darwin").exists(),
            "the metadata entries must not be unpacked next to the binary"
        );
    }

    #[test]
    fn tar_extraction_rejects_an_archive_without_the_bosun_binary() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("release.tar.xz");
        let staged = dir.path().join("bosun");
        std::fs::write(
            &archive_path,
            tar_xz_entries(&[
                ("bosun-aarch64-apple-darwin/LICENSE-MIT", b"license text"),
                ("bosun-aarch64-apple-darwin/README.md", b"readme text"),
            ]),
        )
        .unwrap();

        let err = extract_tar_xz_binary(&archive_path, &staged)
            .expect_err("an archive without the bosun binary must fail");

        assert!(
            format!("{err:#}").contains("contains no bosun binary"),
            "unexpected error: {err}"
        );
        assert!(!staged.exists());
    }

    #[test]
    fn tar_extraction_writes_the_staged_path_not_the_entry_path() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("staging");
        std::fs::create_dir(&dir).unwrap();
        let archive_path = dir.join("release.tar.xz");
        let staged = dir.join("bosun");
        let content = b"#!/bin/sh\necho 'bosun 0.6.0'\n";
        std::fs::write(
            &archive_path,
            tar_xz_with_raw_names(&[("../../bosun", content)]),
        )
        .unwrap();

        extract_tar_xz_binary(&archive_path, &staged).unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), content);
        assert!(
            !root.path().join("bosun").exists(),
            "the entry name must not choose an output path outside the staging directory"
        );
    }

    #[test]
    fn zip_extraction_skips_entries_whose_file_name_is_not_bosun_exe() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("release.zip");
        let staged = dir.path().join("bosun.exe");
        let content = b"the real bosun.exe";
        std::fs::write(
            &archive_path,
            zip_entries(&[
                ("xbosun.exe", b"a different binary"),
                ("bosun.exe", content),
            ]),
        )
        .unwrap();

        extract_zip_binary(&archive_path, &staged).unwrap();

        assert_eq!(
            std::fs::read(&staged).unwrap(),
            content,
            "an entry whose file name merely ends in bosun.exe must not be extracted"
        );
    }

    #[test]
    fn zip_extraction_rejects_an_archive_without_the_bosun_binary() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("release.zip");
        let staged = dir.path().join("bosun.exe");
        std::fs::write(
            &archive_path,
            zip_entries(&[("README.txt", b"readme text")]),
        )
        .unwrap();

        let err = extract_zip_binary(&archive_path, &staged)
            .expect_err("an archive without the bosun.exe binary must fail");

        assert!(
            format!("{err:#}").contains("contains no bosun binary"),
            "unexpected error: {err}"
        );
        assert!(!staged.exists());
    }

    #[test]
    fn zip_extraction_writes_the_staged_path_not_the_entry_path() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("staging");
        std::fs::create_dir(&dir).unwrap();
        let archive_path = dir.join("release.zip");
        let staged = dir.join("bosun.exe");
        let content = b"the real bosun.exe";
        std::fs::write(&archive_path, zip_entries(&[("../bosun.exe", content)])).unwrap();

        extract_zip_binary(&archive_path, &staged).unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), content);
        assert!(
            !root.path().join("bosun.exe").exists(),
            "the entry name must not choose an output path outside the staging directory"
        );
    }

    /// A tar.xz holding each entry at its archive path.
    fn tar_xz_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut uncompressed = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut uncompressed);
            for (path, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, Cursor::new(content))
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(&uncompressed), &mut compressed).unwrap();
        compressed
    }

    /// A tar.xz in cargo-dist's layout: a `bosun-<target>/bosun` entry.
    fn tar_xz_archive(target: &str, content: &[u8]) -> Vec<u8> {
        tar_xz_entries(&[(&format!("bosun-{target}/bosun"), content)])
    }

    /// A tar.xz whose entry names are stored verbatim, bypassing the tar
    /// builder's refusal of `..` components, for traversal fixtures.
    fn tar_xz_with_raw_names(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut uncompressed = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut uncompressed);
            for (path, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::Regular);
                header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
                header.set_cksum();
                builder.append(&header, Cursor::new(content)).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(&uncompressed), &mut compressed).unwrap();
        compressed
    }

    /// A zip holding each file at its archive path.
    fn zip_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            for (path, content) in entries {
                writer.start_file(*path, options).unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    /// A zip holding `bosun.exe`, the cargo-dist layout for Windows targets.
    fn zip_archive(content: &[u8]) -> Vec<u8> {
        zip_entries(&[("bosun.exe", content)])
    }

    fn release_routes(version: &str, target: &str, archive: &[u8]) -> HashMap<String, Vec<u8>> {
        let name = archive_file_name(target);
        HashMap::from([
            (
                format!("/v{version}/{name}.sha256"),
                format!("{} *{name}\n\n", hex(archive)).into_bytes(),
            ),
            (format!("/v{version}/{name}"), archive.to_vec()),
        ])
    }

    fn hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    type Routes = Arc<HashMap<String, Vec<u8>>>;

    async fn serve_release(
        axum::extract::Path(path): axum::extract::Path<String>,
        State(routes): State<Routes>,
    ) -> Response {
        let body = routes
            .get(&path)
            .or_else(|| routes.get(&format!("/{path}")))
            .cloned();
        match body {
            Some(body) => body.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn release_server(routes: HashMap<String, Vec<u8>>) -> u16 {
        let app = Router::new()
            .route("/{*path}", get(serve_release))
            .with_state(Arc::new(routes));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr.port()
    }

    fn assert_no_temp_files(dir: &Path, staged: Option<&Path>) {
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| Some(path.as_path()) != staged)
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("bosun.update.tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    fn assert_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "the staged binary must be executable");
    }
}
