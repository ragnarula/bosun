use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::anyhow;
use bosun_common::session::Permission;
use bosun_common::skills::Skill;
use bosun_common::skills::parse_skill_dir;
use bosun_common::skills::read_skill_markdown;
use futures_util::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

const MAX_FILE_BYTES: usize = 1 << 20;
const MAX_GREP_MATCHES: usize = 500;
const MAX_GREP_FILES: usize = 10_000;
const MAX_GREP_LINE_CHARS: usize = 500;
const MAX_GLOB_RESULTS: usize = 1000;
const MAX_BODY_BYTES: usize = 1 << 20;
const GIT_READ_VERBS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "describe",
    "grep",
    "show-ref",
    "rev-list",
    "ls-tree",
    "cat-file",
];
const GIT_WRITE_VERBS: &[&str] = &["add", "commit"];

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool {tool} requires read-write permission")]
    ReadOnly { tool: &'static str },
    #[error("path {path} is outside the session directory")]
    PathOutsideRoot { path: String },
    #[error("file {path} was not found")]
    NotFound { path: String },
    #[error("file {path} exceeds {MAX_FILE_BYTES} bytes")]
    FileTooLarge { path: String },
    #[error("the text to replace was not found")]
    OldTextNotFound,
    #[error("search exceeded {limit} matches")]
    TooManyMatches { limit: usize },
    #[error("search exceeded {limit} results")]
    TooManyResults { limit: usize },
    #[error("unsupported URL {url}: only http and https are allowed")]
    UnsupportedUrl { url: String },
    #[error("git verb {verb} is not allowed")]
    GitVerbNotAllowed { verb: String },
    #[error("git push is forbidden")]
    GitPushForbidden,
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// Resolve `relative` under `root`, rejecting `..` escapes and absolute paths.
pub fn resolve_path(root: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize session dir {}", root.display()))?;
    let joined = root.join(relative);
    let resolved = resolve_target(&joined).ok_or_else(|| ToolError::NotFound {
        path: relative.to_string(),
    })?;
    if !resolved.starts_with(&root) {
        return Err(ToolError::PathOutsideRoot {
            path: relative.to_string(),
        });
    }
    Ok(resolved)
}

/// Canonical path of `joined`; when it does not exist, of its `..`-normalized
/// form; when that does not exist either, of its parent plus the file name.
/// The last case is the write path: the file does not exist yet, but resolving
/// the parent still catches `..` and symlinks between root and the file.
fn resolve_target(joined: &Path) -> Option<PathBuf> {
    joined.canonicalize().ok().or_else(|| {
        let normalized = normalize_lexical(joined);
        normalized.canonicalize().ok().or_else(|| {
            let file_name = normalized.file_name()?;
            let parent = normalized.parent()?.canonicalize().ok()?;
            Some(parent.join(file_name))
        })
    })
}

/// Resolve `..` and `.` components without touching the filesystem, so escape
/// attempts through directories that do not exist yet are still caught.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub fn read_file(session_dir: &Path, path: &str) -> Result<String, ToolError> {
    let resolved = resolve_path(session_dir, path)?;
    if !resolved.is_file() {
        return Err(ToolError::NotFound {
            path: path.to_string(),
        });
    }
    let metadata = resolved
        .metadata()
        .with_context(|| format!("failed to stat {}", resolved.display()))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(ToolError::FileTooLarge {
            path: path.to_string(),
        });
    }
    let bytes = std::fs::read(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The session's skills as parsed metadata, sorted by name. Skills live in
/// the working copy on the node, so discovery happens here rather than on
/// the control plane's filesystem.
pub fn list_skills(session_dir: &Path) -> Vec<Skill> {
    parse_skill_dir(&session_dir.join(".agents").join("skills"))
}

/// The full text of the skill named `name`, looked up by its parsed name.
pub fn read_skill(session_dir: &Path, name: &str) -> Result<String, ToolError> {
    read_skill_markdown(&session_dir.join(".agents").join("skills"), name).ok_or_else(|| {
        ToolError::NotFound {
            path: name.to_string(),
        }
    })
}

pub fn write_file(session_dir: &Path, path: &str, content: &str) -> Result<(), ToolError> {
    let root = session_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize session dir {}",
            session_dir.display()
        )
    })?;
    let target = resolve_write_target(&root, path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create dir {}", parent.display()))?;
    }
    std::fs::write(&target, content)
        .with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
}

/// Resolve a write target that does not exist yet, creating subdirectories.
/// The path is lexically normalized to catch `..` even through directories
/// that do not exist, the nearest existing ancestor is canonicalized so
/// symlinks cannot smuggle the write outside the root, and a target that
/// already exists must itself canonicalize back inside the root.
fn resolve_write_target(root: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let normalized = normalize_lexical(&root.join(relative));
    if normalized == root {
        return Err(ToolError::NotFound {
            path: relative.to_string(),
        });
    }
    if !normalized.starts_with(root) {
        return Err(ToolError::PathOutsideRoot {
            path: relative.to_string(),
        });
    }
    let file_name = normalized.file_name().ok_or_else(|| ToolError::NotFound {
        path: relative.to_string(),
    })?;
    let parent = normalized.parent().unwrap_or(root);
    let anchor = nearest_existing_ancestor(parent);
    let anchor_canonical = anchor
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", anchor.display()))?;
    if !anchor_canonical.starts_with(root) {
        return Err(ToolError::PathOutsideRoot {
            path: relative.to_string(),
        });
    }
    let tail = parent
        .strip_prefix(anchor)
        .map_err(|_| ToolError::Internal(anyhow!("failed to resolve write target {relative}")))?;
    let target = anchor_canonical.join(tail).join(file_name);
    if let Ok(canonical) = target.canonicalize()
        && !canonical.starts_with(root)
    {
        return Err(ToolError::PathOutsideRoot {
            path: relative.to_string(),
        });
    }
    Ok(target)
}

/// The deepest existing ancestor of `path`, so the existing prefix of a write
/// target can be canonicalized even when the file's subdirectories do not
/// exist yet.
fn nearest_existing_ancestor(path: &Path) -> &Path {
    let mut current = path;
    while !current.exists() {
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    current
}

pub fn edit(session_dir: &Path, path: &str, old: &str, new: &str) -> Result<(), ToolError> {
    let resolved = resolve_path(session_dir, path)?;
    if !resolved.is_file() {
        return Err(ToolError::NotFound {
            path: path.to_string(),
        });
    }
    let metadata = resolved
        .metadata()
        .with_context(|| format!("failed to stat {}", resolved.display()))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(ToolError::FileTooLarge {
            path: path.to_string(),
        });
    }
    let content = std::fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    if !content.contains(old) {
        return Err(ToolError::OldTextNotFound);
    }
    let updated = content.replacen(old, new, 1);
    std::fs::write(&resolved, updated)
        .with_context(|| format!("failed to write {}", resolved.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

pub fn grep(
    session_dir: &Path,
    pattern: &str,
    path: Option<&str>,
) -> Result<Vec<GrepMatch>, ToolError> {
    let re = regex::Regex::new(pattern)
        .with_context(|| format!("failed to compile regex {pattern:?}"))?;
    let root = session_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize session dir {}",
            session_dir.display()
        )
    })?;

    let mut files = Vec::new();
    match path {
        Some(relative) => {
            let target = resolve_path(&root, relative)?;
            if target.is_dir() {
                collect_files(&target, &mut files, &mut 0)?;
            } else if target.is_file() {
                files.push(target);
            } else {
                return Err(ToolError::NotFound {
                    path: relative.to_string(),
                });
            }
        }
        None => collect_files(&root, &mut files, &mut 0)?,
    }

    let mut matches = Vec::new();
    for file in files {
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to stat {}", file.display()))?;
        if metadata.len() > MAX_FILE_BYTES as u64 {
            continue;
        }
        let bytes =
            std::fs::read(&file).with_context(|| format!("failed to read {}", file.display()))?;
        let content = String::from_utf8_lossy(&bytes);
        let relative = file
            .strip_prefix(&root)
            .expect("collected files stay under the session dir")
            .to_string_lossy();
        for (index, line) in content.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            if matches.len() >= MAX_GREP_MATCHES {
                return Err(ToolError::TooManyMatches {
                    limit: MAX_GREP_MATCHES,
                });
            }
            matches.push(GrepMatch {
                path: relative.to_string(),
                line: index + 1,
                text: line.chars().take(MAX_GREP_LINE_CHARS).collect(),
            });
        }
    }
    Ok(matches)
}

fn collect_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    files_visited: &mut usize,
) -> Result<(), ToolError> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("failed to read dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(&path, out, files_visited)?;
        } else if file_type.is_file() {
            if *files_visited >= MAX_GREP_FILES {
                return Err(ToolError::TooManyResults {
                    limit: MAX_GREP_FILES,
                });
            }
            *files_visited += 1;
            out.push(path);
        }
    }
    Ok(())
}

pub fn glob(session_dir: &Path, pattern: &str) -> Result<Vec<String>, ToolError> {
    let full_pattern = session_dir.join(pattern);
    let paths = glob::glob(&full_pattern.to_string_lossy())
        .with_context(|| format!("failed to parse glob pattern {pattern:?}"))?;
    let mut results = Vec::new();
    for path in paths {
        let path = path.with_context(|| format!("failed to read glob match for {pattern:?}"))?;
        let Ok(relative) = path.strip_prefix(session_dir) else {
            continue;
        };
        if relative
            .components()
            .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if results.len() >= MAX_GLOB_RESULTS {
            return Err(ToolError::TooManyResults {
                limit: MAX_GLOB_RESULTS,
            });
        }
        results.push(relative.to_string_lossy().into_owned());
    }
    results.sort();
    Ok(results)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn git(
    session_dir: &Path,
    permission: Permission,
    args: &[String],
) -> Result<GitOutput, ToolError> {
    if args.iter().any(|arg| arg == "-C") {
        // `-C <dir>` would run git in another directory, so it is refused
        // outright.
        return Err(ToolError::GitVerbNotAllowed {
            verb: "-C".to_string(),
        });
    }
    let verb = git_verb(args).ok_or(ToolError::Internal(anyhow!(
        "git requires at least one argument"
    )))?;
    if verb == "push" {
        return Err(ToolError::GitPushForbidden);
    }
    let is_read = GIT_READ_VERBS.contains(&verb);
    let is_write = GIT_WRITE_VERBS.contains(&verb);
    if !is_read && !is_write {
        return Err(ToolError::GitVerbNotAllowed {
            verb: verb.to_string(),
        });
    }
    if is_write && permission != Permission::ReadWrite {
        return Err(ToolError::ReadOnly { tool: "git" });
    }
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(session_dir)
        .output()
        .await
        .with_context(|| format!("failed to run git in {}", session_dir.display()))?;
    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// The first argument is the git verb.
fn git_verb(args: &[String]) -> Option<&str> {
    args.first().map(String::as_str)
}

pub async fn webfetch(url: &str) -> Result<String, ToolError> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("failed to parse URL {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::UnsupportedUrl {
            url: url.to_string(),
        });
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed to read response body from {url}"))?;
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            body.extend_from_slice(&chunk[..MAX_BODY_BYTES - body.len()]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

pub fn permission_from_str(s: &str) -> Option<Permission> {
    let normalized = s.replace('-', "_");
    match normalized.as_str() {
        "read_only" => Some(Permission::ReadOnly),
        "read_write" => Some(Permission::ReadWrite),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bosun_test_support::git_quiet;
    use bosun_test_support::init_repo;

    use super::*;

    #[test]
    fn resolve_path_resolves_within_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/file.txt"), "x").unwrap();

        let resolved = resolve_path(root, "a/b/file.txt").unwrap();
        assert_eq!(resolved, root.join("a/b/file.txt").canonicalize().unwrap());
    }

    #[test]
    fn resolve_path_rejects_escapes_and_absolute_paths() {
        let parent = tempfile::tempdir().unwrap();
        let session = parent.path().join("session");
        std::fs::create_dir(&session).unwrap();
        std::fs::write(parent.path().join("secret.txt"), "x").unwrap();

        for relative in ["../secret.txt", "../", "a/../../secret.txt", "/etc/hosts"] {
            let err = resolve_path(&session, relative).unwrap_err();
            assert!(
                matches!(err, ToolError::PathOutsideRoot { .. }),
                "{relative}"
            );
        }
    }

    #[test]
    fn resolve_path_reports_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A missing file whose parent exists resolves to a write target.
        assert_eq!(
            resolve_path(root, "missing.txt").unwrap(),
            root.canonicalize().unwrap().join("missing.txt")
        );

        // A path with no existing ancestor cannot be resolved.
        let err = resolve_path(root, "a/b/missing.txt").unwrap_err();
        assert!(matches!(err, ToolError::NotFound { .. }));
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "hello.txt", "hi there").unwrap();
        assert_eq!(read_file(root, "hello.txt").unwrap(), "hi there");

        std::fs::create_dir_all(root.join("sub")).unwrap();
        write_file(root, "sub/nested.txt", "deep").unwrap();
        assert_eq!(read_file(root, "sub/nested.txt").unwrap(), "deep");

        // Writing into subdirectories that do not exist yet creates them.
        write_file(root, "nested/new/file.txt", "fresh").unwrap();
        assert_eq!(read_file(root, "nested/new/file.txt").unwrap(), "fresh");
    }

    #[test]
    fn read_file_reports_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_file(dir.path(), "missing.txt").unwrap_err();
        assert!(matches!(err, ToolError::NotFound { .. }));
    }

    #[test]
    fn read_and_edit_reject_files_larger_than_1_mib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "big.txt", &"x".repeat(MAX_FILE_BYTES + 1)).unwrap();

        let err = read_file(root, "big.txt").unwrap_err();
        assert!(matches!(err, ToolError::FileTooLarge { .. }));

        let err = edit(root, "big.txt", "a", "b").unwrap_err();
        assert!(matches!(err, ToolError::FileTooLarge { .. }));
    }

    #[test]
    fn read_file_rejects_symlink_escapes() {
        let parent = tempfile::tempdir().unwrap();
        let session = parent.path().join("session");
        std::fs::create_dir(&session).unwrap();
        std::fs::create_dir(parent.path().join("outside")).unwrap();
        std::fs::write(parent.path().join("outside/secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(parent.path().join("outside"), session.join("link")).unwrap();

        let err = read_file(&session, "link/secret.txt").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideRoot { .. }));
    }

    #[test]
    fn write_file_rejects_symlink_escapes() {
        let parent = tempfile::tempdir().unwrap();
        let session = parent.path().join("session");
        std::fs::create_dir(&session).unwrap();
        std::fs::create_dir(parent.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(parent.path().join("outside"), session.join("link")).unwrap();

        let err = write_file(&session, "link/new.txt", "boom").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideRoot { .. }));
        assert!(!parent.path().join("outside/new.txt").exists());
    }

    #[test]
    fn write_file_rejects_symlinked_target_outside_root() {
        let parent = tempfile::tempdir().unwrap();
        let session = parent.path().join("session");
        std::fs::create_dir(&session).unwrap();
        let outside = parent.path().join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        std::os::unix::fs::symlink(&outside, session.join("evil.txt")).unwrap();

        let err = write_file(&session, "evil.txt", "boom").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideRoot { .. }));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "x");
    }

    #[test]
    fn write_file_rejects_escaping_paths() {
        let parent = tempfile::tempdir().unwrap();
        let session = parent.path().join("session");
        std::fs::create_dir(&session).unwrap();

        let err = write_file(&session, "../evil.txt", "boom").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideRoot { .. }));
        assert!(!parent.path().join("evil.txt").exists());
    }

    #[test]
    fn edit_replaces_first_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "f.txt", "a a a").unwrap();
        edit(root, "f.txt", "a", "b").unwrap();
        assert_eq!(read_file(root, "f.txt").unwrap(), "b a a");
    }

    #[test]
    fn edit_errors_when_old_text_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "f.txt", "hello").unwrap();
        let err = edit(root, "f.txt", "xyz", "abc").unwrap_err();
        assert!(matches!(err, ToolError::OldTextNotFound));
        assert_eq!(read_file(root, "f.txt").unwrap(), "hello");
    }

    #[test]
    fn grep_finds_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "f.txt", "foo\nbar\nfoo baz").unwrap();
        let matches = grep(root, "foo", None).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "f.txt");
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[0].text, "foo");
        assert_eq!(matches[1].line, 3);
        assert_eq!(matches[1].text, "foo baz");
    }

    #[test]
    fn grep_skips_hidden_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "visible.txt", "needle").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_file(root, ".git/config", "needle").unwrap();
        write_file(root, ".hidden.txt", "needle").unwrap();

        let matches = grep(root, "needle", None).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "visible.txt");
    }

    #[test]
    fn grep_narrows_to_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "root.txt", "needle").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        write_file(root, "sub/inner.txt", "needle").unwrap();

        let matches = grep(root, "needle", Some("sub")).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "sub/inner.txt");

        let matches = grep(root, "needle", Some("root.txt")).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "root.txt");
    }

    #[test]
    fn grep_caps_at_500_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "many.txt", &"needle\n".repeat(600)).unwrap();
        let err = grep(root, "needle", None).unwrap_err();
        assert!(matches!(err, ToolError::TooManyMatches { limit: 500 }));
    }

    #[test]
    fn grep_skips_files_larger_than_1_mib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(
            root,
            "big.txt",
            &format!("{}\nneedle", "x".repeat(1024 * 1024)),
        )
        .unwrap();
        write_file(root, "small.txt", "needle").unwrap();

        let matches = grep(root, "needle", None).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "small.txt");
    }

    #[test]
    fn grep_caps_files_visited_at_10000() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..MAX_GREP_FILES + 1 {
            write_file(root, &format!("f{i:05}.txt"), "").unwrap();
        }
        let err = grep(root, "needle", None).unwrap_err();
        assert!(matches!(err, ToolError::TooManyResults { limit: 10_000 }));
    }

    #[test]
    fn grep_caps_matched_line_text_at_500_chars() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let long_line = format!("start{}", "x".repeat(1000));
        write_file(root, "long.txt", &long_line).unwrap();

        let matches = grep(root, "start", None).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text.len(), 500);
    }

    #[test]
    fn glob_returns_sorted_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        write_file(root, "src/b.rs", "").unwrap();
        write_file(root, "src/a.rs", "").unwrap();
        write_file(root, "Cargo.toml", "").unwrap();

        let paths = glob(root, "**/*.rs").unwrap();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn glob_filters_hidden_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_file(root, ".git/config", "").unwrap();
        write_file(root, "visible.txt", "").unwrap();

        let paths = glob(root, "**/*").unwrap();
        assert_eq!(paths, vec!["visible.txt"]);
    }

    #[test]
    fn glob_caps_results_at_1000() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..MAX_GLOB_RESULTS + 1 {
            write_file(root, &format!("f{i:04}.txt"), "").unwrap();
        }
        let err = glob(root, "**/*").unwrap_err();
        assert!(matches!(err, ToolError::TooManyResults { limit: 1000 }));
    }

    #[tokio::test]
    async fn git_read_verbs_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("f.txt"), "x").unwrap();
        git_quiet(root, &["add", "."]);
        git_quiet(root, &["commit", "-q", "-m", "init"]);

        let out = git(root, Permission::ReadWrite, &["status".to_string()])
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);

        let out = git(root, Permission::ReadWrite, &["log".to_string()])
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("init"));

        let out = git(root, Permission::ReadWrite, &["diff".to_string()])
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn git_rejects_push_in_any_form() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let err = git(root, Permission::ReadWrite, &["push".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::GitPushForbidden));

        // `-C <dir>` would run git in another directory, so it is refused
        // before the verb is even considered.
        let err = git(
            root,
            Permission::ReadWrite,
            &["-C".to_string(), ".".to_string(), "push".to_string()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::GitVerbNotAllowed { ref verb } if verb == "-C"));
    }

    #[tokio::test]
    async fn git_refuses_mutating_read_verbs_even_with_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        // `branch`, `tag`, `config` and `remote` were removed from the read
        // allowlist because they accept mutating subcommands.
        for args in [
            vec!["branch".to_string(), "foo".to_string()],
            vec!["tag".to_string(), "v1".to_string()],
            vec!["config".to_string(), "x".to_string()],
            vec![
                "remote".to_string(),
                "add".to_string(),
                "origin".to_string(),
                "https://example.com/repo.git".to_string(),
            ],
        ] {
            let err = git(root, Permission::ReadWrite, &args).await.unwrap_err();
            assert!(
                matches!(err, ToolError::GitVerbNotAllowed { .. }),
                "args {args:?}"
            );
        }
    }

    #[tokio::test]
    async fn git_rejects_disallowed_verbs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let err = git(
            root,
            Permission::ReadWrite,
            &["checkout".to_string(), "master".to_string()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::GitVerbNotAllowed { .. }));
    }

    #[tokio::test]
    async fn git_write_verbs_require_read_write_permission() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let err = git(
            root,
            Permission::ReadOnly,
            &["add".to_string(), ".".to_string()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::ReadOnly { tool: "git" }));
    }

    #[tokio::test]
    async fn git_commit_with_read_write_works() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("f.txt"), "x").unwrap();

        let out = git(
            root,
            Permission::ReadWrite,
            &["add".to_string(), ".".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0);

        let out = git(
            root,
            Permission::ReadWrite,
            &["commit".to_string(), "-m".to_string(), "init".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0);

        let out = git(root, Permission::ReadWrite, &["log".to_string()])
            .await
            .unwrap();
        assert!(out.stdout.contains("init"));
    }

    #[tokio::test]
    async fn git_nonzero_exit_returns_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let out = git(root, Permission::ReadWrite, &["status".to_string()])
            .await
            .unwrap();
        assert_ne!(out.exit_code, 0);
        assert!(!out.stderr.is_empty());
    }

    #[tokio::test]
    async fn webfetch_returns_body_from_local_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app =
            axum::Router::new().route("/", axum::routing::get(|| async { "hello from axum" }));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let body = webfetch(&format!("http://{addr}/")).await.unwrap();
        assert_eq!(body, "hello from axum");
    }

    #[tokio::test]
    async fn webfetch_truncates_bodies_larger_than_1_mib() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let big = "x".repeat(2 * MAX_BODY_BYTES);
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let big = big.clone();
                async move { big }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let body = webfetch(&format!("http://{addr}/")).await.unwrap();
        assert!(
            body.len() <= MAX_BODY_BYTES,
            "body was {} bytes, cap is {MAX_BODY_BYTES}",
            body.len()
        );
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn webfetch_rejects_non_http_schemes() {
        for url in ["file:///etc/passwd", "ftp://example.com/file.txt"] {
            let err = webfetch(url).await.unwrap_err();
            assert!(
                matches!(err, ToolError::UnsupportedUrl { .. }),
                "url: {url}"
            );
        }
    }

    #[test]
    fn permission_from_str_maps_known_values() {
        assert_eq!(permission_from_str("read_only"), Some(Permission::ReadOnly));
        assert_eq!(
            permission_from_str("read_write"),
            Some(Permission::ReadWrite)
        );
        // Hyphenated forms normalize to snake_case.
        assert_eq!(permission_from_str("read-only"), Some(Permission::ReadOnly));
        assert_eq!(
            permission_from_str("read-write"),
            Some(Permission::ReadWrite)
        );
        assert_eq!(permission_from_str("admin"), None);
        assert_eq!(permission_from_str(""), None);
    }

    #[test]
    fn grep_match_serializes_snake_case() {
        let m = GrepMatch {
            path: "a/b.rs".into(),
            line: 3,
            text: "fn main()".into(),
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"path": "a/b.rs", "line": 3, "text": "fn main()"})
        );
    }

    #[test]
    fn git_output_round_trips() {
        let out = GitOutput {
            stdout: "stdout".into(),
            stderr: "stderr".into(),
            exit_code: 128,
        };
        let json = serde_json::to_value(&out).unwrap();
        let decoded: GitOutput = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.stdout, "stdout");
        assert_eq!(decoded.stderr, "stderr");
        assert_eq!(decoded.exit_code, 128);
    }
}
