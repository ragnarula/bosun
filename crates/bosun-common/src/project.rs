//! What Bosun knows about the branches of a repository: the state a node reads
//! from one session's working copy, and the project view the control plane
//! builds from every working copy of the same repository.

use serde::Deserialize;
use serde::Serialize;

/// The newest commits of the main branch a node reads, first parent only.
pub const MAX_MAIN_COMMITS: usize = 12;
/// The newest commits since the fork point a node reads for one branch.
pub const MAX_LANE_COMMITS: usize = 30;
/// The paths a branch changed since its fork point that a node reports.
pub const MAX_CHANGED_PATHS: usize = 500;
/// The files with uncommitted changes a node reports.
pub const MAX_DIRTY_FILES: usize = 200;
/// The remote-tracking branches a node lists.
pub const MAX_REMOTE_BRANCHES: usize = 200;
/// The activity entries a project keeps.
pub const MAX_FEED_ENTRIES: usize = 50;

/// One commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub time_secs: i64,
    /// Whether a branch on `origin` already holds the commit. Always false for
    /// the main branch's commits, which are read from `origin` itself.
    #[serde(default)]
    pub pushed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Created,
    Edited,
    Deleted,
}

/// A file with changes that are not committed, and its line counts against
/// `HEAD`. An untracked file counts every line as added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyFile {
    pub path: String,
    pub op: FileOp,
    pub added: u32,
    pub removed: u32,
}

/// What a node reads from one session's working copy. The executor's
/// `git_state` call answers it; a directory that is not a git working copy
/// answers `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    /// The top of the working copy, which may be above the session's directory.
    pub root: String,
    /// The repository's shared git directory. Two working copies with the same
    /// common directory are worktrees of one repository.
    pub common_dir: String,
    /// `remote.origin.url`, as configured.
    pub origin: Option<String>,
    /// The ref the main branch is read from, such as `origin/main`, or a local
    /// `main` when the repository has no `origin`.
    pub main_ref: Option<String>,
    /// The main branch's newest commits, newest first.
    pub main: Vec<CommitInfo>,
    /// The checked-out branch. None when `HEAD` is detached.
    pub branch: Option<String>,
    /// None in a repository with no commits.
    pub head: Option<String>,
    /// The merge base of `HEAD` and the main branch.
    pub fork: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Commits since the fork point, newest first.
    pub commits: Vec<CommitInfo>,
    /// Paths changed since the fork point, committed or not.
    pub changed: Vec<String>,
    pub dirty: Vec<DirtyFile>,
    /// Branch names on `origin`, without the remote prefix.
    pub remote_branches: Vec<String>,
}

/// How the folder a lane lives in came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyKind {
    /// A clone Bosun made for a session.
    Clone,
    /// A folder the session was started in.
    Folder,
    /// A linked worktree of another folder.
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Draft,
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
}

/// A branch's newest pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
    /// None when the head commit has no checks.
    pub checks: Option<ChecksState>,
    /// None when nobody has approved or asked for changes.
    pub review: Option<ReviewState>,
    pub merged_at_secs: Option<i64>,
}

/// Whether the project's pull requests can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrSource {
    /// Read from GitHub.
    On,
    /// The repository is on GitHub and the control plane has no `github_token`.
    NoToken,
    /// The repository is not on GitHub.
    NotGithub,
    /// The last read from GitHub failed.
    Failing,
}

/// One session working copy, as a lane of the map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    /// `node:root`, stable while the folder exists.
    pub key: String,
    pub node: String,
    pub root: String,
    pub kind: CopyKind,
    /// For a worktree, the folder of the repository it belongs to.
    pub worktree_of: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    /// The fork point's position in the project's `main` list. None when the
    /// fork is older than the commits listed, or not known.
    pub fork_index: Option<u32>,
    pub ahead: u32,
    pub behind: u32,
    pub commits: Vec<CommitInfo>,
    pub dirty: Vec<DirtyFile>,
    /// The root sessions working in the folder.
    pub sessions: Vec<String>,
    pub pr: Option<PullRequest>,
}

/// A branch that was merged into the main branch recently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedLane {
    pub branch: String,
    pub pr: Option<u64>,
    pub title: Option<String>,
    pub commits: u32,
    pub merged_at_secs: i64,
}

/// A file that two lanes both change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    pub path: String,
    /// The two lanes' keys.
    pub lanes: [String; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    Created,
    Edited,
    Deleted,
    Committed,
    Pushed,
    Switched,
    PrOpened,
    Checks,
    Review,
    Merged,
    Overlap,
}

/// One thing that happened in a project. Clients write the words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedEntry {
    pub at_secs: i64,
    pub kind: FeedKind,
    /// The lane it happened on, by key.
    pub lane: Option<String>,
    pub branch: Option<String>,
    /// The session whose tool call made the change. None for a change made
    /// outside Bosun, and for news from GitHub.
    pub session: Option<String>,
    /// That session's persona.
    pub persona: Option<String>,
    /// A file's path, or a commit's subject.
    pub text: Option<String>,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    /// Commits pushed, or committed at once beyond the first.
    #[serde(default)]
    pub count: u32,
    pub pr: Option<u64>,
    pub checks: Option<ChecksState>,
    pub review: Option<ReviewState>,
}

/// Everything the map draws for one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    /// The normalised origin, such as `github.com/ragnarula/bosun`.
    pub url: String,
    /// The repository's page, for a project on GitHub.
    pub web_url: Option<String>,
    /// The main branch's name, such as `main`.
    pub main_branch: String,
    pub main: Vec<CommitInfo>,
    pub lanes: Vec<Lane>,
    pub merged: Vec<MergedLane>,
    /// Branches on `origin` that no lane has checked out.
    pub remote_only: u32,
    pub overlaps: Vec<Overlap>,
    pub pull_requests: PrSource,
    /// Newest first.
    pub feed: Vec<FeedEntry>,
}

/// One project in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub url: String,
    pub lanes: u32,
    pub overlaps: u32,
    /// The root sessions working in the project, so a session can link to it.
    pub sessions: Vec<String>,
}
