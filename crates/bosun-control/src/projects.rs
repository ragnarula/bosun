//! The project map. Every session's working copy is read through the
//! executor's `git_state`: every few seconds, and again right after a call
//! that can change files. Copies of one repository form a project. Its view
//! joins their branches with pull requests read from GitHub, the branches
//! merged recently, and an activity feed built from what changed between
//! reads.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use bosun_common::error::ErrorExt;
use bosun_common::project::ChecksState;
use bosun_common::project::CopyKind;
use bosun_common::project::FeedEntry;
use bosun_common::project::FeedKind;
use bosun_common::project::FileOp;
use bosun_common::project::GitState;
use bosun_common::project::Lane;
use bosun_common::project::MAX_FEED_ENTRIES;
use bosun_common::project::MergedLane;
use bosun_common::project::Overlap;
use bosun_common::project::PrSource;
use bosun_common::project::PrState;
use bosun_common::project::ProjectSummary;
use bosun_common::project::ProjectView;
use bosun_common::project::PullRequest;
use bosun_common::project::ReviewState;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::time::unix_secs;
use bosun_store::store::Store;
use futures_util::future::join_all;
use serde_json::Value;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::debug;
use tracing::warn;

use crate::skills_repos::GitHubClient;
use crate::tools::call_executor;
use crate::tunnel::TunnelRegistry;

/// How often every working copy is read. A change made outside Bosun shows
/// within this time.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How soon a copy is read after a call that can change it.
const QUICK_INTERVAL: Duration = Duration::from_millis(700);
/// How often pull requests are read from GitHub. Each branch costs up to
/// three requests, well inside GitHub's 5000 an hour for a token.
const PR_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// A node that does not answer a read in this time is skipped until the next.
const GIT_STATE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_OVERLAPS: usize = 20;
/// Commits named one by one in the feed when several land between reads.
const MAX_FEED_COMMITS: usize = 3;
/// Tools whose calls can change files in the working copy.
const CHANGING_TOOLS: &[&str] = &["file_write", "edit", "shell"];

/// A tool call starting or finishing, as the tunnel tool executor reports it.
#[derive(Debug, Clone)]
pub struct ToolActivity {
    pub session_id: String,
    pub tool: String,
    pub finished: bool,
}

/// The project map's state, shared by its readers and the API.
pub struct ProjectHub {
    state: Mutex<HubState>,
    /// The id of each project whose view changed.
    updates: broadcast::Sender<String>,
    merged_retention_secs: i64,
    /// Whether the control plane can read pull requests from GitHub.
    github_token: bool,
}

#[derive(Default)]
struct HubState {
    /// Keyed by `node:dir`, the session's directory.
    copies: BTreeMap<String, Copy>,
    projects: BTreeMap<String, Project>,
    /// Calls in flight per session.
    running: HashMap<String, u32>,
    /// Copies to read soon, with the session a change will be credited to.
    pending: BTreeMap<String, Option<String>>,
}

struct Copy {
    node: String,
    /// The root sessions working in the folder.
    roots: Vec<String>,
    /// Every session in the folder, with its persona.
    sessions: Vec<(String, Option<String>)>,
    cloned: bool,
    git: Option<GitState>,
}

impl Copy {
    fn lane_key(&self) -> Option<String> {
        self.git
            .as_ref()
            .map(|git| format!("{}:{}", self.node, git.root))
    }
}

struct Project {
    identity: Identity,
    feed: VecDeque<FeedEntry>,
    /// Each branch's newest pull request, and its head commit.
    prs: HashMap<String, (PullRequest, String)>,
    prs_read: bool,
    pr_source: PrSource,
    merged: Vec<MergedLane>,
    merged_read: bool,
    overlaps: Vec<Overlap>,
    /// The view last announced.
    sent: Option<ProjectView>,
}

/// A repository, named from its `origin`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    id: String,
    name: String,
    url: String,
    web_url: Option<String>,
    /// Owner and name, for a repository on GitHub.
    github: Option<(String, String)>,
}

/// A copy to read, and who a change found there is credited to.
#[derive(Debug, Clone)]
struct Target {
    copy: String,
    node: String,
    session: String,
    credit: Option<String>,
}

/// A merge the store has to record.
pub(crate) struct NewMerge {
    project: String,
    head: String,
    lane: MergedLane,
}

impl ProjectHub {
    pub fn new(merged_retention: Duration, github_token: bool) -> Self {
        Self {
            state: Mutex::new(HubState::default()),
            updates: broadcast::channel(64).0,
            merged_retention_secs: merged_retention.as_secs() as i64,
            github_token,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.updates.subscribe()
    }

    pub fn summaries(&self) -> Vec<ProjectSummary> {
        let state = self.state.lock().unwrap();
        state
            .projects
            .iter()
            .map(|(id, project)| {
                let copies = project_copies(&state.copies, id);
                let mut sessions: Vec<String> = copies
                    .iter()
                    .flat_map(|copy| copy.roots.iter().cloned())
                    .collect();
                sessions.sort();
                sessions.dedup();
                let lanes: BTreeSet<String> =
                    copies.iter().filter_map(|copy| copy.lane_key()).collect();
                ProjectSummary {
                    id: id.clone(),
                    name: project.identity.name.clone(),
                    url: project.identity.url.clone(),
                    lanes: lanes.len() as u32,
                    overlaps: project.overlaps.len() as u32,
                    sessions,
                }
            })
            .collect()
    }

    pub fn view(&self, id: &str) -> Option<ProjectView> {
        let state = self.state.lock().unwrap();
        let project = state.projects.get(id)?;
        let since = now_secs() - self.merged_retention_secs;
        Some(build_view(
            project,
            &project_copies(&state.copies, id),
            since,
        ))
    }

    /// Builds the project's view and announces it when it differs from the
    /// one announced last.
    pub(crate) fn publish(&self, id: &str) {
        let since = now_secs() - self.merged_retention_secs;
        let changed = {
            let mut state = self.state.lock().unwrap();
            let view = match state.projects.get(id) {
                Some(project) => build_view(project, &project_copies(&state.copies, id), since),
                None => return,
            };
            let project = state
                .projects
                .get_mut(id)
                .expect("the project was just read");
            if project.sent.as_ref() == Some(&view) {
                false
            } else {
                project.sent = Some(view);
                true
            }
        };
        if changed {
            let _ = self.updates.send(id.to_string());
        }
    }

    fn note_activity(&self, activity: ToolActivity) {
        let mut state = self.state.lock().unwrap();
        let count = state
            .running
            .entry(activity.session_id.clone())
            .or_insert(0);
        if !activity.finished {
            *count += 1;
            return;
        }
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.running.remove(&activity.session_id);
        }
        if !CHANGING_TOOLS.contains(&activity.tool.as_str()) {
            return;
        }
        let copy = state.copies.iter().find_map(|(key, copy)| {
            copy.sessions
                .iter()
                .any(|(id, _)| *id == activity.session_id)
                .then(|| key.clone())
        });
        if let Some(copy) = copy {
            state.pending.insert(copy, Some(activity.session_id));
        }
    }

    /// Replaces the copies with the folders the sessions use now. A session
    /// still being created has no folder to read yet, and a stopped one has
    /// none left. A copy whose sessions are all gone leaves the map, and a
    /// project with no copies left goes with it; its stream hears so.
    pub(crate) fn sync_sessions(&self, sessions: &[Session]) {
        let sessions: Vec<&Session> = sessions
            .iter()
            .filter(|session| {
                !matches!(
                    session.state,
                    SessionState::Creating | SessionState::Stopped
                )
            })
            .collect();
        let mut state = self.state.lock().unwrap();
        let mut seen = BTreeSet::new();
        for session in &sessions {
            let key = format!("{}:{}", session.node, session.dir);
            seen.insert(key.clone());
            let copy = state.copies.entry(key).or_insert_with(|| Copy {
                node: session.node.clone(),
                roots: Vec::new(),
                sessions: Vec::new(),
                cloned: false,
                git: None,
            });
            if !copy.sessions.iter().any(|(id, _)| *id == session.id) {
                copy.sessions
                    .push((session.id.clone(), session.persona.clone()));
            }
            if session.parent_id.is_none() && !copy.roots.contains(&session.id) {
                copy.roots.push(session.id.clone());
            }
            copy.cloned |= session.repo_url.is_some();
        }
        // A session that ended leaves its copy; one that remains keeps it.
        let live: BTreeSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
        state.copies.retain(|key, _| seen.contains(key));
        for copy in state.copies.values_mut() {
            copy.sessions.retain(|(id, _)| live.contains(id.as_str()));
            copy.roots.retain(|id| live.contains(id.as_str()));
        }
        let used: BTreeSet<String> = state
            .copies
            .values()
            .filter_map(|copy| copy.git.as_ref().map(identify).map(|identity| identity.id))
            .collect();
        let gone: Vec<String> = state
            .projects
            .keys()
            .filter(|id| !used.contains(*id))
            .cloned()
            .collect();
        state.projects.retain(|id, _| used.contains(id));
        state.pending.retain(|key, _| seen.contains(key));
        drop(state);
        for id in gone {
            let _ = self.updates.send(id);
        }
    }

    /// Every copy to read: the copies still to be read soon, or all of them.
    fn targets(&self, all: bool) -> Vec<Target> {
        let mut state = self.state.lock().unwrap();
        let pending = std::mem::take(&mut state.pending);
        let keys: Vec<(String, Option<String>)> = if all {
            state
                .copies
                .keys()
                .map(|key| (key.clone(), pending.get(key).cloned().flatten()))
                .collect()
        } else {
            pending.into_iter().collect()
        };
        keys.into_iter()
            .filter_map(|(key, credit)| {
                let copy = state.copies.get(&key)?;
                let session = copy
                    .roots
                    .first()
                    .or_else(|| copy.sessions.first().map(|(id, _)| id))?
                    .clone();
                // A change found while a session's call is running is that
                // session's; otherwise it was made outside Bosun.
                let credit = credit.or_else(|| {
                    copy.sessions
                        .iter()
                        .find(|(id, _)| state.running.contains_key(id))
                        .map(|(id, _)| id.clone())
                });
                Some(Target {
                    copy: key,
                    node: copy.node.clone(),
                    session,
                    credit,
                })
            })
            .collect()
    }

    /// Takes a fresh read of one copy: records what changed in the feed and
    /// returns the projects to announce and the merges to record.
    pub(crate) fn apply_read(
        &self,
        copy_key: &str,
        git: Option<GitState>,
        credit: Option<&str>,
        now: i64,
    ) -> (BTreeSet<String>, Vec<NewMerge>) {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let mut touched = BTreeSet::new();
        let mut merges = Vec::new();
        let Some(copy) = state.copies.get_mut(copy_key) else {
            return (touched, merges);
        };
        let old = copy.git.take();
        copy.git = git.clone();
        let persona = credit.and_then(|credit| {
            copy.sessions
                .iter()
                .find(|(id, _)| id == credit)
                .and_then(|(_, persona)| persona.clone())
        });
        let lane_key = copy.lane_key();
        if let Some(old) = &old {
            touched.insert(identify(old).id);
        }
        let Some(new) = git else {
            return (touched, merges);
        };
        let identity = identify(&new);
        touched.insert(identity.id.clone());
        let project = state
            .projects
            .entry(identity.id.clone())
            .or_insert_with(|| Project {
                identity: identity.clone(),
                feed: VecDeque::new(),
                prs: HashMap::new(),
                prs_read: false,
                pr_source: match (&identity.github, self.github_token) {
                    (None, _) => PrSource::NotGithub,
                    (Some(_), true) => PrSource::On,
                    (Some(_), false) => PrSource::NoToken,
                },
                merged: Vec::new(),
                merged_read: false,
                overlaps: Vec::new(),
                sent: None,
            });
        project.identity = identity.clone();
        if let Some(old) = old.as_ref().filter(|old| identify(old) == identity) {
            for mut entry in copy_changes(old, &new) {
                entry.at_secs = now;
                entry.lane = lane_key.clone();
                entry.session = credit.map(str::to_string);
                entry.persona = persona.clone();
                push_feed(&mut project.feed, entry);
            }
            if let Some(merge) = merged_into_main(old, &new, now) {
                let pr = old
                    .branch
                    .as_ref()
                    .and_then(|branch| project.prs.get(branch))
                    .map(|(pr, _)| pr.clone());
                let lane = MergedLane {
                    pr: pr.as_ref().map(|pr| pr.number),
                    title: pr.map(|pr| pr.title),
                    ..merge.1
                };
                merges.push(NewMerge {
                    project: identity.id.clone(),
                    head: merge.0,
                    lane,
                });
            }
        }
        // New overlaps go to the feed; ones that end leave quietly.
        let copies = project_copies(&state.copies, &identity.id);
        let overlaps = find_overlaps(&copies);
        let project = state
            .projects
            .get_mut(&identity.id)
            .expect("inserted above");
        for overlap in &overlaps {
            if !project.overlaps.contains(overlap) {
                push_feed(
                    &mut project.feed,
                    FeedEntry {
                        at_secs: now,
                        kind: FeedKind::Overlap,
                        lane: lane_key.clone(),
                        branch: new.branch.clone(),
                        session: None,
                        persona: None,
                        text: Some(overlap.path.clone()),
                        added: 0,
                        removed: 0,
                        count: 0,
                        pr: None,
                        checks: None,
                        review: None,
                    },
                );
            }
        }
        project.overlaps = overlaps;
        (touched, merges)
    }

    /// The GitHub projects and the branches to read their pull requests for.
    fn github_targets(&self) -> Vec<(String, String, String, Vec<String>)> {
        let state = self.state.lock().unwrap();
        state
            .projects
            .iter()
            .filter_map(|(id, project)| {
                let (owner, name) = project.identity.github.clone()?;
                let copies = project_copies(&state.copies, id);
                let main = main_branch_name(&copies);
                let branches: BTreeSet<String> = copies
                    .iter()
                    .filter_map(|copy| copy.git.as_ref()?.branch.clone())
                    .filter(|branch| *branch != main)
                    .collect();
                Some((id.clone(), owner, name, branches.into_iter().collect()))
            })
            .collect()
    }

    fn set_pr_source(&self, id: &str, source: PrSource) {
        if let Some(project) = self.state.lock().unwrap().projects.get_mut(id) {
            project.pr_source = source;
        }
    }

    /// Takes a fresh read of a project's pull requests: records what changed
    /// in the feed and returns the merges to record. The first read after a
    /// boot only learns the state, so it adds nothing to the feed.
    fn apply_pull_requests(
        &self,
        id: &str,
        prs: HashMap<String, (PullRequest, String)>,
        failed: bool,
        now: i64,
    ) -> Vec<NewMerge> {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let lanes: HashMap<String, (String, u32)> = project_copies(&state.copies, id)
            .iter()
            .filter_map(|copy| {
                let git = copy.git.as_ref()?;
                Some((git.branch.clone()?, (copy.lane_key()?, git.ahead)))
            })
            .collect();
        let since = now - self.merged_retention_secs;
        let Some(project) = state.projects.get_mut(id) else {
            return Vec::new();
        };
        project.pr_source = if failed {
            PrSource::Failing
        } else {
            PrSource::On
        };
        let mut merges = Vec::new();
        for (branch, (pr, head)) in &prs {
            let old = project.prs.get(branch).map(|(pr, _)| pr);
            let lane = lanes.get(branch);
            if project.prs_read {
                for kind in pr_changes(old, pr) {
                    push_feed(
                        &mut project.feed,
                        FeedEntry {
                            at_secs: now,
                            kind,
                            lane: lane.map(|(key, _)| key.clone()),
                            branch: Some(branch.clone()),
                            session: None,
                            persona: None,
                            text: Some(pr.title.clone()),
                            added: 0,
                            removed: 0,
                            count: 0,
                            pr: Some(pr.number),
                            checks: pr.checks,
                            review: pr.review,
                        },
                    );
                }
            }
            let newly_merged =
                pr.state == PrState::Merged && old.map(|old| old.state) != Some(PrState::Merged);
            let merged_at = pr.merged_at_secs.unwrap_or(now);
            if newly_merged && merged_at >= since {
                merges.push(NewMerge {
                    project: id.to_string(),
                    head: head.clone(),
                    lane: MergedLane {
                        branch: branch.clone(),
                        pr: Some(pr.number),
                        title: Some(pr.title.clone()),
                        commits: lane.map(|(_, ahead)| *ahead).unwrap_or(0),
                        merged_at_secs: merged_at,
                    },
                });
            }
        }
        // A failed read keeps what was known.
        if !failed {
            project.prs = prs;
        } else {
            project.prs.extend(prs);
        }
        project.prs_read = true;
        merges
    }

    fn add_merged(&self, id: &str, lane: MergedLane) {
        if let Some(project) = self.state.lock().unwrap().projects.get_mut(id)
            && !project
                .merged
                .iter()
                .any(|known| known.branch == lane.branch && known.pr == lane.pr)
        {
            project.merged.insert(0, lane);
        }
    }

    /// Projects whose recent merges have not been read from the store yet.
    fn unread_merges(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .projects
            .iter()
            .filter(|(_, project)| !project.merged_read)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn set_merged(&self, id: &str, merged: Vec<MergedLane>) {
        if let Some(project) = self.state.lock().unwrap().projects.get_mut(id) {
            project.merged = merged;
            project.merged_read = true;
        }
    }
}

fn now_secs() -> i64 {
    unix_secs(SystemTime::now())
}

fn push_feed(feed: &mut VecDeque<FeedEntry>, entry: FeedEntry) {
    feed.push_front(entry);
    feed.truncate(MAX_FEED_ENTRIES);
}

fn project_copies<'a>(copies: &'a BTreeMap<String, Copy>, id: &str) -> Vec<&'a Copy> {
    copies
        .values()
        .filter(|copy| copy.git.as_ref().is_some_and(|git| identify(git).id == id))
        .collect()
}

/// Reads every session's working copy every few seconds, and a copy soon
/// after one of its sessions' calls that can change it. Runs until the
/// activity channel closes.
pub async fn run(
    hub: Arc<ProjectHub>,
    store: Store,
    tunnels: Arc<TunnelRegistry>,
    mut activity: mpsc::UnboundedReceiver<ToolActivity>,
) {
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut quick = tokio::time::interval(QUICK_INTERVAL);
    quick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = poll.tick() => {
                match store.list_sessions().await {
                    Ok(sessions) => hub.sync_sessions(&sessions),
                    Err(error) => {
                        warn!(error = %error.display_chain(), "failed to list sessions for the project map");
                        continue;
                    }
                }
                let targets = hub.targets(true);
                refresh(&hub, &store, &tunnels, targets).await;
            }
            _ = quick.tick() => {
                let targets = hub.targets(false);
                if !targets.is_empty() {
                    refresh(&hub, &store, &tunnels, targets).await;
                }
            }
            message = activity.recv() => match message {
                Some(message) => hub.note_activity(message),
                None => return,
            },
        }
    }
}

async fn refresh(hub: &ProjectHub, store: &Store, tunnels: &TunnelRegistry, targets: Vec<Target>) {
    let reads = join_all(
        targets
            .into_iter()
            .filter(|target| tunnels.has_tunnel(&target.node))
            .map(|target| async move {
                let read = tokio::time::timeout(
                    GIT_STATE_TIMEOUT,
                    read_copy(tunnels, store, &target.session),
                )
                .await;
                (target, read)
            }),
    )
    .await;
    let now = now_secs();
    let mut touched = BTreeSet::new();
    let mut merges = Vec::new();
    for (target, read) in reads {
        match read {
            Ok(Ok(git)) => {
                let (projects, merged) =
                    hub.apply_read(&target.copy, git, target.credit.as_deref(), now);
                touched.extend(projects);
                merges.extend(merged);
            }
            Ok(Err(error)) => debug!(
                copy = %target.copy,
                error = %error.display_chain(),
                "failed to read the working copy's git state"
            ),
            Err(_) => debug!(copy = %target.copy, "the working copy's git state took too long"),
        }
    }
    record_merges(hub, store, merges).await;
    read_stored_merges(hub, store).await;
    for id in touched {
        hub.publish(&id);
    }
}

async fn read_copy(
    tunnels: &TunnelRegistry,
    store: &Store,
    session: &str,
) -> anyhow::Result<Option<GitState>> {
    let outcome = call_executor(tunnels, store, session, "git_state", &json!({})).await?;
    if outcome.is_error {
        anyhow::bail!("the executor refused git_state: {}", outcome.content);
    }
    serde_json::from_value(outcome.content).context("failed to parse the git state")
}

async fn record_merges(hub: &ProjectHub, store: &Store, merges: Vec<NewMerge>) {
    for merge in merges {
        if let Err(error) = store
            .record_merged_lane(&merge.project, &merge.head, &merge.lane)
            .await
        {
            warn!(
                project = %merge.project,
                branch = %merge.lane.branch,
                error = %error.display_chain(),
                "failed to record a merged branch"
            );
        }
        hub.add_merged(&merge.project, merge.lane);
    }
}

async fn read_stored_merges(hub: &ProjectHub, store: &Store) {
    let since = now_secs() - hub.merged_retention_secs;
    if let Err(error) = store.prune_merged_lanes(since).await {
        warn!(error = %error.display_chain(), "failed to prune merged branches");
    }
    for id in hub.unread_merges() {
        match store.merged_lanes(&id, since).await {
            Ok(merged) => hub.set_merged(&id, merged),
            Err(error) => warn!(
                project = %id,
                error = %error.display_chain(),
                "failed to read merged branches"
            ),
        }
    }
}

/// Reads each GitHub project's pull requests once a minute. A control plane
/// with no token reads nothing and says so in each project's view.
pub async fn run_pull_requests(hub: Arc<ProjectHub>, store: Store, github: GitHubClient) {
    let mut tick = tokio::time::interval(PR_POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first read waits for the copies' first reads, which name the
    // branches.
    tick.tick().await;
    loop {
        tick.tick().await;
        for (id, owner, name, branches) in hub.github_targets() {
            if !github.has_token() {
                hub.set_pr_source(&id, PrSource::NoToken);
                hub.publish(&id);
                continue;
            }
            let mut prs = HashMap::new();
            let mut failed = false;
            for branch in branches {
                match read_pull_request(&github, &owner, &name, &branch).await {
                    Ok(Some(pr)) => {
                        prs.insert(branch, pr);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(
                            project = %id,
                            branch = %branch,
                            error = %error.display_chain(),
                            "failed to read the branch's pull request"
                        );
                        failed = true;
                    }
                }
            }
            let merges = hub.apply_pull_requests(&id, prs, failed, now_secs());
            record_merges(&hub, &store, merges).await;
            hub.publish(&id);
        }
    }
}

/// The branch's newest pull request and its head commit, with the head's
/// checks and the reviews for one that is open.
async fn read_pull_request(
    github: &GitHubClient,
    owner: &str,
    name: &str,
    branch: &str,
) -> anyhow::Result<Option<(PullRequest, String)>> {
    let head = format!("{owner}:{branch}");
    let list = github
        .api_json(
            &format!("/repos/{owner}/{name}/pulls"),
            &[("head", &head), ("state", "all"), ("per_page", "1")],
        )
        .await?;
    let Some((mut pr, sha)) = list
        .as_array()
        .and_then(|list| list.first())
        .and_then(parse_pull)
    else {
        return Ok(None);
    };
    if matches!(pr.state, PrState::Open | PrState::Draft) {
        let runs = github
            .api_json(
                &format!("/repos/{owner}/{name}/commits/{sha}/check-runs"),
                &[("per_page", "100")],
            )
            .await?;
        pr.checks = summarise_checks(
            runs["check_runs"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        let reviews = github
            .api_json(
                &format!("/repos/{owner}/{name}/pulls/{}/reviews", pr.number),
                &[("per_page", "100")],
            )
            .await?;
        pr.review = summarise_reviews(reviews.as_array().map(Vec::as_slice).unwrap_or(&[]));
    }
    Ok(Some((pr, sha)))
}

/// One pull request from GitHub's list, and its head commit.
fn parse_pull(value: &Value) -> Option<(PullRequest, String)> {
    let merged_at_secs = value["merged_at"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp());
    let state = match (value["state"].as_str()?, value["draft"].as_bool()) {
        _ if merged_at_secs.is_some() => PrState::Merged,
        ("closed", _) => PrState::Closed,
        (_, Some(true)) => PrState::Draft,
        _ => PrState::Open,
    };
    Some((
        PullRequest {
            number: value["number"].as_u64()?,
            title: value["title"].as_str().unwrap_or_default().to_string(),
            url: value["html_url"].as_str().unwrap_or_default().to_string(),
            state,
            checks: None,
            review: None,
            merged_at_secs,
        },
        value["head"]["sha"].as_str()?.to_string(),
    ))
}

/// A failed run wins over one still running, so a known failure shows at
/// once.
fn summarise_checks(runs: &[Value]) -> Option<ChecksState> {
    if runs.is_empty() {
        return None;
    }
    let failed = runs.iter().any(|run| {
        matches!(
            run["conclusion"].as_str(),
            Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure")
        )
    });
    if failed {
        return Some(ChecksState::Failed);
    }
    if runs
        .iter()
        .any(|run| run["status"].as_str() != Some("completed"))
    {
        return Some(ChecksState::Running);
    }
    Some(ChecksState::Passed)
}

/// Each reviewer's latest approval or request for changes counts; a request
/// for changes from anyone wins.
fn summarise_reviews(reviews: &[Value]) -> Option<ReviewState> {
    let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
    for review in reviews {
        let (Some(user), Some(state)) =
            (review["user"]["login"].as_str(), review["state"].as_str())
        else {
            continue;
        };
        match state {
            "APPROVED" | "CHANGES_REQUESTED" => {
                latest.insert(user, state);
            }
            "DISMISSED" => {
                latest.remove(user);
            }
            _ => {}
        }
    }
    if latest.values().any(|state| *state == "CHANGES_REQUESTED") {
        Some(ReviewState::ChangesRequested)
    } else if latest.values().any(|state| *state == "APPROVED") {
        Some(ReviewState::Approved)
    } else {
        None
    }
}

/// What changed in a pull request between two reads.
fn pr_changes(old: Option<&PullRequest>, new: &PullRequest) -> Vec<FeedKind> {
    let Some(old) = old else {
        return match new.state {
            PrState::Open | PrState::Draft => vec![FeedKind::PrOpened],
            PrState::Merged => vec![FeedKind::Merged],
            PrState::Closed => Vec::new(),
        };
    };
    let mut kinds = Vec::new();
    if new.checks != old.checks
        && matches!(new.checks, Some(ChecksState::Passed | ChecksState::Failed))
    {
        kinds.push(FeedKind::Checks);
    }
    if new.review != old.review && new.review.is_some() {
        kinds.push(FeedKind::Review);
    }
    if new.state == PrState::Merged && old.state != PrState::Merged {
        kinds.push(FeedKind::Merged);
    }
    kinds
}

/// The project a working copy belongs to, named from its `origin`. A copy
/// with no `origin` is a project of its own repository on its machine.
fn identify(git: &GitState) -> Identity {
    let url = match &git.origin {
        Some(origin) => normalise_origin(origin),
        None => {
            let common = Path::new(&git.common_dir);
            let repo = if common.file_name().is_some_and(|name| name == ".git") {
                common.parent().unwrap_or(common)
            } else {
                common
            };
            repo.to_string_lossy().into_owned()
        }
    };
    let name = url
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(&url)
        .to_string();
    let github = url.strip_prefix("github.com/").and_then(|path| {
        let (owner, repo) = path.split_once('/')?;
        (!repo.contains('/')).then(|| (owner.to_string(), repo.to_string()))
    });
    Identity {
        id: format!("{:016x}", fnv1a(&url)),
        web_url: github.as_ref().map(|_| format!("https://{url}")),
        name,
        url,
        github,
    }
}

/// An `origin` URL as `host/path`, so the SSH and HTTPS forms of one
/// repository match: `git@github.com:o/r.git`, `ssh://git@github.com/o/r`
/// and `https://github.com/o/r.git` all give `github.com/o/r`. A local path
/// stays a path.
fn normalise_origin(origin: &str) -> String {
    let origin = origin.trim().trim_end_matches('/');
    let origin = origin.strip_suffix(".git").unwrap_or(origin);
    if let Some((_, rest)) = origin.split_once("://") {
        let rest = rest.split_once('@').map_or(rest, |(_, host)| host);
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.split(':').next().unwrap_or(host);
        if host.is_empty() {
            return format!("/{path}");
        }
        return format!("{}/{path}", host.to_lowercase());
    }
    // scp-like `user@host:path`; a path with a colon before any slash.
    if let Some((before, path)) = origin.split_once(':')
        && !before.contains('/')
    {
        let host = before.split_once('@').map_or(before, |(_, host)| host);
        return format!("{}/{}", host.to_lowercase(), path.trim_start_matches('/'));
    }
    origin.to_string()
}

fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// What changed in one working copy between two reads.
fn copy_changes(old: &GitState, new: &GitState) -> Vec<FeedEntry> {
    let entry = |kind: FeedKind| FeedEntry {
        at_secs: 0,
        kind,
        lane: None,
        branch: new.branch.clone(),
        session: None,
        persona: None,
        text: None,
        added: 0,
        removed: 0,
        count: 0,
        pr: None,
        checks: None,
        review: None,
    };
    let mut entries = Vec::new();
    if old.branch != new.branch {
        entries.push(entry(FeedKind::Switched));
    }
    let known: HashMap<&str, &bosun_common::project::DirtyFile> = old
        .dirty
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    if old.head == new.head {
        for file in &new.dirty {
            let (added, removed) = match known.get(file.path.as_str()) {
                Some(before)
                    if before.op == file.op
                        && before.added == file.added
                        && before.removed == file.removed =>
                {
                    continue;
                }
                Some(before) => (
                    file.added.saturating_sub(before.added),
                    file.removed.saturating_sub(before.removed),
                ),
                None => (file.added, file.removed),
            };
            entries.push(FeedEntry {
                text: Some(file.path.clone()),
                added,
                removed,
                ..entry(match file.op {
                    FileOp::Created => FeedKind::Created,
                    FileOp::Edited => FeedKind::Edited,
                    FileOp::Deleted => FeedKind::Deleted,
                })
            });
        }
    } else if new.branch == old.branch {
        let fresh: Vec<_> = new
            .commits
            .iter()
            .take_while(|commit| Some(&commit.sha) != old.head.as_ref())
            .collect();
        for commit in fresh.iter().take(MAX_FEED_COMMITS) {
            entries.push(FeedEntry {
                text: Some(commit.subject.clone()),
                ..entry(FeedKind::Committed)
            });
        }
        if fresh.len() > MAX_FEED_COMMITS {
            entries.push(FeedEntry {
                count: (fresh.len() - MAX_FEED_COMMITS) as u32,
                ..entry(FeedKind::Committed)
            });
        }
    }
    let was_local: BTreeSet<&str> = old
        .commits
        .iter()
        .filter(|commit| !commit.pushed)
        .map(|commit| commit.sha.as_str())
        .collect();
    let pushed = new
        .commits
        .iter()
        .filter(|commit| commit.pushed && was_local.contains(commit.sha.as_str()))
        .count();
    if pushed > 0 {
        entries.push(FeedEntry {
            count: pushed as u32,
            ..entry(FeedKind::Pushed)
        });
    }
    entries
}

/// A branch that had its own commits and whose head is now on the main
/// branch: its head, and the merged branch. Merges a pull request reports
/// are found by the pull request reader instead.
fn merged_into_main(old: &GitState, new: &GitState, now: i64) -> Option<(String, MergedLane)> {
    let head = old.head.as_ref()?;
    let branch = old.branch.clone()?;
    if old.ahead == 0 || !new.main.iter().any(|commit| commit.sha == *head) {
        return None;
    }
    Some((
        head.clone(),
        MergedLane {
            branch,
            pr: None,
            title: None,
            commits: old.ahead,
            merged_at_secs: now,
        },
    ))
}

/// The main branch's name, from the copy whose main branch is newest.
fn main_branch_name(copies: &[&Copy]) -> String {
    newest_main(copies)
        .and_then(|git| git.main_ref.clone())
        .map(|main| {
            main.strip_prefix("origin/")
                .map(str::to_string)
                .unwrap_or(main)
        })
        .unwrap_or_else(|| "main".to_string())
}

fn newest_main<'a>(copies: &[&'a Copy]) -> Option<&'a GitState> {
    copies
        .iter()
        .filter_map(|copy| copy.git.as_ref())
        .max_by_key(|git| {
            git.main
                .first()
                .map(|commit| commit.time_secs)
                .unwrap_or(i64::MIN)
        })
}

/// Pairs of lanes that change the same file, by path.
fn find_overlaps(copies: &[&Copy]) -> Vec<Overlap> {
    let mut lanes: Vec<(String, &GitState)> = copies
        .iter()
        .filter_map(|copy| Some((copy.lane_key()?, copy.git.as_ref()?)))
        .collect();
    lanes.sort_by(|a, b| a.0.cmp(&b.0));
    lanes.dedup_by(|a, b| a.0 == b.0);
    let mut overlaps = Vec::new();
    for (i, (a_key, a)) in lanes.iter().enumerate() {
        let a_paths: BTreeSet<&str> = a.changed.iter().map(String::as_str).collect();
        for (b_key, b) in &lanes[i + 1..] {
            for path in b
                .changed
                .iter()
                .filter(|path| a_paths.contains(path.as_str()))
            {
                overlaps.push(Overlap {
                    path: path.clone(),
                    lanes: [a_key.clone(), b_key.clone()],
                });
            }
        }
    }
    overlaps.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.lanes.cmp(&b.lanes)));
    overlaps.truncate(MAX_OVERLAPS);
    overlaps
}

fn build_view(project: &Project, copies: &[&Copy], merged_since: i64) -> ProjectView {
    let main_git = newest_main(copies);
    let main = main_git.map(|git| git.main.clone()).unwrap_or_default();
    let main_branch = main_branch_name(copies);
    let mut lanes: BTreeMap<String, Lane> = BTreeMap::new();
    for copy in copies {
        let (Some(git), Some(key)) = (copy.git.as_ref(), copy.lane_key()) else {
            continue;
        };
        if let Some(lane) = lanes.get_mut(&key) {
            for root in &copy.roots {
                if !lane.sessions.contains(root) {
                    lane.sessions.push(root.clone());
                }
            }
            continue;
        }
        let common = Path::new(&git.common_dir);
        let linked = common != Path::new(&git.root).join(".git");
        let (kind, worktree_of) = if linked {
            let repo = if common.file_name().is_some_and(|name| name == ".git") {
                common
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
            } else {
                None
            };
            (CopyKind::Worktree, repo)
        } else if copy.cloned {
            (CopyKind::Clone, None)
        } else {
            (CopyKind::Folder, None)
        };
        let fork_index = git
            .fork
            .as_ref()
            .and_then(|fork| main.iter().position(|commit| commit.sha == *fork))
            .map(|index| index as u32);
        let pr = git
            .branch
            .as_ref()
            .and_then(|branch| project.prs.get(branch))
            .map(|(pr, _)| pr.clone());
        lanes.insert(
            key.clone(),
            Lane {
                key,
                node: copy.node.clone(),
                root: git.root.clone(),
                kind,
                worktree_of,
                branch: git.branch.clone(),
                head: git.head.clone(),
                fork_index,
                ahead: git.ahead,
                behind: git.behind,
                commits: git.commits.clone(),
                dirty: git.dirty.clone(),
                sessions: copy.roots.clone(),
                pr,
            },
        );
    }
    let merged: Vec<MergedLane> = project
        .merged
        .iter()
        .filter(|lane| lane.merged_at_secs >= merged_since)
        .cloned()
        .collect();
    let mut taken: BTreeSet<&str> = lanes
        .values()
        .filter_map(|lane| lane.branch.as_deref())
        .collect();
    taken.insert(main_branch.as_str());
    for lane in &merged {
        taken.insert(lane.branch.as_str());
    }
    let remote: BTreeSet<&str> = copies
        .iter()
        .filter_map(|copy| copy.git.as_ref())
        .flat_map(|git| git.remote_branches.iter().map(String::as_str))
        .collect();
    let remote_only = remote
        .iter()
        .filter(|branch| !taken.contains(*branch))
        .count() as u32;
    ProjectView {
        id: project.identity.id.clone(),
        name: project.identity.name.clone(),
        url: project.identity.url.clone(),
        web_url: project.identity.web_url.clone(),
        main_branch,
        main,
        lanes: lanes.into_values().collect(),
        merged,
        remote_only,
        overlaps: project.overlaps.clone(),
        pull_requests: project.pr_source,
        feed: project.feed.iter().cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use bosun_common::project::CommitInfo;
    use bosun_common::project::DirtyFile;
    use bosun_common::session::Permission;

    use super::*;

    fn commit(sha: &str, pushed: bool) -> CommitInfo {
        CommitInfo {
            sha: sha.into(),
            subject: format!("Commit {sha}"),
            author: "Ann".into(),
            time_secs: 100,
            pushed,
        }
    }

    fn dirty(path: &str, op: FileOp, added: u32, removed: u32) -> DirtyFile {
        DirtyFile {
            path: path.into(),
            op,
            added,
            removed,
        }
    }

    fn git(root: &str, branch: &str) -> GitState {
        GitState {
            root: root.into(),
            common_dir: format!("{root}/.git"),
            origin: Some("git@github.com:ragnarula/bosun.git".into()),
            main_ref: Some("origin/main".into()),
            main: vec![
                commit("m3", false),
                commit("m2", false),
                commit("m1", false),
            ],
            branch: Some(branch.into()),
            head: Some("h1".into()),
            fork: Some("m2".into()),
            ahead: 1,
            behind: 1,
            commits: vec![commit("h1", false)],
            changed: Vec::new(),
            dirty: Vec::new(),
            remote_branches: vec!["main".into(), "other".into()],
        }
    }

    fn session(id: &str, node: &str, dir: &str, parent: Option<&str>, persona: &str) -> Session {
        Session {
            id: id.into(),
            node: node.into(),
            repo_url: None,
            git_ref: None,
            dir: dir.into(),
            model: "m".into(),
            persona: Some(persona.into()),
            parent_id: parent.map(str::to_string),
            owner_id: parent.unwrap_or(id).into(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            mcp_servers: String::new(),
            state: SessionState::Running,
            interrupt_cause: None,
            created_at_secs: 0,
            prompt: None,
            summary: None,
        }
    }

    #[test]
    fn origins_of_one_repository_normalise_alike() {
        for origin in [
            "git@github.com:ragnarula/bosun.git",
            "https://github.com/ragnarula/bosun.git",
            "https://github.com/ragnarula/bosun",
            "ssh://git@github.com/ragnarula/bosun.git",
            "ssh://git@GitHub.com:22/ragnarula/bosun",
            "https://token@github.com/ragnarula/bosun/",
        ] {
            assert_eq!(
                normalise_origin(origin),
                "github.com/ragnarula/bosun",
                "{origin}"
            );
        }
        assert_eq!(normalise_origin("/srv/git/bosun.git"), "/srv/git/bosun");
        assert_eq!(
            normalise_origin("file:///srv/git/bosun.git"),
            "/srv/git/bosun"
        );
    }

    #[test]
    fn a_github_project_is_named_for_its_repository() {
        let identity = identify(&git("/w/a", "main"));
        assert_eq!(identity.name, "bosun");
        assert_eq!(identity.url, "github.com/ragnarula/bosun");
        assert_eq!(
            identity.web_url.as_deref(),
            Some("https://github.com/ragnarula/bosun")
        );
        assert_eq!(identity.github, Some(("ragnarula".into(), "bosun".into())));

        let local = identify(&GitState {
            origin: None,
            ..git("/w/plain", "main")
        });
        assert_eq!(local.name, "plain");
        assert_eq!(local.github, None);
        assert_ne!(local.id, identity.id);
    }

    #[test]
    fn edits_report_only_the_lines_added_since_the_last_read() {
        let old = GitState {
            dirty: vec![
                dirty("a.rs", FileOp::Edited, 3, 1),
                dirty("same.rs", FileOp::Edited, 1, 0),
            ],
            ..git("/w/a", "feature")
        };
        let new = GitState {
            dirty: vec![
                dirty("a.rs", FileOp::Edited, 5, 1),
                dirty("same.rs", FileOp::Edited, 1, 0),
                dirty("new.rs", FileOp::Created, 9, 0),
            ],
            ..git("/w/a", "feature")
        };
        let entries = copy_changes(&old, &new);
        let got: Vec<_> = entries
            .iter()
            .map(|e| (e.kind, e.text.as_deref().unwrap(), e.added, e.removed))
            .collect();
        assert_eq!(
            got,
            [
                (FeedKind::Edited, "a.rs", 2, 0),
                (FeedKind::Created, "new.rs", 9, 0)
            ]
        );
    }

    #[test]
    fn a_commit_names_its_subjects_and_a_push_counts_its_commits() {
        let old = GitState {
            dirty: vec![dirty("a.rs", FileOp::Edited, 3, 1)],
            ..git("/w/a", "feature")
        };
        let committed = GitState {
            head: Some("h5".into()),
            commits: ["h5", "h4", "h3", "h2", "h1"]
                .iter()
                .map(|sha| commit(sha, false))
                .collect(),
            ..git("/w/a", "feature")
        };
        let entries = copy_changes(&old, &committed);
        let got: Vec<_> = entries
            .iter()
            .map(|e| (e.kind, e.text.clone(), e.count))
            .collect();
        assert_eq!(
            got,
            [
                (FeedKind::Committed, Some("Commit h5".to_string()), 0),
                (FeedKind::Committed, Some("Commit h4".to_string()), 0),
                (FeedKind::Committed, Some("Commit h3".to_string()), 0),
                (FeedKind::Committed, None, 1),
            ],
            "a commit does not report the files it took out of the dirty list"
        );

        let pushed = GitState {
            commits: committed
                .commits
                .iter()
                .map(|c| CommitInfo {
                    pushed: true,
                    ..c.clone()
                })
                .collect(),
            ..committed.clone()
        };
        let entries = copy_changes(&committed, &pushed);
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].kind, entries[0].count), (FeedKind::Pushed, 5));
    }

    #[test]
    fn a_branch_switch_is_one_entry_and_names_no_commits() {
        let old = git("/w/a", "feature");
        let new = GitState {
            head: Some("x9".into()),
            commits: vec![commit("x9", false)],
            ..git("/w/a", "other")
        };
        let kinds: Vec<_> = copy_changes(&old, &new).iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [FeedKind::Switched]);
    }

    #[test]
    fn a_branch_whose_head_reaches_main_has_merged() {
        let old = git("/w/a", "feature");
        let new = GitState {
            main: vec![
                commit("merge", false),
                commit("h1", false),
                commit("m3", false),
            ],
            ahead: 0,
            ..git("/w/a", "feature")
        };
        let (head, lane) = merged_into_main(&old, &new, 500).unwrap();
        assert_eq!(head, "h1");
        assert_eq!(
            (lane.branch.as_str(), lane.commits, lane.merged_at_secs),
            ("feature", 1, 500)
        );

        assert!(merged_into_main(&old, &git("/w/a", "feature"), 500).is_none());
        let fresh = GitState {
            ahead: 0,
            ..git("/w/a", "feature")
        };
        assert!(
            merged_into_main(&fresh, &new, 500).is_none(),
            "a branch with no commits of its own has nothing to merge"
        );
    }

    #[test]
    fn checks_fail_before_they_run_and_pass_only_when_all_pass() {
        let run =
            |status: &str, conclusion: Value| json!({ "status": status, "conclusion": conclusion });
        assert_eq!(summarise_checks(&[]), None);
        assert_eq!(
            summarise_checks(&[
                run("completed", json!("success")),
                run("in_progress", Value::Null)
            ]),
            Some(ChecksState::Running)
        );
        assert_eq!(
            summarise_checks(&[
                run("completed", json!("failure")),
                run("queued", Value::Null)
            ]),
            Some(ChecksState::Failed)
        );
        assert_eq!(
            summarise_checks(&[
                run("completed", json!("success")),
                run("completed", json!("skipped"))
            ]),
            Some(ChecksState::Passed)
        );
    }

    #[test]
    fn reviews_count_each_reviewers_latest_verdict() {
        let review = |user: &str, state: &str| json!({ "user": { "login": user }, "state": state });
        assert_eq!(summarise_reviews(&[review("a", "COMMENTED")]), None);
        assert_eq!(
            summarise_reviews(&[review("a", "CHANGES_REQUESTED"), review("a", "APPROVED")]),
            Some(ReviewState::Approved)
        );
        assert_eq!(
            summarise_reviews(&[review("a", "APPROVED"), review("b", "CHANGES_REQUESTED")]),
            Some(ReviewState::ChangesRequested)
        );
        assert_eq!(
            summarise_reviews(&[review("b", "CHANGES_REQUESTED"), review("b", "DISMISSED")]),
            None
        );
    }

    #[test]
    fn a_pull_request_reads_draft_closed_and_merged() {
        let pull = |state: &str, draft: bool, merged_at: Value| {
            json!({
                "number": 60, "title": "Back off", "html_url": "https://github.com/o/r/pull/60",
                "state": state, "draft": draft, "merged_at": merged_at, "head": { "sha": "abc" }
            })
        };
        let (pr, head) = parse_pull(&pull("open", true, Value::Null)).unwrap();
        assert_eq!(
            (pr.number, pr.state, head.as_str()),
            (60, PrState::Draft, "abc")
        );
        assert_eq!(
            parse_pull(&pull("open", false, Value::Null))
                .unwrap()
                .0
                .state,
            PrState::Open
        );
        assert_eq!(
            parse_pull(&pull("closed", false, Value::Null))
                .unwrap()
                .0
                .state,
            PrState::Closed
        );
        let (merged, _) =
            parse_pull(&pull("closed", false, json!("2026-10-08T15:30:00Z"))).unwrap();
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.merged_at_secs, Some(1_791_473_400));
    }

    #[test]
    fn pull_request_changes_name_checks_reviews_and_merges() {
        let pr = |state, checks, review| PullRequest {
            number: 60,
            title: "t".into(),
            url: String::new(),
            state,
            checks,
            review,
            merged_at_secs: None,
        };
        let running = pr(PrState::Open, Some(ChecksState::Running), None);
        assert_eq!(pr_changes(None, &running), [FeedKind::PrOpened]);
        assert_eq!(pr_changes(Some(&running), &running), []);
        let passed = pr(
            PrState::Open,
            Some(ChecksState::Passed),
            Some(ReviewState::Approved),
        );
        assert_eq!(
            pr_changes(Some(&running), &passed),
            [FeedKind::Checks, FeedKind::Review]
        );
        let merged = pr(
            PrState::Merged,
            Some(ChecksState::Passed),
            Some(ReviewState::Approved),
        );
        assert_eq!(pr_changes(Some(&passed), &merged), [FeedKind::Merged]);
    }

    /// Two sessions in two worktrees of one repository, and a child sharing
    /// the second root's folder: one project, two lanes, the child credited by
    /// persona.
    #[test]
    fn copies_of_one_repository_form_one_project_with_a_lane_each() {
        let hub = ProjectHub::new(Duration::from_secs(86_400), true);
        hub.sync_sessions(&[
            session("s1", "local", "/w/bosun", None, "lead"),
            session("s2", "local", "/w/bosun-map", None, "lead"),
            session("c1", "local", "/w/bosun-map", Some("s2"), "builder"),
        ]);
        let first = GitState {
            changed: vec!["api.rs".into()],
            ..git("/w/bosun", "crew-view")
        };
        let map = GitState {
            common_dir: "/w/bosun/.git".into(),
            changed: vec!["watch.rs".into()],
            ..git("/w/bosun-map", "project-map")
        };
        hub.apply_read("local:/w/bosun", Some(first.clone()), None, 10);
        let (touched, _) = hub.apply_read("local:/w/bosun-map", Some(map.clone()), None, 10);
        let id = touched.into_iter().next().unwrap();

        // The builder edits api.rs on the second lane too.
        hub.note_activity(ToolActivity {
            session_id: "c1".into(),
            tool: "edit".into(),
            finished: false,
        });
        let targets = hub.targets(true);
        let credited: Vec<_> = targets
            .iter()
            .map(|t| (t.copy.as_str(), t.credit.as_deref()))
            .collect();
        assert_eq!(
            credited,
            [("local:/w/bosun", None), ("local:/w/bosun-map", Some("c1"))]
        );
        let edited = GitState {
            dirty: vec![dirty("api.rs", FileOp::Edited, 4, 0)],
            changed: vec!["watch.rs".into(), "api.rs".into()],
            ..map.clone()
        };
        hub.apply_read("local:/w/bosun-map", Some(edited), Some("c1"), 20);

        let view = hub.view(&id).unwrap();
        assert_eq!(view.name, "bosun");
        assert_eq!(view.main_branch, "main");
        let lanes: Vec<_> = view
            .lanes
            .iter()
            .map(|lane| {
                (
                    lane.branch.as_deref().unwrap(),
                    lane.kind,
                    lane.worktree_of.as_deref(),
                    lane.fork_index,
                    lane.sessions.clone(),
                )
            })
            .collect();
        assert_eq!(
            lanes,
            [
                (
                    "crew-view",
                    CopyKind::Folder,
                    None,
                    Some(1),
                    vec!["s1".to_string()]
                ),
                (
                    "project-map",
                    CopyKind::Worktree,
                    Some("/w/bosun"),
                    Some(1),
                    vec!["s2".to_string()]
                ),
            ]
        );
        assert_eq!(view.overlaps.len(), 1);
        assert_eq!(view.overlaps[0].path, "api.rs");
        assert_eq!(view.remote_only, 1, "`other` is on GitHub only");
        let feed: Vec<_> = view
            .feed
            .iter()
            .map(|e| {
                (
                    e.kind,
                    e.text.as_deref(),
                    e.session.as_deref(),
                    e.persona.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            feed,
            [
                (FeedKind::Overlap, Some("api.rs"), None, None),
                (
                    FeedKind::Edited,
                    Some("api.rs"),
                    Some("c1"),
                    Some("builder")
                ),
            ]
        );
        let summaries = hub.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].sessions, ["s1", "s2"]);

        // The second tree stops: its lane goes.
        let stopped = |id, parent| Session {
            state: SessionState::Stopped,
            ..session(id, "local", "/w/bosun-map", parent, "lead")
        };
        hub.sync_sessions(&[
            session("s1", "local", "/w/bosun", None, "lead"),
            stopped("s2", None),
            stopped("c1", Some("s2")),
        ]);
        let view = hub.view(&id).unwrap();
        assert_eq!(view.lanes.len(), 1);
        hub.sync_sessions(&[]);
        assert!(hub.view(&id).is_none());
    }

    #[test]
    fn a_merged_pull_request_is_recorded_once_and_reported_after_the_first_read() {
        let hub = ProjectHub::new(Duration::from_secs(86_400), true);
        hub.sync_sessions(&[session("s1", "local", "/w/a", None, "lead")]);
        let (touched, _) = hub.apply_read("local:/w/a", Some(git("/w/a", "fix")), None, 10);
        let id = touched.into_iter().next().unwrap();
        assert_eq!(
            hub.github_targets(),
            [(
                id.clone(),
                "ragnarula".into(),
                "bosun".into(),
                vec!["fix".into()]
            )]
        );

        let pr = |state| PullRequest {
            number: 60,
            title: "Back off".into(),
            url: String::new(),
            state,
            checks: None,
            review: None,
            merged_at_secs: (state == PrState::Merged).then_some(now_secs()),
        };
        let read = |state| HashMap::from([("fix".to_string(), (pr(state), "h1".to_string()))]);
        assert!(
            hub.apply_pull_requests(&id, read(PrState::Open), false, now_secs())
                .is_empty()
        );
        assert!(
            hub.view(&id).unwrap().feed.is_empty(),
            "the first read only learns the state"
        );
        let merges = hub.apply_pull_requests(&id, read(PrState::Merged), false, now_secs());
        assert_eq!(merges.len(), 1);
        assert_eq!(
            (merges[0].head.as_str(), merges[0].lane.pr),
            ("h1", Some(60))
        );
        assert!(
            hub.apply_pull_requests(&id, read(PrState::Merged), false, now_secs())
                .is_empty()
        );
        let view = hub.view(&id).unwrap();
        assert_eq!(view.feed[0].kind, FeedKind::Merged);
        assert_eq!(view.pull_requests, PrSource::On);
        assert_eq!(
            view.lanes[0].pr.as_ref().map(|pr| pr.state),
            Some(PrState::Merged)
        );
    }
}
