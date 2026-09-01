use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context;
use bosun_common::types::Artifact;
use bosun_common::types::Manifest;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// The control plane's artifacts directory: one file per platform, named
/// `bosun.<target-triple>`. Hashes files lazily and reuses a hash while the
/// file's mtime is unchanged.
pub struct ArtifactStore {
    dir: PathBuf,
    cache: Arc<Mutex<HashMap<String, CachedArtifact>>>,
}

#[derive(Clone)]
struct CachedArtifact {
    modified: SystemTime,
    sha256: String,
    size: u64,
}

impl ArtifactStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The manifest of every artifact currently in the directory. Targets
    /// with no file are absent; a directory that does not exist is empty.
    /// Scans and hashes on a blocking thread, so a large artifact never
    /// stalls the async runtime.
    pub async fn manifest(&self) -> Result<Manifest, ArtifactError> {
        let dir = self.dir.clone();
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || scan_manifest(&dir, &cache))
            .await
            .context("artifact manifest task failed")?
    }

    /// The artifact file for `target`, or `None` when the target cannot name
    /// a file in the directory.
    pub fn artifact_path(&self, target: &str) -> Option<PathBuf> {
        if target.is_empty()
            || !target
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return None;
        }
        Some(self.dir.join(format!("bosun.{target}")))
    }
}

/// Scans the artifacts directory and rehashes files whose mtime moved on
/// since the cache. Runs on a blocking thread via `ArtifactStore::manifest`.
fn scan_manifest(
    dir: &Path,
    cache: &Mutex<HashMap<String, CachedArtifact>>,
) -> Result<Manifest, ArtifactError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Manifest {
                version: bosun_common::version::VERSION.to_string(),
                artifacts: HashMap::new(),
            });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to list {}", dir.display()))
                .map_err(ArtifactError::Internal);
        }
    };

    let mut artifacts = HashMap::new();
    let mut cache = cache.lock().unwrap();

    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read an entry in {}", dir.display()))?;
        let file_name = entry.file_name();
        let Some(target) = file_name
            .to_str()
            .and_then(|name| name.strip_prefix("bosun."))
        else {
            continue;
        };
        let path = entry.path();
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let cached = match cache.get(target) {
            Some(cached) if cached.modified == modified => cached.clone(),
            _ => {
                let sha256 = hash_file(&path)?;
                let cached = CachedArtifact {
                    modified,
                    sha256,
                    size: metadata.len(),
                };
                cache.insert(target.to_string(), cached.clone());
                cached
            }
        };
        artifacts.insert(
            target.to_string(),
            Artifact {
                sha256: cached.sha256,
                size: cached.size,
            },
        );
    }

    cache.retain(|target, _| artifacts.contains_key(target));

    Ok(Manifest {
        version: bosun_common::version::VERSION.to_string(),
        artifacts,
    })
}

fn hash_file(path: &Path) -> Result<String, ArtifactError> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("failed to hash {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::SystemTime;

    use tempfile::tempdir;

    use super::*;

    fn sha256_of(content: &[u8]) -> String {
        format!("{:x}", Sha256::digest(content))
    }

    fn store_with(dir: &Path, files: &[(&str, &[u8])]) -> ArtifactStore {
        std::fs::create_dir_all(dir).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
        }
        ArtifactStore::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn manifest_lists_every_artifact_with_hash_and_size() {
        let dir = tempdir().unwrap();
        let store = store_with(
            &dir.path().join("artifacts"),
            &[
                ("bosun.aarch64-apple-darwin", b"mac"),
                ("bosun.x86_64-unknown-linux-musl", b"linux"),
            ],
        );

        let manifest = store.manifest().await.unwrap();
        assert_eq!(manifest.version, bosun_common::version::VERSION);
        let mac = &manifest.artifacts["aarch64-apple-darwin"];
        assert_eq!(mac.sha256, sha256_of(b"mac"));
        assert_eq!(mac.size, 3);
        let linux = &manifest.artifacts["x86_64-unknown-linux-musl"];
        assert_eq!(linux.sha256, sha256_of(b"linux"));
        assert_eq!(linux.size, 5);
    }

    #[tokio::test]
    async fn manifest_is_stable_across_calls_for_unchanged_files() {
        let dir = tempdir().unwrap();
        let store = store_with(
            &dir.path().join("artifacts"),
            &[("bosun.aarch64-apple-darwin", b"mac")],
        );

        let first = store.manifest().await.unwrap();
        let second = store.manifest().await.unwrap();
        assert_eq!(first.artifacts, second.artifacts);
    }

    #[tokio::test]
    async fn manifest_rehashes_when_a_file_changes_and_its_mtime_moves_on() {
        let dir = tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        let file = artifacts.join("bosun.x86_64-unknown-linux-musl");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(&file, b"one").unwrap();
        let store = ArtifactStore::new(artifacts);

        let first = store.manifest().await.unwrap();

        std::fs::write(&file, b"two").unwrap();
        let modified = SystemTime::now() + Duration::from_secs(1);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        let second = store.manifest().await.unwrap();
        assert_ne!(
            first.artifacts["x86_64-unknown-linux-musl"].sha256,
            second.artifacts["x86_64-unknown-linux-musl"].sha256
        );
        assert_eq!(
            second.artifacts["x86_64-unknown-linux-musl"].sha256,
            sha256_of(b"two")
        );
    }

    #[tokio::test]
    async fn manifest_keeps_the_cached_hash_when_only_content_changes() {
        let dir = tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        let file = artifacts.join("bosun.x86_64-unknown-linux-musl");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(&file, b"one").unwrap();
        let store = ArtifactStore::new(artifacts);

        let first = store.manifest().await.unwrap();
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();

        std::fs::write(&file, b"two").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        let second = store.manifest().await.unwrap();
        assert_eq!(
            first.artifacts["x86_64-unknown-linux-musl"].sha256,
            second.artifacts["x86_64-unknown-linux-musl"].sha256
        );
    }

    #[tokio::test]
    async fn targets_without_a_file_are_absent() {
        let dir = tempdir().unwrap();
        let store = store_with(
            &dir.path().join("artifacts"),
            &[("bosun.aarch64-apple-darwin", b"x")],
        );

        let manifest = store.manifest().await.unwrap();
        assert!(manifest.artifacts.contains_key("aarch64-apple-darwin"));
        assert!(!manifest.artifacts.contains_key("x86_64-unknown-linux-musl"));
    }

    #[tokio::test]
    async fn non_artifact_files_and_directories_are_ignored() {
        let dir = tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(artifacts.join("README.md"), b"docs").unwrap();
        std::fs::write(artifacts.join("bosun"), b"no triple").unwrap();
        std::fs::create_dir_all(artifacts.join("bosun.aarch64-apple-darwin")).unwrap();
        std::fs::write(artifacts.join("bosun.x86_64-unknown-linux-musl"), b"x").unwrap();
        let store = ArtifactStore::new(artifacts);

        let manifest = store.manifest().await.unwrap();
        assert!(manifest.artifacts.contains_key("x86_64-unknown-linux-musl"));
        assert_eq!(manifest.artifacts.len(), 1);
    }

    #[tokio::test]
    async fn missing_directory_is_an_empty_manifest() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().join("does-not-exist"));

        let manifest = store.manifest().await.unwrap();
        assert!(manifest.artifacts.is_empty());
    }

    #[tokio::test]
    async fn cache_drops_entries_for_removed_files() {
        let dir = tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        let file = artifacts.join("bosun.x86_64-unknown-linux-musl");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(&file, b"x").unwrap();
        let store = ArtifactStore::new(artifacts.clone());

        store.manifest().await.unwrap();
        assert_eq!(store.cache.lock().unwrap().len(), 1);

        std::fs::remove_file(&file).unwrap();
        store.manifest().await.unwrap();
        assert!(store.cache.lock().unwrap().is_empty());
    }

    #[test]
    fn artifact_path_names_files_after_the_target_triple() {
        let store = ArtifactStore::new(PathBuf::from("/srv/artifacts"));
        assert_eq!(
            store.artifact_path("aarch64-apple-darwin"),
            Some(PathBuf::from("/srv/artifacts/bosun.aarch64-apple-darwin"))
        );
        assert_eq!(
            store.artifact_path("x86_64-pc-windows-msvc"),
            Some(PathBuf::from("/srv/artifacts/bosun.x86_64-pc-windows-msvc"))
        );
    }

    #[test]
    fn artifact_path_rejects_targets_that_cannot_name_a_file() {
        let store = ArtifactStore::new(PathBuf::from("/srv/artifacts"));
        assert_eq!(store.artifact_path(""), None);
        assert_eq!(store.artifact_path("../etc/passwd"), None);
        assert_eq!(store.artifact_path("a/b"), None);
        assert_eq!(store.artifact_path("a b"), None);
    }
}
