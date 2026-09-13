use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::anyhow;
use bosun_common::skills::Skill;
use bosun_common::skills::parse_skill_dir;
use bosun_common::skills::read_skill_markdown;
use bosun_common::tool::SPILL_BYTE_LIMIT;
use bosun_common::tool::SPILL_LINE_LIMIT;
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
/// How many commits `history` lists when the caller names no limit.
const DEFAULT_LOG_LIMIT: usize = 20;
/// The most it will list, so one call cannot return a whole repository.
const MAX_LOG_LIMIT: usize = 200;

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
    #[error(
        "file {path} is too large to read in one call; pass offset and limit to read a range of lines"
    )]
    ReadTooLarge { path: String },
    #[error("the text to replace was not found")]
    OldTextNotFound,
    #[error("search exceeded {limit} matches")]
    TooManyMatches { limit: usize },
    #[error("search exceeded {limit} results")]
    TooManyResults { limit: usize },
    #[error("unsupported URL {url}: only http and https are allowed")]
    UnsupportedUrl { url: String },
    #[error("history op {op} is not one of diff, status, log, show")]
    HistoryOpNotAllowed { op: String },
    #[error("history {op} requires a ref")]
    HistoryRefMissing { op: &'static str },
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

/// Reads a line window of a file. `offset` is the 1-based number of the first
/// line to return and `limit` the maximum number of lines; a `None` limit reads
/// to the end of the file. The raw window never exceeds the spill limits
/// (`SPILL_LINE_LIMIT` lines and `SPILL_BYTE_LIMIT` bytes), so the caller can
/// return it inline; the executor additionally caps the serialized result, so
/// a `file_read` never spills to a file in the working copy. A read whose
/// window would exceed the raw limits is refused instead, so the model pages a
/// large file with offset and limit. The file is streamed, so a file of any
/// size costs memory bounded by the window, not by the file.
pub fn read_file(
    session_dir: &Path,
    path: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<String, ToolError> {
    let resolved = resolve_path(session_dir, path)?;
    if !resolved.is_file() {
        return Err(ToolError::NotFound {
            path: path.to_string(),
        });
    }
    let file =
        File::open(&resolved).with_context(|| format!("failed to read {}", resolved.display()))?;
    let mut reader = BufReader::new(file);

    // Skip the lines before the window. Lines are 1-based, so `offset - 1`
    // lines precede `offset`.
    let mut before = offset.saturating_sub(1);
    while before > 0 {
        if !skip_line(&mut reader)
            .with_context(|| format!("failed to read {}", resolved.display()))?
        {
            break;
        }
        before -= 1;
    }

    let mut content = Vec::new();
    let mut lines_read = 0usize;
    loop {
        if let Some(limit) = limit
            && lines_read >= limit
        {
            break;
        }
        match read_window_line(&mut reader, &mut content)
            .with_context(|| format!("failed to read {}", resolved.display()))?
        {
            WindowLine::None => break,
            WindowLine::Overflow => {
                return Err(ToolError::ReadTooLarge {
                    path: path.to_string(),
                });
            }
            WindowLine::Newline | WindowLine::Eof => {
                lines_read += 1;
                if lines_read > SPILL_LINE_LIMIT {
                    return Err(ToolError::ReadTooLarge {
                        path: path.to_string(),
                    });
                }
            }
        }
    }
    Ok(String::from_utf8_lossy(&content).into_owned())
}

/// Discards one line without buffering it, so a skipped line of any length
/// costs constant memory. Returns whether a line was consumed; an unterminated
/// final line at EOF counts, exactly like a newline-terminated one.
fn skip_line<R: BufRead>(reader: &mut R) -> std::io::Result<bool> {
    let mut saw_bytes = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(saw_bytes);
        }
        saw_bytes = true;
        if let Some(pos) = buf.iter().position(|&byte| byte == b'\n') {
            reader.consume(pos + 1);
            return Ok(true);
        }
        let len = buf.len();
        reader.consume(len);
    }
}

/// How one read line ended.
enum WindowLine {
    /// A newline-terminated line was appended to the window.
    Newline,
    /// An unterminated final line was appended to the window at EOF.
    Eof,
    /// EOF before any bytes: no line.
    None,
    /// The line would push the window past `SPILL_BYTE_LIMIT` bytes.
    Overflow,
}

/// Appends one line of the file to the window. The window never grows past
/// `SPILL_BYTE_LIMIT` bytes: a line that would push it over that cap stops the
/// read with `Overflow` without buffering the rest of the line.
fn read_window_line<R: BufRead>(
    reader: &mut R,
    content: &mut Vec<u8>,
) -> std::io::Result<WindowLine> {
    let start_len = content.len();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return if content.len() > start_len {
                Ok(WindowLine::Eof)
            } else {
                Ok(WindowLine::None)
            };
        }
        if let Some(pos) = buf.iter().position(|&byte| byte == b'\n') {
            let take = pos + 1;
            if content.len() + take > SPILL_BYTE_LIMIT {
                return Ok(WindowLine::Overflow);
            }
            content.extend_from_slice(&buf[..take]);
            reader.consume(take);
            return Ok(WindowLine::Newline);
        }
        if content.len() + buf.len() > SPILL_BYTE_LIMIT {
            return Ok(WindowLine::Overflow);
        }
        content.extend_from_slice(buf);
        let consumed = buf.len();
        reader.consume(consumed);
    }
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

/// The repo-standard files the executor scans for at the working-copy root,
/// listed in the order the presence notice advertises them.
pub const REPO_STANDARD_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// The repo-standard files that exist at the working-copy root, in
/// [`REPO_STANDARD_FILES`] order. Presence only: the response never carries
/// the files' contents, which the model reads on demand with `file_read`.
pub fn repo_standards_present(session_dir: &Path) -> Vec<&'static str> {
    REPO_STANDARD_FILES
        .into_iter()
        .filter(|name| session_dir.join(name).is_file())
        .collect()
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
pub struct HistoryOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Reads repository history: what changed, what is dirty, recent commits, and
/// one commit's contents.
///
/// This replaces a general `git` tool. Four operations cover 84% of the git
/// calls sessions actually made, and every one of them is a read, so there is
/// no verb allowlist to keep in step with git and no mutating verb reachable
/// through it. A session that needs to write history uses `shell`.
///
/// Arguments are passed to git as separate arguments, never through a shell,
/// so a path, ref, or pattern cannot inject another command. Paths are
/// resolved inside the session directory; refs are opaque to us and validated
/// by git itself, which fails the call rather than inventing a result.
pub struct HistoryQuery<'a> {
    pub op: &'a str,
    pub paths: &'a [String],
    pub git_ref: Option<&'a str>,
    pub summary: bool,
    pub limit: Option<usize>,
    pub grep: Option<&'a str>,
    pub author: Option<&'a str>,
}

pub async fn read_history(
    session_dir: &Path,
    query: HistoryQuery<'_>,
) -> Result<HistoryOutput, ToolError> {
    let HistoryQuery {
        op,
        paths,
        git_ref,
        summary,
        limit,
        grep,
        author,
    } = query;
    let mut args: Vec<String> = Vec::new();
    match op {
        "diff" => {
            args.push("diff".into());
            if let Some(reference) = git_ref {
                args.push(reference.to_string());
            }
            if summary {
                args.push("--stat".into());
            }
        }
        "status" => {
            args.push("status".into());
            args.push("--porcelain".into());
        }
        "log" => {
            let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT);
            args.push("log".into());
            args.push("--oneline".into());
            args.push(format!("-{limit}"));
            if let Some(pattern) = grep {
                args.push(format!("--grep={pattern}"));
            }
            if let Some(name) = author {
                args.push(format!("--author={name}"));
            }
            if let Some(reference) = git_ref {
                args.push(reference.to_string());
            }
        }
        "show" => {
            // `show` with no ref would print HEAD, which `diff` and `log`
            // already cover; naming the commit is the whole point of the op.
            let Some(reference) = git_ref else {
                return Err(ToolError::HistoryRefMissing { op: "show" });
            };
            args.push("show".into());
            args.push(reference.to_string());
            if summary {
                args.push("--stat".into());
            }
        }
        other => {
            return Err(ToolError::HistoryOpNotAllowed {
                op: other.to_string(),
            });
        }
    }
    // `--` separates paths from refs, so a file named like a branch cannot be
    // read as one. `status` takes paths too, but `show` does not.
    if !paths.is_empty() && op != "show" {
        args.push("--".into());
        for path in paths {
            resolve_path(session_dir, path)?;
            args.push(path.clone());
        }
    }
    let output = tokio::process::Command::new("git")
        .args(&args)
        .current_dir(session_dir)
        .output()
        .await
        .with_context(|| format!("failed to run git in {}", session_dir.display()))?;
    Ok(HistoryOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Reads a file as it stood at `git_ref`, via `git show <ref>:<path>`. The path
/// is resolved inside the session directory first and then made relative to it,
/// because git resolves a `<ref>:<path>` spec from the repository root.
pub async fn read_file_at_ref(
    session_dir: &Path,
    path: &str,
    git_ref: &str,
) -> Result<String, ToolError> {
    let resolved = resolve_path(session_dir, path)?;
    let relative = resolved.strip_prefix(session_dir).unwrap_or(&resolved);
    let spec = format!("{git_ref}:{}", relative.to_string_lossy());
    let output = tokio::process::Command::new("git")
        .arg("show")
        .arg(&spec)
        .current_dir(session_dir)
        .output()
        .await
        .with_context(|| format!("failed to run git in {}", session_dir.display()))?;
    if !output.status.success() {
        return Err(ToolError::NotFound {
            path: format!("{path} at {git_ref}"),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use bosun_test_support::git_quiet;
    use bosun_test_support::init_repo;

    use super::*;

    /// Reads a whole file with the default window: no offset and no limit.
    fn read_all(root: &Path, path: &str) -> Result<String, ToolError> {
        read_file(root, path, 1, None)
    }

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
    fn repo_standards_present_lists_root_files_in_canonical_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(repo_standards_present(root).is_empty());

        // A directory named AGENTS.md is not a standards file.
        std::fs::create_dir(root.join("AGENTS.md")).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "rules").unwrap();
        assert_eq!(
            repo_standards_present(root),
            vec!["CLAUDE.md"],
            "only files count, and only at the root"
        );

        std::fs::remove_dir(root.join("AGENTS.md")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "rules").unwrap();
        assert_eq!(
            repo_standards_present(root),
            vec!["AGENTS.md", "CLAUDE.md"],
            "both files list in canonical order"
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "hello.txt", "hi there").unwrap();
        assert_eq!(read_all(root, "hello.txt").unwrap(), "hi there");

        std::fs::create_dir_all(root.join("sub")).unwrap();
        write_file(root, "sub/nested.txt", "deep").unwrap();
        assert_eq!(read_all(root, "sub/nested.txt").unwrap(), "deep");

        // Writing into subdirectories that do not exist yet creates them.
        write_file(root, "nested/new/file.txt", "fresh").unwrap();
        assert_eq!(read_all(root, "nested/new/file.txt").unwrap(), "fresh");
    }

    #[test]
    fn read_file_reports_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_all(dir.path(), "missing.txt").unwrap_err();
        assert!(matches!(err, ToolError::NotFound { .. }));
    }

    #[test]
    fn edit_rejects_files_larger_than_1_mib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "big.txt", &"x".repeat(MAX_FILE_BYTES + 1)).unwrap();

        let err = edit(root, "big.txt", "a", "b").unwrap_err();
        assert!(matches!(err, ToolError::FileTooLarge { .. }));
    }

    #[test]
    fn read_file_returns_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "lines.txt", "one\ntwo\nthree\nfour\nfive\n").unwrap();

        // A window starts at `offset` and returns up to `limit` lines as raw
        // text, newline endings intact.
        assert_eq!(
            read_file(root, "lines.txt", 2, Some(3)).unwrap(),
            "two\nthree\nfour\n"
        );
        assert_eq!(read_file(root, "lines.txt", 4, Some(1)).unwrap(), "four\n");

        // The first window of a file with no limit reads to the end of the
        // file when it fits inline.
        assert_eq!(
            read_all(root, "lines.txt").unwrap(),
            "one\ntwo\nthree\nfour\nfive\n"
        );
        assert_eq!(
            read_file(root, "lines.txt", 1, Some(2)).unwrap(),
            "one\ntwo\n"
        );

        // An offset past the last line reads nothing, so the model can probe
        // for the end of the file.
        assert_eq!(read_file(root, "lines.txt", 6, None).unwrap(), "");
        assert_eq!(read_file(root, "lines.txt", 6, Some(10)).unwrap(), "");
    }

    #[test]
    fn read_file_treats_an_unterminated_final_line_as_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "f.txt", "one\ntwo\nthree").unwrap();

        assert_eq!(read_all(root, "f.txt").unwrap(), "one\ntwo\nthree");
        assert_eq!(read_file(root, "f.txt", 2, None).unwrap(), "two\nthree");
        assert_eq!(read_file(root, "f.txt", 3, Some(1)).unwrap(), "three");
        assert_eq!(read_file(root, "f.txt", 4, None).unwrap(), "");
    }

    #[test]
    fn read_file_handles_empty_files_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "empty.txt", "").unwrap();
        assert_eq!(read_all(root, "empty.txt").unwrap(), "");
        assert_eq!(read_file(root, "empty.txt", 1, Some(5)).unwrap(), "");

        // A blank line is one line: offset and limit count it like any other.
        write_file(root, "blank.txt", "one\n\nthree\n").unwrap();
        assert_eq!(read_all(root, "blank.txt").unwrap(), "one\n\nthree\n");
        assert_eq!(read_file(root, "blank.txt", 2, Some(1)).unwrap(), "\n");
        assert_eq!(
            read_file(root, "blank.txt", 2, Some(2)).unwrap(),
            "\nthree\n"
        );
    }

    #[test]
    fn read_file_refuses_a_window_larger_than_the_inline_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // More lines than fit inline.
        let lines = "x\n".repeat(SPILL_LINE_LIMIT + 1);
        write_file(root, "many.txt", &lines).unwrap();
        let err = read_all(root, "many.txt").unwrap_err();
        assert!(matches!(err, ToolError::ReadTooLarge { .. }));
        let err = read_file(root, "many.txt", 1, Some(SPILL_LINE_LIMIT + 1)).unwrap_err();
        assert!(matches!(err, ToolError::ReadTooLarge { .. }));

        // One line larger than the byte budget.
        write_file(root, "long.txt", &"x".repeat(SPILL_BYTE_LIMIT + 1)).unwrap();
        let err = read_all(root, "long.txt").unwrap_err();
        assert!(matches!(err, ToolError::ReadTooLarge { .. }));
        let err = read_file(root, "long.txt", 1, Some(1)).unwrap_err();
        assert!(matches!(err, ToolError::ReadTooLarge { .. }));

        // Enough lines that their total bytes exceed the budget while each
        // line alone fits.
        let wide = "x".repeat(SPILL_BYTE_LIMIT / 2);
        write_file(root, "wide.txt", &format!("{wide}\n{wide}\n")).unwrap();
        let err = read_all(root, "wide.txt").unwrap_err();
        assert!(matches!(err, ToolError::ReadTooLarge { .. }));

        // One of the two wide lines fits inline on its own.
        assert_eq!(
            read_file(root, "wide.txt", 1, Some(1)).unwrap(),
            format!("{wide}\n")
        );
    }

    #[test]
    fn read_file_pages_a_file_larger_than_the_inline_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut body = String::new();
        for i in 1..=SPILL_LINE_LIMIT * 2 {
            body.push_str(&format!("line {i} {}\n", "y".repeat(8)));
        }
        write_file(root, "big.txt", &body).unwrap();

        // The whole file does not fit inline.
        let err = read_all(root, "big.txt").unwrap_err();
        assert!(matches!(err, ToolError::ReadTooLarge { .. }));

        // Page through it in two inline-sized windows that cover the file.
        let first = read_file(root, "big.txt", 1, Some(SPILL_LINE_LIMIT)).unwrap();
        assert_eq!(
            first,
            format!(
                "{}\n",
                body.lines()
                    .take(SPILL_LINE_LIMIT)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        );
        let second = read_file(
            root,
            "big.txt",
            SPILL_LINE_LIMIT + 1,
            Some(SPILL_LINE_LIMIT),
        )
        .unwrap();
        let tail: Vec<&str> = body.lines().skip(SPILL_LINE_LIMIT).collect();
        assert_eq!(second, format!("{}\n", tail.join("\n")));
        assert_eq!(
            first.len() + second.len(),
            body.len(),
            "the two windows cover the whole file"
        );
        // Past the last line there is nothing more to read.
        assert_eq!(
            read_file(
                root,
                "big.txt",
                SPILL_LINE_LIMIT * 2 + 1,
                Some(SPILL_LINE_LIMIT)
            )
            .unwrap(),
            ""
        );
    }

    #[test]
    fn read_file_rejects_symlink_escapes() {
        let parent = tempfile::tempdir().unwrap();
        let session = parent.path().join("session");
        std::fs::create_dir(&session).unwrap();
        std::fs::create_dir(parent.path().join("outside")).unwrap();
        std::fs::write(parent.path().join("outside/secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(parent.path().join("outside"), session.join("link")).unwrap();

        let err = read_all(&session, "link/secret.txt").unwrap_err();
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
        assert_eq!(read_all(root, "f.txt").unwrap(), "b a a");
    }

    #[test]
    fn edit_errors_when_old_text_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(root, "f.txt", "hello").unwrap();
        let err = edit(root, "f.txt", "xyz", "abc").unwrap_err();
        assert!(matches!(err, ToolError::OldTextNotFound));
        assert_eq!(read_all(root, "f.txt").unwrap(), "hello");
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
    async fn webfetch_returns_body_from_local_server() {
        let body = serve_once(Arc::from(&b"hello from axum"[..])).await;

        let fetched = webfetch(&format!("http://{}/", body.addr)).await.unwrap();
        assert_eq!(fetched, "hello from axum");
    }

    #[tokio::test]
    async fn webfetch_truncates_bodies_larger_than_1_mib() {
        let body = serve_once(Arc::from(vec![b'x'; 2 * MAX_BODY_BYTES])).await;

        let fetched = webfetch(&format!("http://{}/", body.addr)).await.unwrap();
        assert!(
            fetched.len() <= MAX_BODY_BYTES,
            "body was {} bytes, cap is {MAX_BODY_BYTES}",
            fetched.len()
        );
        assert!(!fetched.is_empty());
    }

    /// One HTTP/1.1 response with `body` served per connection, until the
    /// listener is dropped.
    struct ServedBody {
        addr: SocketAddr,
        _guard: tokio::task::JoinHandle<()>,
    }

    async fn serve_once(body: Arc<[u8]>) -> ServedBody {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let guard = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    use tokio::io::AsyncWriteExt;
                    let mut buf = [0u8; 4096];
                    let mut read = 0;
                    while let Ok(n) = stream.read(&mut buf[read..]).await {
                        if n == 0 {
                            break;
                        }
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let header =
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        ServedBody {
            addr,
            _guard: guard,
        }
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
    fn history_output_round_trips() {
        let out = HistoryOutput {
            stdout: "stdout".into(),
            stderr: "stderr".into(),
            exit_code: 128,
        };
        let json = serde_json::to_value(&out).unwrap();
        let decoded: HistoryOutput = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.stdout, "stdout");
        assert_eq!(decoded.stderr, "stderr");
        assert_eq!(decoded.exit_code, 128);
    }

    /// The four ops each answer, and none of them can reach a mutating verb:
    /// the op is mapped to a fixed argument list, so there is no verb for a
    /// caller to name.
    #[tokio::test]
    async fn history_read_answers_each_op() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("f.txt"), "one\n").unwrap();
        git_quiet(root, &["add", "."]);
        git_quiet(root, &["commit", "-q", "-m", "first commit"]);
        std::fs::write(root.join("f.txt"), "two\n").unwrap();

        let diff = read_history(
            root,
            HistoryQuery {
                op: "diff",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap();
        assert!(diff.stdout.contains("-one"), "diff shows the change");

        let summary = read_history(
            root,
            HistoryQuery {
                op: "diff",
                paths: &[],
                git_ref: None,
                summary: true,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap();
        assert!(summary.stdout.contains("f.txt"), "a stat names the file");
        assert!(!summary.stdout.contains("-one"), "a stat is not a patch");

        let status = read_history(
            root,
            HistoryQuery {
                op: "status",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap();
        assert!(
            status.stdout.contains("f.txt"),
            "status lists the dirty file"
        );

        let log = read_history(
            root,
            HistoryQuery {
                op: "log",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap();
        assert!(log.stdout.contains("first commit"));

        let show = read_history(
            root,
            HistoryQuery {
                op: "show",
                paths: &[],
                git_ref: Some("HEAD"),
                summary: true,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap();
        assert!(show.stdout.contains("first commit"));
    }

    #[tokio::test]
    async fn history_read_filters_the_log_by_grep_and_author() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("a.txt"), "a").unwrap();
        git_quiet(root, &["add", "."]);
        git_quiet(root, &["commit", "-q", "-m", "alpha change"]);
        std::fs::write(root.join("b.txt"), "b").unwrap();
        git_quiet(root, &["add", "."]);
        git_quiet(root, &["commit", "-q", "-m", "beta change"]);

        let hit = read_history(
            root,
            HistoryQuery {
                op: "log",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: Some("alpha"),
                author: None,
            },
        )
        .await
        .unwrap();
        assert!(hit.stdout.contains("alpha"), "grep keeps the match");
        assert!(!hit.stdout.contains("beta"), "grep drops the rest");

        let nobody = read_history(
            root,
            HistoryQuery {
                op: "log",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: Some("nobody"),
            },
        )
        .await
        .unwrap();
        assert!(
            nobody.stdout.trim().is_empty(),
            "an unmatched author is empty"
        );
    }

    #[tokio::test]
    async fn history_read_refuses_an_unknown_op_and_a_show_without_a_ref() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let err = read_history(
            root,
            HistoryQuery {
                op: "push",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::HistoryOpNotAllowed { ref op } if op == "push"));

        let err = read_history(
            root,
            HistoryQuery {
                op: "show",
                paths: &[],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::HistoryRefMissing { op: "show" }));
    }

    /// The log limit is bounded, so one call cannot return a whole repository.
    #[tokio::test]
    async fn history_read_clamps_the_log_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("f.txt"), "x").unwrap();
        git_quiet(root, &["add", "."]);
        git_quiet(root, &["commit", "-q", "-m", "only commit"]);

        for limit in [Some(0), Some(usize::MAX), None] {
            let out = read_history(
                root,
                HistoryQuery {
                    op: "log",
                    paths: &[],
                    git_ref: None,
                    summary: false,
                    limit,
                    grep: None,
                    author: None,
                },
            )
            .await
            .unwrap();
            assert!(
                out.stdout.contains("only commit"),
                "limit {limit:?} still answers"
            );
        }
    }

    #[tokio::test]
    async fn file_read_at_a_ref_reads_the_committed_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("f.txt"), "committed\n").unwrap();
        git_quiet(root, &["add", "."]);
        git_quiet(root, &["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("f.txt"), "working copy\n").unwrap();

        let at_head = read_file_at_ref(root, "f.txt", "HEAD").await.unwrap();
        assert_eq!(
            at_head, "committed\n",
            "a ref read ignores the working copy"
        );

        let now = read_file(root, "f.txt", 1, None).unwrap();
        assert_eq!(
            now, "working copy\n",
            "a plain read still sees the working copy"
        );

        let missing = read_file_at_ref(root, "f.txt", "does-not-exist")
            .await
            .unwrap_err();
        assert!(matches!(missing, ToolError::NotFound { .. }));
    }

    /// A path is resolved inside the session directory before it reaches git,
    /// for both the history ops and a read at a ref.
    #[tokio::test]
    async fn history_and_ref_reads_refuse_a_path_outside_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let err = read_history(
            root,
            HistoryQuery {
                op: "diff",
                paths: &["../outside.txt".to_string()],
                git_ref: None,
                summary: false,
                limit: None,
                grep: None,
                author: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideRoot { .. }));

        let err = read_file_at_ref(root, "../outside.txt", "HEAD")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideRoot { .. }));
    }
}
