//! Reads a working copy's git state for the project map: the branch, its
//! commits since the main branch, what is not committed, and the main
//! branch's newest commits. Every command is a read, and every argument is
//! passed to git separately, never through a shell.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use bosun_common::project::CommitInfo;
use bosun_common::project::DirtyFile;
use bosun_common::project::FileOp;
use bosun_common::project::GitState;
use bosun_common::project::MAX_CHANGED_PATHS;
use bosun_common::project::MAX_DIRTY_FILES;
use bosun_common::project::MAX_LANE_COMMITS;
use bosun_common::project::MAX_MAIN_COMMITS;
use bosun_common::project::MAX_REMOTE_BRANCHES;

use crate::tools::ToolError;

/// An untracked file larger than this is reported with no line count, so one
/// large build output does not cost a read of the whole file.
const MAX_COUNTED_FILE_BYTES: u64 = 1 << 20;

/// `git log` fields, separated by the unit separator, which no subject holds.
const LOG_FORMAT: &str = "--format=%H%x1f%s%x1f%an%x1f%ct";

/// The state of the working copy `dir` belongs to. None when `dir` is not in a
/// git working copy.
pub async fn read_git_state(dir: &Path) -> Result<Option<GitState>, ToolError> {
    let Some(top) = git(dir, &["rev-parse", "--show-toplevel", "--git-common-dir"]).await? else {
        return Ok(None);
    };
    let mut lines = top.lines();
    let (Some(root), Some(common)) = (lines.next(), lines.next()) else {
        return Ok(None);
    };
    let root = PathBuf::from(root);
    // A relative common directory is relative to the directory git ran in.
    let common_dir = dir.join(common);
    let common_dir = common_dir.canonicalize().unwrap_or(common_dir);

    let origin = git(&root, &["config", "--get", "remote.origin.url"])
        .await?
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty());
    let main_ref = main_ref(&root, origin.is_some()).await?;
    let main = match &main_ref {
        Some(main_ref) => log(
            &root,
            &["--first-parent", &format!("-{MAX_MAIN_COMMITS}"), main_ref],
        )
        .await?
        .into_iter()
        .map(|commit| CommitInfo {
            pushed: false,
            ..commit
        })
        .collect(),
        None => Vec::new(),
    };

    let status = git(
        &root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )
    .await?
    .unwrap_or_default();
    let status = parse_status(&status);
    let numstat = git(&root, &["diff", "HEAD", "--numstat", "-z"])
        .await?
        .map(|out| parse_numstat(&out))
        .unwrap_or_default();
    let mut dirty = Vec::new();
    for (path, op) in status.files.iter().take(MAX_DIRTY_FILES) {
        let (added, removed) = match numstat.get(path) {
            Some(counts) => *counts,
            None if *op == FileOp::Created => (count_lines(&root.join(path)).await, 0),
            None => (0, 0),
        };
        dirty.push(DirtyFile {
            path: path.clone(),
            op: *op,
            added,
            removed,
        });
    }

    let head = status.head.clone();
    let (fork, ahead, behind, commits, mut changed) = match (&head, &main_ref) {
        (Some(_), Some(main_ref)) => {
            let fork = git(&root, &["merge-base", "HEAD", main_ref])
                .await?
                .map(|sha| sha.trim().to_string());
            let (behind, ahead) = git(
                &root,
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{main_ref}...HEAD"),
                ],
            )
            .await?
            .map(|out| parse_counts(&out))
            .unwrap_or((0, 0));
            let (commits, changed) = match &fork {
                Some(fork) => (
                    log(
                        &root,
                        &[&format!("-{MAX_LANE_COMMITS}"), &format!("{fork}..HEAD")],
                    )
                    .await?,
                    git(&root, &["diff", "--name-only", "-z", fork, "HEAD"])
                        .await?
                        .map(|out| split_z(&out))
                        .unwrap_or_default(),
                ),
                None => (Vec::new(), Vec::new()),
            };
            (fork, ahead, behind, commits, changed)
        }
        _ => (None, 0, 0, Vec::new(), Vec::new()),
    };
    let commits = if origin.is_some() && !commits.is_empty() {
        let unpushed: HashSet<String> = git(
            &root,
            &[
                "rev-list",
                &format!("--max-count={}", MAX_LANE_COMMITS + 1),
                "HEAD",
                "--not",
                "--remotes=origin",
            ],
        )
        .await?
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default();
        commits
            .into_iter()
            .map(|commit| CommitInfo {
                pushed: !unpushed.contains(&commit.sha),
                ..commit
            })
            .collect()
    } else {
        commits
    };
    for file in &dirty {
        if !changed.contains(&file.path) {
            changed.push(file.path.clone());
        }
    }
    changed.truncate(MAX_CHANGED_PATHS);

    let remote_branches = if origin.is_some() {
        git(
            &root,
            &[
                "for-each-ref",
                "--format=%(refname:lstrip=3)",
                &format!("--count={MAX_REMOTE_BRANCHES}"),
                "refs/remotes/origin",
            ],
        )
        .await?
        .map(|out| {
            out.lines()
                .filter(|name| !name.is_empty() && *name != "HEAD")
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(Some(GitState {
        root: root.to_string_lossy().into_owned(),
        common_dir: common_dir.to_string_lossy().into_owned(),
        origin,
        main_ref,
        main,
        branch: status.branch,
        head,
        fork,
        ahead,
        behind,
        commits,
        changed,
        dirty,
        remote_branches,
    }))
}

/// The ref the main branch is read from: `origin`'s default branch, else
/// `origin/main` or `origin/master`, else a local `main` or `master`.
async fn main_ref(root: &Path, has_origin: bool) -> Result<Option<String>, ToolError> {
    if has_origin
        && let Some(name) = git(
            root,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .await?
    {
        return Ok(Some(name.trim().to_string()));
    }
    let candidates: &[&str] = if has_origin {
        &["origin/main", "origin/master", "main", "master"]
    } else {
        &["main", "master"]
    };
    for candidate in candidates {
        if git(root, &["rev-parse", "--verify", "--quiet", candidate])
            .await?
            .is_some()
        {
            return Ok(Some((*candidate).to_string()));
        }
    }
    Ok(None)
}

/// Runs git in `dir`. None when git exits non-zero: an unknown ref, a
/// repository with no commits, or a directory outside any repository.
async fn git(dir: &Path, args: &[&str]) -> Result<Option<String>, ToolError> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("failed to run git in {}", dir.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

async fn log(root: &Path, args: &[&str]) -> Result<Vec<CommitInfo>, ToolError> {
    let mut full = vec!["log", LOG_FORMAT];
    full.extend_from_slice(args);
    Ok(git(root, &full)
        .await?
        .map(|out| parse_log(&out))
        .unwrap_or_default())
}

fn parse_log(out: &str) -> Vec<CommitInfo> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split('\x1f');
            Some(CommitInfo {
                sha: fields.next()?.to_string(),
                subject: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                time_secs: fields.next()?.parse().ok()?,
                pushed: false,
            })
        })
        .collect()
}

/// `git rev-list --left-right --count A...B`: commits only in A, then only in B.
fn parse_counts(out: &str) -> (u32, u32) {
    let mut parts = out.split_whitespace().map(|part| part.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn split_z(out: &str) -> Vec<String> {
    out.split('\0')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Default, PartialEq)]
struct Status {
    branch: Option<String>,
    head: Option<String>,
    files: Vec<(String, FileOp)>,
}

/// `git status --porcelain=v2 --branch -z`. Each record ends with a NUL; a
/// rename's record is followed by one more holding the old path.
fn parse_status(out: &str) -> Status {
    let mut status = Status::default();
    let mut records = out.split('\0');
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            if let Some(oid) = header.strip_prefix("branch.oid ") {
                status.head = (oid != "(initial)").then(|| oid.to_string());
            } else if let Some(head) = header.strip_prefix("branch.head ") {
                status.branch = (head != "(detached)").then(|| head.to_string());
            }
            continue;
        }
        let (kind, fields) = match record.split_once(' ') {
            Some(split) => split,
            None => continue,
        };
        let (xy, path) = match kind {
            "1" => match fields.splitn(8, ' ').collect::<Vec<_>>()[..] {
                [xy, _, _, _, _, _, _, path] => (xy, path),
                _ => continue,
            },
            "2" => {
                records.next();
                match fields.splitn(9, ' ').collect::<Vec<_>>()[..] {
                    [xy, _, _, _, _, _, _, _, path] => (xy, path),
                    _ => continue,
                }
            }
            "u" => match fields.splitn(10, ' ').collect::<Vec<_>>()[..] {
                [xy, _, _, _, _, _, _, _, _, path] => (xy, path),
                _ => continue,
            },
            "?" => ("??", fields),
            _ => continue,
        };
        let op = if xy.contains('D') {
            FileOp::Deleted
        } else if xy == "??" || xy.starts_with('A') {
            FileOp::Created
        } else {
            FileOp::Edited
        };
        status.files.push((path.to_string(), op));
    }
    status
}

/// `git diff --numstat -z`: `added<TAB>removed<TAB>path<NUL>`, or for a rename
/// `added<TAB>removed<TAB><NUL>old<NUL>new<NUL>`. A binary file counts `-`,
/// read as zero.
fn parse_numstat(out: &str) -> BTreeMap<String, (u32, u32)> {
    let mut counts = BTreeMap::new();
    let mut records = out.split('\0');
    while let Some(record) = records.next() {
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            records.next();
            match records.next() {
                Some(new) => new,
                None => continue,
            }
        } else {
            path
        };
        counts.insert(
            path.to_string(),
            (added.parse().unwrap_or(0), removed.parse().unwrap_or(0)),
        );
    }
    counts
}

/// The lines in a new file: zero for a file too large to read, a binary file,
/// or one that cannot be read.
async fn count_lines(path: &Path) -> u32 {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return 0;
    };
    if !meta.is_file() || meta.len() > MAX_COUNTED_FILE_BYTES {
        return 0;
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return 0;
    };
    if bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
    let unterminated = usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    (newlines + unterminated) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Ann")
            .env("GIT_AUTHOR_EMAIL", "ann@example.com")
            .env("GIT_COMMITTER_NAME", "Ann")
            .env("GIT_COMMITTER_EMAIL", "ann@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn commit(dir: &Path, file: &str, text: &str, subject: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "-q", "-m", subject]);
    }

    #[test]
    fn status_reads_branch_head_and_each_kind_of_entry() {
        let out = "# branch.oid abc123\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +1 -0\0\
1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
1 A. N... 000000 100644 100644 000 ccc new.rs\0\
1 .D N... 100644 100644 000000 ddd ddd gone.rs\0\
2 R. N... 100644 100644 100644 eee eee R100 moved.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 f1 f2 f3 both.rs\0\
? untracked.txt\0";
        let status = parse_status(out);
        assert_eq!(status.branch.as_deref(), Some("feature"));
        assert_eq!(status.head.as_deref(), Some("abc123"));
        assert_eq!(
            status.files,
            vec![
                ("src/a b.rs".to_string(), FileOp::Edited),
                ("new.rs".to_string(), FileOp::Created),
                ("gone.rs".to_string(), FileOp::Deleted),
                ("moved.rs".to_string(), FileOp::Edited),
                ("both.rs".to_string(), FileOp::Edited),
                ("untracked.txt".to_string(), FileOp::Created),
            ]
        );
    }

    #[test]
    fn status_of_a_detached_head_and_an_empty_repository_has_no_branch_or_head() {
        let status = parse_status("# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(status.branch, None);
        assert_eq!(status.head, None);
    }

    #[test]
    fn numstat_reads_renames_and_binary_files() {
        let out = "3\t1\tsrc/a.rs\0-\t-\timage.png\0\
2\t0\t\0old.rs\0new.rs\0";
        let counts = parse_numstat(out);
        assert_eq!(counts.get("src/a.rs"), Some(&(3, 1)));
        assert_eq!(counts.get("image.png"), Some(&(0, 0)));
        assert_eq!(counts.get("new.rs"), Some(&(2, 0)));
        assert_eq!(counts.get("old.rs"), None);
    }

    #[tokio::test]
    async fn a_folder_outside_git_has_no_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_git_state(dir.path()).await.unwrap(), None);
    }

    /// A clone on a branch with one pushed and one local commit, an edit and a
    /// new file, read from a subdirectory of the working copy.
    #[tokio::test]
    async fn a_branch_reports_its_commits_fork_and_uncommitted_work() {
        let base = tempfile::tempdir().unwrap();
        let upstream = base.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        run(&upstream, &["init", "-q", "-b", "main"]);
        commit(&upstream, "a.txt", "one\n", "First");
        commit(&upstream, "b.txt", "two\n", "Second");

        let clone = base.path().join("clone");
        run(
            base.path(),
            &[
                "clone",
                "-q",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["checkout", "-q", "-b", "feature"]);
        commit(&clone, "c.txt", "three\n", "Pushed work");
        run(&clone, &["push", "-q", "origin", "feature"]);
        commit(&clone, "a.txt", "one\nmore\n", "Local work");
        commit(&upstream, "d.txt", "four\n", "Third on main");
        run(&clone, &["fetch", "-q"]);
        std::fs::write(clone.join("b.txt"), "two\nthree\nfour\n").unwrap();
        std::fs::create_dir(clone.join("sub")).unwrap();
        std::fs::write(clone.join("sub/new.txt"), "x\ny").unwrap();

        let state = read_git_state(&clone.join("sub")).await.unwrap().unwrap();

        assert_eq!(PathBuf::from(&state.root), clone.canonicalize().unwrap());
        assert_eq!(state.main_ref.as_deref(), Some("origin/main"));
        assert_eq!(
            state
                .main
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            ["Third on main", "Second", "First"]
        );
        assert_eq!(state.branch.as_deref(), Some("feature"));
        assert_eq!(state.fork.as_deref(), Some(state.main[1].sha.as_str()));
        assert_eq!((state.ahead, state.behind), (2, 1));
        assert_eq!(
            state
                .commits
                .iter()
                .map(|c| (c.subject.as_str(), c.pushed))
                .collect::<Vec<_>>(),
            [("Local work", false), ("Pushed work", true)]
        );
        assert_eq!(
            state.dirty,
            vec![
                DirtyFile {
                    path: "b.txt".into(),
                    op: FileOp::Edited,
                    added: 2,
                    removed: 0
                },
                DirtyFile {
                    path: "sub/new.txt".into(),
                    op: FileOp::Created,
                    added: 2,
                    removed: 0
                },
            ]
        );
        let mut changed = state.changed.clone();
        changed.sort();
        assert_eq!(changed, ["a.txt", "b.txt", "c.txt", "sub/new.txt"]);
        assert_eq!(state.remote_branches, ["feature", "main"]);
    }

    /// A linked worktree shares its repository's common directory.
    #[tokio::test]
    async fn a_worktree_names_the_repository_it_belongs_to() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        commit(&repo, "a.txt", "one\n", "First");
        let tree = base.path().join("tree");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                tree.to_str().unwrap(),
            ],
        );

        let main = read_git_state(&repo).await.unwrap().unwrap();
        let side = read_git_state(&tree).await.unwrap().unwrap();

        assert_eq!(main.common_dir, side.common_dir);
        assert_ne!(main.root, side.root);
        assert_eq!(side.branch.as_deref(), Some("side"));
        assert_eq!(side.main_ref.as_deref(), Some("main"));
        assert_eq!(side.origin, None);
        assert!(side.commits.iter().all(|c| !c.pushed));
    }
}
