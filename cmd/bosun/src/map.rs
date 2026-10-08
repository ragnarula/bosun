//! `bosun map`: one project's branches as lanes off its main branch, kept up
//! to date from the project's stream. Each lane names its crew, where it is
//! checked out, how far it is from main, what is not committed and its pull
//! request; the files two lanes both change and the newest activity follow.
//! Where lists the same lanes by machine. Enter opens the lane's session.

use std::io;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use bosun_agent::sse::sse_stream;
use bosun_common::project::ChecksState;
use bosun_common::project::FeedEntry;
use bosun_common::project::FeedKind;
use bosun_common::project::Lane;
use bosun_common::project::PrSource;
use bosun_common::project::PrState;
use bosun_common::project::ProjectSummary;
use bosun_common::project::ProjectView;
use bosun_common::project::PullRequest;
use bosun_common::project::ReviewState;
use bosun_common::session::SessionView;
use crossterm::event;
use crossterm::event::Event as TermEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::execute;
use crossterm::terminal;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc;

use crate::crew;

/// The width a branch name is padded to.
const NAME: usize = 24;
/// The width the lanes' graph is padded to: wide enough for a lane that
/// leaves main's newest commit with all its marks and the `◌`.
fn graph_width(main_len: usize) -> usize {
    main_len * 2 + LANE_MARKS * 2 + 3
}
/// The commits a lane draws before it folds the older ones into `┄`.
const LANE_MARKS: usize = 10;
/// The newest feed entries shown under the lanes.
const FEED_LINES: usize = 6;
/// How often the crews' sessions are read again for their tags.
const SESSIONS_INTERVAL: Duration = Duration::from_secs(3);
/// How long the map waits before it opens a lost stream again.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// The hull colours of Bosun Signal. A branch keeps the one its name picks,
/// as it does in the pane.
const HULLS: [Color; 6] = [
    Color::Rgb(0x5b, 0x8d, 0xef),
    Color::Rgb(0x2b, 0xb3, 0xa3),
    Color::Rgb(0xe0, 0x86, 0x4a),
    Color::Rgb(0xb0, 0x7c, 0xe8),
    Color::Rgb(0xd4, 0xb1, 0x45),
    Color::Rgb(0xe0, 0x6b, 0x8f),
];

/// The two screens of the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Branches,
    Where,
}

fn hull(branch: &str) -> Color {
    HULLS[(crew::fnv1a(branch.as_bytes()) % HULLS.len() as u32) as usize]
}

fn dim() -> Style {
    Style::default().fg(crew::MUTED)
}

fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        let kept: String = text.chars().take(width.saturating_sub(2)).collect();
        format!("{kept}… ")
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

fn plural(count: u32, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn branch_name(lane: &Lane) -> String {
    lane.branch.clone().unwrap_or_else(|| {
        let head = lane.head.as_deref().unwrap_or_default();
        format!("detached at {}", &head[..head.len().min(7)])
    })
}

fn clock(secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|at| at.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// The fact a pull request's badge leads with, as the pane's badge does.
fn pr_label(pr: &PullRequest) -> (String, Color) {
    let n = format!("#{}", pr.number);
    match (pr.state, pr.review, pr.checks) {
        (PrState::Merged, _, _) => (format!("{n} merged"), crew::ACCENT),
        (PrState::Closed, _, _) => (format!("{n} closed"), crew::MUTED),
        (PrState::Draft, _, _) => (format!("{n} draft"), crew::MUTED),
        (_, Some(ReviewState::ChangesRequested), _) => {
            (format!("{n} changes asked"), crew::STOPPED)
        }
        (_, _, Some(ChecksState::Failed)) => (format!("{n} checks failed"), crew::STOPPED),
        (_, Some(ReviewState::Approved), _) => (format!("{n} approved"), crew::ADDED),
        (_, _, Some(ChecksState::Passed)) => (format!("{n} checks passed"), crew::WORKING),
        (_, _, Some(ChecksState::Running)) => (format!("{n} checks running"), crew::WORKING),
        _ => (format!("{n} open"), crew::MUTED),
    }
}

/// The members of every tree working in the lane's folder.
fn lane_crew(lane: &Lane, sessions: &[SessionView]) -> Vec<crew::Member> {
    lane.sessions
        .iter()
        .flat_map(|root| crew::crew_members(root, sessions, None))
        .collect()
}

fn dirty_totals(lane: &Lane) -> (usize, u32, u32) {
    lane.dirty
        .iter()
        .fold((0, 0, 0), |(files, added, removed), file| {
            (files + 1, added + file.added, removed + file.removed)
        })
}

/// The lane's graph: spaces up to its fork on main, then its commits, `●`
/// for one on the remote and `○` for one only on the machine, and `◌` for
/// files not committed.
fn lane_graph(lane: &Lane, main_len: usize) -> String {
    let fork = lane
        .fork_index
        .map_or(0, |index| main_len.saturating_sub(1 + index as usize));
    let mut marks: Vec<&str> = lane
        .commits
        .iter()
        .rev()
        .map(|commit| if commit.pushed { "●" } else { "○" })
        .collect();
    if marks.len() > LANE_MARKS {
        marks.drain(..marks.len() - LANE_MARKS + 1);
        marks.insert(0, "┄");
    }
    let mut graph = format!("{}╰{}", " ".repeat(fork * 2), marks.join("─"));
    if !lane.dirty.is_empty() {
        graph.push_str(" ◌");
    }
    graph
}

fn lane_line(
    lane: &Lane,
    project: &ProjectView,
    sessions: &[SessionView],
    selected: bool,
) -> Line<'static> {
    let colour = hull(&branch_name(lane));
    let mut spans = vec![
        Span::styled(
            if selected { "› " } else { "  " },
            Style::default().fg(crew::ACCENT),
        ),
        Span::styled(
            pad(&branch_name(lane), NAME),
            Style::default().fg(colour).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::styled(
            pad(
                &lane_graph(lane, project.main.len()),
                graph_width(project.main.len()),
            ),
            Style::default().fg(colour),
        ),
    ];
    let members = lane_crew(lane, sessions);
    if members.is_empty() {
        spans.push(Span::styled("—", dim()));
    }
    for member in members.iter().take(4) {
        spans.push(crew::tag_span(&member.persona));
    }
    let tags = members.len().clamp(1, 4) * 2;
    spans.push(Span::raw(" ".repeat(10usize.saturating_sub(tags))));
    spans.push(Span::styled(
        pad(&format!("{} {}", lane.node, kind_word(lane)), 18),
        dim(),
    ));
    spans.push(Span::raw(pad(
        &format!("{}↑ {}↓", lane.ahead, lane.behind),
        8,
    )));
    let (files, added, removed) = dirty_totals(lane);
    if files > 0 {
        spans.push(Span::raw(format!("◌ {files} ")));
        spans.push(Span::styled(
            format!("+{added}"),
            Style::default().fg(crew::ADDED),
        ));
        if removed > 0 {
            spans.push(Span::styled(
                format!(" −{removed}"),
                Style::default().fg(crew::REMOVED),
            ));
        }
        spans.push(Span::raw("  "));
    }
    if let Some(pr) = &lane.pr {
        let (label, colour) = pr_label(pr);
        spans.push(Span::styled(label, Style::default().fg(colour)));
    }
    Line::from(spans)
}

fn kind_word(lane: &Lane) -> &'static str {
    match lane.kind {
        bosun_common::project::CopyKind::Clone => "clone",
        bosun_common::project::CopyKind::Folder => "folder",
        bosun_common::project::CopyKind::Worktree => "worktree",
    }
}

/// Who a feed entry names: the crew member whose call made the change, you
/// for a change made outside Bosun, or nobody for news from GitHub.
fn feed_who(entry: &FeedEntry, sessions: &[SessionView]) -> String {
    if let Some(id) = &entry.session {
        let root = sessions
            .iter()
            .find(|view| view.session.id == *id)
            .map(|view| view.session.owner_id.clone());
        let name = root.and_then(|root| {
            crew::crew_members(&root, sessions, None)
                .into_iter()
                .find(|member| member.id == *id)
                .map(|member| member.name)
        });
        return name.unwrap_or_else(|| {
            entry
                .persona
                .as_deref()
                .map(crew::display_name)
                .unwrap_or_else(|| "A crew member".into())
        });
    }
    "You".into()
}

/// One feed entry in words, as the pane writes it.
pub fn feed_text(entry: &FeedEntry, sessions: &[SessionView]) -> String {
    let who = feed_who(entry, sessions);
    let text = entry.text.clone().unwrap_or_default();
    let pr = entry
        .pr
        .map_or("the pull request".into(), |n| format!("#{n}"));
    match entry.kind {
        FeedKind::Created => format!("{who} created {text}"),
        FeedKind::Edited => format!("{who} edited {text}"),
        FeedKind::Deleted => format!("{who} deleted {text}"),
        FeedKind::Committed if entry.text.is_some() => format!("{who} committed “{text}”"),
        FeedKind::Committed => {
            format!("and {}", plural(entry.count, "more commit", "more commits"))
        }
        FeedKind::Pushed => format!("{who} pushed {}", plural(entry.count, "commit", "commits")),
        FeedKind::Switched => format!(
            "{who} switched to {}",
            entry.branch.as_deref().unwrap_or("a detached head")
        ),
        FeedKind::PrOpened => format!("{pr} opened: {text}"),
        FeedKind::Checks if entry.checks == Some(ChecksState::Failed) => {
            format!("Checks failed on {pr}")
        }
        FeedKind::Checks => format!("Checks passed on {pr}"),
        FeedKind::Review if entry.review == Some(ReviewState::Approved) => {
            format!("{pr} was approved")
        }
        FeedKind::Review => format!("Changes were asked for on {pr}"),
        FeedKind::Merged => format!("{pr} merged into main"),
        FeedKind::Overlap => format!("{text} is changing on two branches"),
    }
}

/// The overlaps, one line per file, naming its branches.
fn overlap_lines(project: &ProjectView) -> Vec<Line<'static>> {
    let mut paths: Vec<(String, Vec<String>)> = Vec::new();
    for overlap in &project.overlaps {
        let names: Vec<String> = overlap
            .lanes
            .iter()
            .map(|key| {
                project
                    .lanes
                    .iter()
                    .find(|lane| lane.key == *key)
                    .map_or_else(|| key.clone(), branch_name)
            })
            .collect();
        match paths.iter_mut().find(|(path, _)| *path == overlap.path) {
            Some((_, branches)) => {
                for name in names {
                    if !branches.contains(&name) {
                        branches.push(name);
                    }
                }
            }
            None => paths.push((overlap.path.clone(), names)),
        }
    }
    paths
        .into_iter()
        .map(|(path, branches)| {
            Line::from(Span::styled(
                format!("! {path} is changing on {}", branches.join(" and ")),
                Style::default().fg(crew::NEEDS_YOU),
            ))
        })
        .collect()
}

/// The map's lines, from the header to the newest activity. `selected` is
/// the lane the cursor is on; `live` says whether the stream is open, and
/// is None when the map is printed once.
pub fn map_lines(
    project: &ProjectView,
    sessions: &[SessionView],
    mode: Mode,
    selected: usize,
    live: Option<bool>,
) -> Vec<Line<'static>> {
    let mut header = vec![
        Span::styled(
            project.name.clone(),
            Style::default()
                .fg(crew::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} · {}",
                project.url,
                plural(project.lanes.len() as u32, "branch", "branches")
            ),
            dim(),
        ),
    ];
    match live {
        Some(true) => header.push(Span::styled("  ● live", Style::default().fg(crew::WORKING))),
        Some(false) => header.push(Span::styled("  ○ reconnecting", dim())),
        None => {}
    }
    let mut lines = vec![Line::from(header), Line::default()];
    match mode {
        Mode::Branches => branch_lines(project, sessions, selected, &mut lines),
        Mode::Where => where_lines(project, sessions, selected, &mut lines),
    }
    lines.push(Line::default());
    lines.extend(overlap_lines(project));
    let note = match project.pull_requests {
        PrSource::NoToken => Some("pull requests are off: the control plane has no github_token"),
        PrSource::Failing => Some("GitHub did not answer the last read of pull requests"),
        PrSource::On | PrSource::NotGithub => None,
    };
    if let Some(note) = note {
        lines.push(Line::from(Span::styled(note, dim())));
    }
    lines.push(Line::from(Span::styled("─".repeat(72), dim())));
    for entry in project.feed.iter().take(FEED_LINES) {
        let mut spans = vec![
            Span::styled(format!("{}  ", clock(entry.at_secs)), dim()),
            Span::raw(feed_text(entry, sessions)),
        ];
        if entry.added > 0 {
            spans.push(Span::styled(
                format!(" +{}", entry.added),
                Style::default().fg(crew::ADDED),
            ));
        }
        if entry.removed > 0 {
            spans.push(Span::styled(
                format!(" −{}", entry.removed),
                Style::default().fg(crew::REMOVED),
            ));
        }
        if let Some(branch) = &entry.branch {
            spans.push(Span::styled(
                format!("  {branch}"),
                Style::default().fg(hull(branch)),
            ));
        }
        lines.push(Line::from(spans));
    }
    if project.feed.is_empty() {
        lines.push(Line::from(Span::styled(
            "nothing has happened since the control plane started",
            dim(),
        )));
    }
    lines
}

fn branch_lines(
    project: &ProjectView,
    sessions: &[SessionView],
    selected: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let main_graph = vec!["●"; project.main.len()].join("─");
    let mut main = vec![
        Span::raw("  "),
        Span::styled(
            pad(&project.main_branch, NAME),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(pad(&main_graph, graph_width(project.main.len())), dim()),
    ];
    if let Some(head) = project.main.first() {
        main.push(Span::styled(
            format!("{} {}", &head.sha[..head.sha.len().min(7)], head.subject),
            dim(),
        ));
    }
    lines.push(Line::from(main));
    for (index, lane) in project.lanes.iter().enumerate() {
        lines.push(lane_line(lane, project, sessions, index == selected));
    }
    for merged in &project.merged {
        let what = merged
            .pr
            .map_or("merged".into(), |pr| format!("merged #{pr}"));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(pad(&merged.branch, NAME), dim()),
            Span::styled(pad("╰─╯", graph_width(project.main.len())), dim()),
            Span::styled(format!("{what} at {}", clock(merged.merged_at_secs)), dim()),
        ]));
    }
    if project.remote_only > 0 {
        let place = if project.web_url.is_some() {
            "GitHub"
        } else {
            "the remote"
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(" ".repeat(NAME)),
            Span::styled(
                format!(
                    "and {} only on {place}",
                    plural(project.remote_only, "branch", "branches")
                ),
                dim(),
            ),
        ]));
    }
}

/// The lanes by machine, each worktree under the folder it belongs to.
fn where_lines(
    project: &ProjectView,
    sessions: &[SessionView],
    selected: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let mut nodes: Vec<&str> = project
        .lanes
        .iter()
        .map(|lane| lane.node.as_str())
        .collect();
    nodes.sort();
    nodes.dedup();
    for node in nodes {
        lines.push(Line::from(Span::styled(
            node.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (index, lane) in where_order(project, node) {
            let colour = hull(&branch_name(lane));
            let mut spans = vec![
                Span::styled(
                    if index == selected { "› " } else { "  " },
                    Style::default().fg(crew::ACCENT),
                ),
                Span::raw(if lane.kind == bosun_common::project::CopyKind::Worktree {
                    "  └ "
                } else {
                    ""
                }),
                Span::raw(format!("{}  ", lane.root)),
                Span::styled(format!("{}  ", kind_word(lane)), dim()),
                Span::styled(
                    format!("{}  ", branch_name(lane)),
                    Style::default().fg(colour),
                ),
            ];
            let (files, added, removed) = dirty_totals(lane);
            if files > 0 {
                spans.push(Span::raw(format!("◌ {files} ")));
                spans.push(Span::styled(
                    format!("+{added}"),
                    Style::default().fg(crew::ADDED),
                ));
                if removed > 0 {
                    spans.push(Span::styled(
                        format!(" −{removed}"),
                        Style::default().fg(crew::REMOVED),
                    ));
                }
                spans.push(Span::raw("  "));
            }
            for member in lane_crew(lane, sessions).iter().take(4) {
                spans.push(crew::tag_span(&member.persona));
            }
            lines.push(Line::from(spans));
        }
    }
}

/// A node's lanes with their index in the project: folders first, each
/// followed by its worktrees.
fn where_order<'a>(project: &'a ProjectView, node: &str) -> Vec<(usize, &'a Lane)> {
    let lanes: Vec<(usize, &Lane)> = project
        .lanes
        .iter()
        .enumerate()
        .filter(|(_, lane)| lane.node == node)
        .collect();
    let mut ordered: Vec<(usize, &Lane)> = Vec::new();
    for (index, lane) in lanes.iter().filter(|(_, lane)| lane.worktree_of.is_none()) {
        ordered.push((*index, lane));
        ordered.extend(
            lanes
                .iter()
                .filter(|(_, other)| other.worktree_of.as_deref() == Some(lane.root.as_str())),
        );
    }
    for entry in &lanes {
        if !ordered.iter().any(|(index, _)| *index == entry.0) {
            ordered.push(*entry);
        }
    }
    ordered
}

/// The project a `bosun map` argument names: its id or its name. With none,
/// the only project. An error names the choices when there is more than one.
pub fn pick_project<'a>(
    projects: &'a [ProjectSummary],
    wanted: Option<&str>,
) -> anyhow::Result<&'a ProjectSummary> {
    match wanted {
        Some(wanted) => projects
            .iter()
            .find(|project| project.id == wanted || project.name == wanted)
            .with_context(|| format!("no project is named {wanted}; {}", choices(projects))),
        None => match projects {
            [] => anyhow::bail!("no session works in a git repository yet"),
            [only] => Ok(only),
            _ => anyhow::bail!("name a project: {}", choices(projects)),
        },
    }
}

fn choices(projects: &[ProjectSummary]) -> String {
    if projects.is_empty() {
        return "there are none".into();
    }
    projects
        .iter()
        .map(|project| format!("{} ({})", project.name, project.url))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Draws the project's map. In a terminal it stays open and follows the
/// project's stream; otherwise it prints the map once.
pub async fn run(cp_url: &str, wanted: Option<&str>) -> anyhow::Result<()> {
    let client = crate::cp_client()?;
    let projects: Vec<ProjectSummary> = client
        .get(format!("{cp_url}/projects"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?
        .error_for_status()
        .context("the control plane returned an error for the projects")?
        .json()
        .await
        .context("failed to parse the projects")?;
    let id = pick_project(&projects, wanted)?.id.clone();
    let sessions = crate::fetch_sessions(cp_url).await?;
    if !io::stdout().is_terminal() {
        let project: ProjectView = client
            .get(format!("{cp_url}/projects/{id}"))
            .send()
            .await
            .context("failed to reach the control plane")?
            .error_for_status()
            .context("the control plane returned an error for the project")?
            .json()
            .await
            .context("failed to parse the project")?;
        // Not a terminal: plain letters, as `bosun list` prints them.
        for line in map_lines(&project, &sessions, Mode::Branches, usize::MAX, None) {
            println!("{}", crate::ansi(&line, false));
        }
        return Ok(());
    }
    let open = live(&client, cp_url, &id, sessions).await?;
    match open {
        Some(session) => crate::attach::attach(cp_url, &session).await,
        None => Ok(()),
    }
}

/// The live map's state.
struct MapState {
    project: Option<ProjectView>,
    sessions: Vec<SessionView>,
    mode: Mode,
    selected: usize,
    live: bool,
}

/// Restores the terminal when the map exits, including on error or panic.
struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Runs the live map until the reader quits, or picks a lane to open. Returns
/// the session to open.
async fn live(
    client: &reqwest::Client,
    cp_url: &str,
    id: &str,
    sessions: Vec<SessionView>,
) -> anyhow::Result<Option<String>> {
    terminal::enable_raw_mode()?;
    let restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let quit = Arc::new(AtomicBool::new(false));
    let (keys_tx, mut keys) = mpsc::unbounded_channel();
    let reader_quit = Arc::clone(&quit);
    std::thread::Builder::new()
        .name("bosun-map-input".to_string())
        .spawn(move || {
            while !reader_quit.load(Ordering::Relaxed) {
                if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                    let Ok(event) = event::read() else { break };
                    if keys_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        })?;
    let mut state = MapState {
        project: None,
        sessions,
        mode: Mode::Branches,
        selected: 0,
        live: false,
    };
    let result = follow(&mut terminal, client, cp_url, id, &mut state, &mut keys).await;
    quit.store(true, Ordering::Relaxed);
    drop(restore);
    result
}

async fn follow(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &reqwest::Client,
    cp_url: &str,
    id: &str,
    state: &mut MapState,
    keys: &mut mpsc::UnboundedReceiver<TermEvent>,
) -> anyhow::Result<Option<String>> {
    let mut sessions_tick = tokio::time::interval(SESSIONS_INTERVAL);
    loop {
        draw(terminal, state)?;
        let stream = client
            .get(format!("{cp_url}/projects/{id}/events"))
            .send()
            .await
            .ok()
            .and_then(|response| response.error_for_status().ok())
            .map(|response| sse_stream(response.bytes_stream()));
        let Some(stream) = stream else {
            state.live = false;
            draw(terminal, state)?;
            if let Some(done) = wait_keys(terminal, state, keys, RECONNECT_DELAY).await? {
                return Ok(done);
            }
            continue;
        };
        tokio::pin!(stream);
        state.live = true;
        loop {
            tokio::select! {
                item = stream.next() => match item {
                    Some(Ok(sse)) if sse.event.as_deref() == Some("gone") => return Ok(None),
                    Some(Ok(sse)) => {
                        if let Ok(view) = serde_json::from_str::<ProjectView>(&sse.data) {
                            state.project = Some(view);
                            clamp(state);
                            draw(terminal, state)?;
                        }
                    }
                    Some(Err(_)) | None => {
                        state.live = false;
                        break;
                    }
                },
                key = keys.recv() => {
                    let Some(key) = key else { return Ok(None) };
                    if let Some(done) = handle_key(state, key) {
                        return Ok(done);
                    }
                    draw(terminal, state)?;
                }
                _ = sessions_tick.tick() => {
                    if let Ok(sessions) = crate::fetch_sessions(cp_url).await {
                        state.sessions = sessions;
                        draw(terminal, state)?;
                    }
                }
            }
        }
        draw(terminal, state)?;
        if let Some(done) = wait_keys(terminal, state, keys, RECONNECT_DELAY).await? {
            return Ok(done);
        }
    }
}

/// Handles keys for `wait`, so the reader can quit while the stream is down.
async fn wait_keys(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut MapState,
    keys: &mut mpsc::UnboundedReceiver<TermEvent>,
    wait: Duration,
) -> anyhow::Result<Option<Option<String>>> {
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Ok(None),
            key = keys.recv() => {
                let Some(key) = key else { return Ok(Some(None)) };
                if let Some(done) = handle_key(state, key) {
                    return Ok(Some(done));
                }
                draw(terminal, state)?;
            }
        }
    }
}

fn clamp(state: &mut MapState) {
    let lanes = state
        .project
        .as_ref()
        .map_or(0, |project| project.lanes.len());
    state.selected = state.selected.min(lanes.saturating_sub(1));
}

/// What a key does: Some(None) quits, Some(Some(id)) opens that session, and
/// None keeps the map open.
fn handle_key(state: &mut MapState, event: TermEvent) -> Option<Option<String>> {
    let TermEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let lanes = state
        .project
        .as_ref()
        .map_or(0, |project| project.lanes.len());
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(None),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Some(None),
        KeyCode::Up | KeyCode::Char('k') => state.selected = state.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            state.selected = (state.selected + 1).min(lanes.saturating_sub(1));
        }
        KeyCode::Char('1') => state.mode = Mode::Branches,
        KeyCode::Char('2') | KeyCode::Char('w') => state.mode = Mode::Where,
        KeyCode::Enter => {
            let lane = state.project.as_ref()?.lanes.get(state.selected)?;
            return Some(lane.sessions.first().cloned());
        }
        _ => {}
    }
    None
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &MapState,
) -> anyhow::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let mut lines = match &state.project {
            Some(project) => map_lines(
                project,
                &state.sessions,
                state.mode,
                state.selected,
                Some(state.live),
            ),
            None => vec![Line::from(Span::styled("reading the project…", dim()))],
        };
        let footer = Line::from(vec![
            Span::styled(
                " 1 Branches ",
                if state.mode == Mode::Branches {
                    Style::default().bg(crew::ACCENT).fg(Color::Black)
                } else {
                    dim()
                },
            ),
            Span::styled(
                " 2 Where ",
                if state.mode == Mode::Where {
                    Style::default().bg(crew::ACCENT).fg(Color::Black)
                } else {
                    dim()
                },
            ),
            Span::styled("   ↑↓ lane · enter open its session · q quit", dim()),
        ]);
        let room = area.height.saturating_sub(1) as usize;
        lines.truncate(room);
        while lines.len() < room {
            lines.push(Line::default());
        }
        lines.push(footer);
        frame.render_widget(Paragraph::new(lines), area);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bosun_common::project::CommitInfo;
    use bosun_common::project::CopyKind;
    use bosun_common::project::DirtyFile;
    use bosun_common::project::FileOp;
    use bosun_common::project::Overlap;

    use super::*;

    fn commit(sha: &str, pushed: bool) -> CommitInfo {
        CommitInfo {
            sha: sha.into(),
            subject: format!("Commit {sha}"),
            author: "Ann".into(),
            time_secs: 0,
            pushed,
        }
    }

    fn lane(key: &str, branch: &str, kind: CopyKind) -> Lane {
        Lane {
            key: key.into(),
            node: "local".into(),
            root: format!("/w/{branch}"),
            kind,
            worktree_of: (kind == CopyKind::Worktree).then(|| "/w/crew-view".into()),
            branch: Some(branch.into()),
            head: Some("h2".into()),
            fork_index: Some(1),
            ahead: 2,
            behind: 1,
            commits: vec![commit("h2", false), commit("h1", true)],
            dirty: vec![DirtyFile {
                path: "api.rs".into(),
                op: FileOp::Edited,
                added: 3,
                removed: 1,
            }],
            sessions: Vec::new(),
            pr: None,
        }
    }

    fn project() -> ProjectView {
        ProjectView {
            id: "p1".into(),
            name: "bosun".into(),
            url: "github.com/ragnarula/bosun".into(),
            web_url: Some("https://github.com/ragnarula/bosun".into()),
            main_branch: "main".into(),
            main: vec![
                commit("m3", false),
                commit("m2", false),
                commit("m1", false),
            ],
            lanes: vec![
                lane("a", "crew-view", CopyKind::Folder),
                lane("b", "project-map", CopyKind::Worktree),
            ],
            merged: Vec::new(),
            remote_only: 2,
            overlaps: vec![Overlap {
                path: "api.rs".into(),
                lanes: ["a".into(), "b".into()],
            }],
            pull_requests: PrSource::NoToken,
            feed: vec![FeedEntry {
                at_secs: 0,
                kind: FeedKind::Created,
                lane: Some("b".into()),
                branch: Some("project-map".into()),
                session: None,
                persona: None,
                text: Some("watch.rs".into()),
                added: 31,
                removed: 0,
                count: 0,
                pr: None,
                checks: None,
                review: None,
            }],
        }
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_lane_leaves_main_at_its_fork_with_pushed_and_local_commits() {
        let lane = lane("a", "crew-view", CopyKind::Folder);
        // Main lists three commits, newest first; the fork is the second
        // newest, so the lane leaves from the second mark from the left.
        assert_eq!(lane_graph(&lane, 3), "  ╰●─○ ◌");
        let older = Lane {
            fork_index: None,
            dirty: Vec::new(),
            commits: (0..14).map(|i| commit(&format!("c{i}"), true)).collect(),
            ..lane
        };
        assert_eq!(
            lane_graph(&older, 3),
            format!("╰┄─{}", ["●"; LANE_MARKS - 1].join("─")),
            "a fork older than main's list starts at the left, and old commits fold"
        );
    }

    #[test]
    fn the_map_names_each_lane_the_overlap_the_quiet_branches_and_the_news() {
        let lines: Vec<String> = map_lines(&project(), &[], Mode::Branches, 1, None)
            .iter()
            .map(text)
            .collect();
        assert!(
            lines[0].starts_with("bosun  github.com/ragnarula/bosun · 2 branches"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("main") && lines[2].contains("●─●─●"),
            "{lines:?}"
        );
        let lane = &lines[4];
        assert!(
            lane.starts_with("› project-map"),
            "the cursor marks its lane: {lane}"
        );
        assert!(
            lane.contains("local worktree") && lane.contains("2↑ 1↓") && lane.contains("◌ 1 +3 −1"),
            "{lane}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("and 2 branches only on GitHub")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "! api.rs is changing on crew-view and project-map"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("no github_token")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("You created watch.rs +31  project-map")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_lane_that_leaves_mains_newest_commit_is_drawn_whole() {
        let mut project = project();
        project.main = (0..12).map(|i| commit(&format!("m{i}"), false)).collect();
        project.lanes[0].fork_index = Some(0);
        let graph = lane_graph(&project.lanes[0], project.main.len());
        let line = text(&lane_line(&project.lanes[0], &project, &[], false));
        assert!(line.contains(&graph), "{line}");
    }

    #[test]
    fn where_lists_each_worktree_under_its_folder() {
        let lines: Vec<String> = map_lines(&project(), &[], Mode::Where, 0, None)
            .iter()
            .map(text)
            .collect();
        let at = lines.iter().position(|line| line == "local").unwrap();
        assert!(
            lines[at + 1].starts_with("› /w/crew-view  folder  crew-view"),
            "{lines:?}"
        );
        assert!(
            lines[at + 2].starts_with("    └ /w/project-map  worktree  project-map"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_pull_request_badge_leads_with_what_matters_most() {
        let pr = |state, checks, review| PullRequest {
            number: 7,
            title: "t".into(),
            url: String::new(),
            state,
            checks,
            review,
            merged_at_secs: None,
        };
        let label = |pr: PullRequest| pr_label(&pr).0;
        assert_eq!(
            label(pr(PrState::Open, Some(ChecksState::Running), None)),
            "#7 checks running"
        );
        assert_eq!(
            label(pr(
                PrState::Open,
                Some(ChecksState::Passed),
                Some(ReviewState::Approved)
            )),
            "#7 approved"
        );
        assert_eq!(
            label(pr(
                PrState::Open,
                Some(ChecksState::Failed),
                Some(ReviewState::Approved)
            )),
            "#7 checks failed",
            "a failure wins over an approval"
        );
        assert_eq!(label(pr(PrState::Merged, None, None)), "#7 merged");
    }

    #[test]
    fn the_only_project_is_picked_and_more_than_one_must_be_named() {
        let summary = |id: &str, name: &str| ProjectSummary {
            id: id.into(),
            name: name.into(),
            url: format!("github.com/o/{name}"),
            lanes: 1,
            overlaps: 0,
            sessions: Vec::new(),
        };
        let one = [summary("p1", "bosun")];
        assert_eq!(pick_project(&one, None).unwrap().id, "p1");
        let two = [summary("p1", "bosun"), summary("p2", "lanyard")];
        let error = pick_project(&two, None).unwrap_err().to_string();
        assert_eq!(
            error,
            "name a project: bosun (github.com/o/bosun), lanyard (github.com/o/lanyard)"
        );
        assert_eq!(pick_project(&two, Some("lanyard")).unwrap().id, "p2");
        assert_eq!(pick_project(&two, Some("p1")).unwrap().name, "bosun");
        assert!(pick_project(&[], None).is_err());
    }
}
