//! The crew a session tree is drawn as in the terminal: each member's flag tag
//! and state word, and the threads, tree, flow, task list and file list built
//! from the tree stream. `bosun list` and `bosun open` both draw from here.

use std::borrow::Cow;
use std::collections::HashMap;

use bosun_common::session::ActivityPhase;
use bosun_common::session::Block;
use bosun_common::session::ChildEventKind;
use bosun_common::session::Event;
use bosun_common::session::Message;
use bosun_common::session::Role;
use bosun_common::session::SessionState;
use bosun_common::session::SessionView;
use bosun_control::api::USER_REJECTED_TEXT;
use chrono::Local;
use chrono::TimeZone;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line as TuiLine;
use ratatui::text::Span;
use serde_json::Value;

use crate::attach::wrap_text;

// The Bosun Signal palette. The four flag colours draw the tags; the rest
// colour headings, state words and diff lines.
pub const FLAG_RED: Color = Color::Rgb(0xe5, 0x33, 0x2a);
pub const FLAG_YELLOW: Color = Color::Rgb(0xff, 0xcc, 0x00);
pub const FLAG_BLUE: Color = Color::Rgb(0x1f, 0x5f, 0xd1);
pub const FLAG_WHITE: Color = Color::Rgb(0xf4, 0xf6, 0xfa);
pub const ACCENT: Color = Color::Rgb(0x6a, 0xa6, 0xff);
pub const WORKING: Color = Color::Rgb(0x2d, 0xd4, 0xbf);
pub const NEEDS_YOU: Color = Color::Rgb(0xff, 0xb0, 0x20);
pub const STOPPED: Color = Color::Rgb(0xff, 0x6b, 0x6b);
pub const MUTED: Color = Color::Rgb(0x5d, 0x70, 0x90);
pub const ADDED: Color = Color::Rgb(0x4a, 0xde, 0x80);
pub const REMOVED: Color = Color::Rgb(0xff, 0x8a, 0x8a);
pub const BRASS: Color = Color::Rgb(0xd6, 0xa8, 0x5c);

/// Thread entries kept in memory across the whole tree; the oldest drop when
/// the cap is hit.
pub const MAX_CHAT_ENTRIES: usize = 5000;
/// Changed paths kept for the Files view; the oldest change drops first.
pub const MAX_FILES: usize = 500;
/// Open `edit` and `file_write` calls waiting for their result. A call whose
/// result never comes is forgotten when the map is cleared at the cap.
const MAX_PENDING_CALLS: usize = 256;
/// Diff lines an edit shows under its row.
pub const MAX_DIFF_LINES: usize = 6;
/// Distinct kinds of call one folded line counts; further kinds are dropped.
const MAX_FOLD_ITEMS: usize = 8;
/// Rows a report, question or failure line shows before it is cut.
const MAX_EVENT_ROWS: usize = 3;
/// The longest shell command a folded line names.
const FOLD_COMMAND_CHARS: usize = 40;
/// Columns the body of a chat entry is indented under its head line.
const CHAT_INDENT: &str = "   ";

/// An International Code of Signals flag. Each persona flies one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    P,
    K,
    O,
    U,
    G,
    D,
    T,
    J,
    N,
    X,
}

/// The flags a persona outside the five known ones may get.
const SPARE_FLAGS: [Flag; 5] = [Flag::D, Flag::T, Flag::J, Flag::N, Flag::X];

impl Flag {
    /// The tag's foreground and background: the flag's two main colours.
    pub fn colours(self) -> (Color, Color) {
        match self {
            Flag::P => (FLAG_WHITE, FLAG_BLUE),
            Flag::K => (FLAG_BLUE, FLAG_YELLOW),
            Flag::O => (FLAG_YELLOW, FLAG_RED),
            Flag::U => (FLAG_RED, FLAG_WHITE),
            Flag::G => (FLAG_YELLOW, FLAG_BLUE),
            Flag::D => (FLAG_BLUE, FLAG_YELLOW),
            Flag::T => (FLAG_WHITE, FLAG_RED),
            Flag::J => (FLAG_WHITE, FLAG_BLUE),
            Flag::N => (FLAG_BLUE, FLAG_WHITE),
            Flag::X => (FLAG_BLUE, FLAG_WHITE),
        }
    }
}

/// The flag a persona flies. The five known personas have fixed flags; any
/// other name gets a spare flag chosen by a hash of its lowercase name, so a
/// persona keeps its flag across runs and machines.
pub fn persona_flag(persona: &str) -> Flag {
    match persona.to_ascii_lowercase().as_str() {
        "lead" => Flag::P,
        "architect" => Flag::K,
        "builder" => Flag::O,
        "reviewer" => Flag::U,
        "researcher" => Flag::G,
        other => SPARE_FLAGS[(fnv1a(other.as_bytes()) % SPARE_FLAGS.len() as u32) as usize],
    }
}

/// 32-bit FNV-1a: small, stable across platforms and Rust versions, which
/// `std`'s hasher is not.
pub fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// The two letters a persona's tag shows.
pub fn tag_text(persona: &str) -> String {
    match persona.to_ascii_lowercase().as_str() {
        "lead" => "Ld".into(),
        "architect" => "Ar".into(),
        "builder" => "Bu".into(),
        "reviewer" => "Rv".into(),
        "researcher" => "Rs".into(),
        other => {
            let mut letters = other.chars().filter(|c| c.is_alphanumeric());
            let first = letters.next().map_or('?', |c| c.to_ascii_uppercase());
            let second = letters.next().unwrap_or(' ');
            format!("{first}{second}")
        }
    }
}

/// A persona's tag: its two letters in its flag's colours.
pub fn tag_span(persona: &str) -> Span<'static> {
    let (fg, bg) = persona_flag(persona).colours();
    Span::styled(
        tag_text(persona),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

/// A persona's name as a crew member is called: its first letter capitalised.
pub fn display_name(persona: &str) -> String {
    let mut chars = persona.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One session of a tree as the crew shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub parent_id: Option<String>,
    pub persona: String,
    /// The persona's name, numbered from the second member of one persona.
    pub name: String,
    pub state: SessionState,
    pub asking: bool,
    /// Direct children that are not stopped.
    pub live_children: usize,
}

/// The persona a session with none named runs: the catalog's default, or
/// `lead` when the catalog is unknown.
const FALLBACK_PERSONA: &str = "lead";

/// The members of the tree `root_id` names: the root first, then the others
/// in the order they were created. A second member of one persona is named
/// "Builder 2", so two members never share a name.
pub fn crew_members(
    root_id: &str,
    sessions: &[SessionView],
    default_persona: Option<&str>,
) -> Vec<Member> {
    let mut tree: Vec<&SessionView> = sessions
        .iter()
        .filter(|view| view.session.owner_id == root_id || view.session.id == root_id)
        .collect();
    tree.sort_by(|a, b| {
        let key = |view: &&SessionView| {
            (
                view.session.id != root_id,
                view.session.created_at_secs,
                view.session.id.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    let mut seen: HashMap<String, usize> = HashMap::new();
    tree.iter()
        .map(|view| {
            let session = &view.session;
            let persona = session
                .persona
                .clone()
                .or_else(|| default_persona.map(str::to_string))
                .unwrap_or_else(|| FALLBACK_PERSONA.to_string());
            let count = seen.entry(persona.to_ascii_lowercase()).or_default();
            *count += 1;
            let name = if *count == 1 {
                display_name(&persona)
            } else {
                format!("{} {count}", display_name(&persona))
            };
            let live_children = sessions
                .iter()
                .filter(|child| {
                    child.session.parent_id.as_deref() == Some(session.id.as_str())
                        && child.session.state != SessionState::Stopped
                })
                .count();
            Member {
                id: session.id.clone(),
                parent_id: session.parent_id.clone(),
                persona,
                name,
                state: session.state,
                asking: view.overview.asking,
                live_children,
            }
        })
        .collect()
}

/// The word that follows a member's tag. Colour never carries the state
/// alone: the word always says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateWord {
    NeedsYou,
    /// The origin leaf of the question the root waits on.
    AsksYou,
    /// A session between the root and the leaf of the question it waits on.
    Waiting,
    Working,
    WaitingOn(usize),
    /// A child that finished its turn: it has reported.
    Done,
    /// A root that finished its turn and waits for the reader.
    Idle,
    Stopped,
}

impl StateWord {
    pub fn text(self) -> Cow<'static, str> {
        match self {
            StateWord::NeedsYou => "needs you".into(),
            StateWord::AsksYou => "asks you".into(),
            StateWord::Waiting => "waiting".into(),
            StateWord::Working => "working".into(),
            StateWord::WaitingOn(count) => format!("waiting on {count}").into(),
            StateWord::Done => "done".into(),
            StateWord::Idle => "idle".into(),
            StateWord::Stopped => "stopped".into(),
        }
    }

    pub fn colour(self) -> Color {
        match self {
            StateWord::NeedsYou | StateWord::AsksYou => NEEDS_YOU,
            StateWord::Working | StateWord::WaitingOn(_) => WORKING,
            StateWord::Waiting | StateWord::Done | StateWord::Idle => MUTED,
            StateWord::Stopped => STOPPED,
        }
    }
}

pub fn state_word(member: &Member) -> StateWord {
    if member.asking {
        return StateWord::NeedsYou;
    }
    match member.state {
        SessionState::Running | SessionState::Creating => StateWord::Working,
        SessionState::WaitingForInput if member.live_children > 0 => {
            StateWord::WaitingOn(member.live_children)
        }
        SessionState::WaitingForInput if member.parent_id.is_some() => StateWord::Done,
        SessionState::WaitingForInput => StateWord::Idle,
        SessionState::Interrupted | SessionState::Stopped => StateWord::Stopped,
    }
}

/// What a member doing `tool` is said to be doing, before its target.
pub fn tool_verb(tool: &str) -> Cow<'static, str> {
    match tool {
        "edit" | "file_write" => "editing".into(),
        "file_read" | "webfetch" => "reading".into(),
        "grep" | "glob" => "searching for".into(),
        "shell" => "running".into(),
        "skill" => "loading skill".into(),
        "spawn" => "starting a".into(),
        "message_child" => "messaging".into(),
        "ask" => "asking".into(),
        other => format!("running {other}").into(),
    }
}

/// The last part of a path: a caption names the file, not where it lives.
fn file_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

/// What a member is doing, read from its newest loop activity: "editing
/// winsw.ts", "running npm test", "messaging Builder". A child named by
/// `message_child` is called by its member name when `members` has it.
pub fn activity_caption(phase: &ActivityPhase, members: &[Member]) -> Option<String> {
    match phase {
        ActivityPhase::ToolStarted { name, target } => {
            let verb = tool_verb(name);
            let Some(target) = target else {
                return Some(verb.into_owned());
            };
            let target = match name.as_str() {
                "edit" | "file_write" | "file_read" => file_name(target).to_string(),
                "message_child" => members
                    .iter()
                    .find(|member| member.id == *target)
                    .map_or_else(|| short_id(target), |member| member.name.clone()),
                _ => target.clone(),
            };
            Some(format!("{verb} {target}"))
        }
        ActivityPhase::WakeStarted
        | ActivityPhase::RequestSent { .. }
        | ActivityPhase::FirstToken { .. }
        | ActivityPhase::ResponseComplete { .. }
        | ActivityPhase::ToolFinished { .. }
        | ActivityPhase::EmptyRetry { .. } => Some("thinking".into()),
        ActivityPhase::CompactionStarted { .. } | ActivityPhase::CompactionFinished { .. } => {
            Some("compacting".into())
        }
        ActivityPhase::WakeDropped { .. } => None,
    }
}

/// The first eight characters of a session id, for a session the crew does
/// not list.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// One side of an exchange between the reader and the agents of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Party {
    You,
    Session(String),
    /// A persona a spawn starts, before the result names the child.
    Persona(String),
}

/// One kind of tool call a folded line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldItem {
    Read,
    Search,
    Run(String),
    Fetch,
    Skill(String),
    Tasks,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub added: bool,
    pub text: String,
}

/// How an order ended, read from the child's next event after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderOutcome {
    Done,
    Asked,
    Failed,
}

impl From<ChildEventKind> for OrderOutcome {
    fn from(kind: ChildEventKind) -> Self {
        match kind {
            ChildEventKind::Report => OrderOutcome::Done,
            ChildEventKind::Ask => OrderOutcome::Asked,
            ChildEventKind::Failure => OrderOutcome::Failed,
        }
    }
}

/// A `spawn` or `message_child` call: an order from a session to its child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub call_id: String,
    /// The child; None until the spawn's result names it.
    pub child: Option<String>,
    /// The persona a spawn starts.
    pub persona: Option<String>,
    pub text: String,
    /// Set by the child's newest event after the order, or by a failed call.
    pub outcome: Option<OrderOutcome>,
}

/// What one entry of a session's thread holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// A user-role text: the reader's message in the root's thread, an order
    /// from the parent in a child's.
    Received(String),
    /// The session's own text.
    Text(String),
    /// The tool calls the session made between messages, counted by kind.
    Fold(Vec<(FoldItem, usize)>),
    Edit {
        path: String,
        added: u64,
        removed: u64,
        diff: Vec<DiffLine>,
    },
    Order(Order),
    /// The session's own question. `raised_from` names the direct child whose
    /// question it surfaces, and `origin` the leaf that first asked it.
    Ask {
        message: String,
        options: Vec<String>,
        raised_from: Option<String>,
        origin: Option<String>,
        answer: Option<String>,
    },
    /// A child's report, question or failure to the session.
    ChildEvent {
        child: String,
        kind: ChildEventKind,
        text: String,
        origin: Option<String>,
    },
    /// The reader's answer to a question in the root's thread, delivered to
    /// `to`. Only the flow draws it: the thread draws the answer under its
    /// question.
    Answer {
        to: String,
        text: String,
    },
    /// The reader rejected the question on screen.
    Rejected,
    /// The session cleared its context, for the reason it gave.
    Cleared(String),
}

/// One entry of a session's thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The session whose thread holds the entry.
    pub thread: String,
    pub at_ms: Option<u64>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Todo,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub kind: Option<String>,
    /// The child session the task is assigned to.
    pub owner: Option<String>,
}

/// Reads one item of an `Event::Todos`. An item without content is skipped.
fn parse_todo(item: &Value) -> Option<TodoItem> {
    let content = item["content"].as_str()?.to_string();
    let status = match item["status"].as_str() {
        Some("in_progress") => TodoStatus::InProgress,
        Some("done") => TodoStatus::Done,
        _ => TodoStatus::Todo,
    };
    Some(TodoItem {
        content,
        status,
        kind: item["kind"].as_str().map(str::to_string),
        owner: item["owner"].as_str().map(str::to_string),
    })
}

/// The task list's groups in the order the Tasks view shows them, empty
/// groups left out.
pub fn task_groups(items: &[TodoItem]) -> Vec<(&'static str, Vec<&TodoItem>)> {
    [
        ("IN PROGRESS", TodoStatus::InProgress),
        ("TO DO", TodoStatus::Todo),
        ("DONE", TodoStatus::Done),
    ]
    .into_iter()
    .map(|(heading, status)| {
        let group: Vec<&TodoItem> = items.iter().filter(|item| item.status == status).collect();
        (heading, group)
    })
    .filter(|(_, group)| !group.is_empty())
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Created,
    Edited,
    Deleted,
}

impl FileOp {
    pub fn letter(self) -> char {
        match self {
            FileOp::Created => 'C',
            FileOp::Edited => 'E',
            FileOp::Deleted => 'D',
        }
    }
}

/// One path the crew changed, with every change to it summed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub op: FileOp,
    /// Lines added and removed, summed over the changes that counted them.
    /// None when only shell runs changed the path: they report no counts.
    pub lines: Option<(u64, u64)>,
    /// The member that changed it last.
    pub by: String,
    pub at_ms: Option<u64>,
    /// The newest edit's diff lines; empty when no edit changed it.
    pub diff: Vec<DiffLine>,
}

/// The edit or write the Files view leads with: the newest one, running or
/// finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowEdit {
    pub by: String,
    pub path: String,
    /// The last two lines of the text it writes.
    pub tail: Vec<String>,
    pub finished: bool,
    call: String,
}

/// An `edit` or `file_write` call waiting for its result, which carries the
/// counts the chat and the Files view show.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCall {
    by: String,
    name: String,
    args: Value,
}

/// What the tree stream says about a crew: every member's thread, the task
/// list, the files changed and each member's newest activity. Members and
/// their states come from the session list, and the stream's state events
/// keep them current.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crew {
    pub root_id: String,
    pub members: Vec<Member>,
    /// The entries of every thread in the tree, in stream order.
    pub entries: Vec<Entry>,
    /// The root's streaming text, until the durable text replaces it.
    pub chat_delta: Option<String>,
    pub todos: Vec<TodoItem>,
    /// Newest change first.
    pub files: Vec<FileChange>,
    pub now_edit: Option<NowEdit>,
    pub activity: HashMap<String, ActivityPhase>,
    /// The leaf whose question the root's open surfaced ask carries.
    question_leaf: Option<String>,
    pending: HashMap<String, PendingCall>,
}

impl Crew {
    pub fn new(root_id: &str) -> Self {
        Self {
            root_id: root_id.to_string(),
            ..Self::default()
        }
    }

    /// Replaces the members with the session list's view of the tree, and
    /// forgets the activity of sessions that left it.
    pub fn set_members(&mut self, members: Vec<Member>) {
        self.activity
            .retain(|id, _| members.iter().any(|member| member.id == *id));
        self.members = members;
    }

    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.id == id)
    }

    /// Applies one durable event `session_id` wrote.
    pub fn apply(&mut self, session_id: &str, event: &Event) {
        let is_root = session_id == self.root_id;
        match event {
            Event::Message { message, at_ms } => {
                if let Some(member) = self.members.iter_mut().find(|m| m.id == session_id) {
                    member.asking = matches!(message.block, Block::Ask { answer: None, .. });
                }
                // Any message of the root closes its open question; an open
                // surfaced ask sets the leaf again below.
                if is_root {
                    self.question_leaf = None;
                }
                self.apply_message(session_id, message, *at_ms);
            }
            Event::Activity { phase, .. } => {
                self.activity.insert(session_id.to_string(), phase.clone());
            }
            Event::Todos { items, .. } if is_root => {
                self.todos = items.iter().filter_map(parse_todo).collect();
            }
            Event::State { state, .. } => {
                if let Some(member) = self.members.iter_mut().find(|m| m.id == session_id) {
                    member.state = *state;
                }
                if is_root {
                    self.chat_delta = None;
                }
            }
            _ => {}
        }
    }

    /// Appends a live text delta of the root's.
    pub fn apply_delta(&mut self, text: &str) {
        self.chat_delta
            .get_or_insert_with(String::new)
            .push_str(text);
    }

    fn apply_message(&mut self, session_id: &str, message: &Message, at_ms: Option<u64>) {
        let is_root = session_id == self.root_id;
        match (&message.role, &message.block) {
            (Role::User, Block::Text { text }) if is_root && text == USER_REJECTED_TEXT => {
                self.push(session_id, at_ms, EntryKind::Rejected);
            }
            (Role::User, Block::Text { text }) => {
                if self.answer_open_ask(session_id, text) {
                    if is_root {
                        self.push(
                            session_id,
                            at_ms,
                            EntryKind::Answer {
                                to: session_id.to_string(),
                                text: text.clone(),
                            },
                        );
                    }
                    return;
                }
                self.push(session_id, at_ms, EntryKind::Received(text.clone()));
            }
            (Role::Assistant, Block::Text { text }) => {
                if is_root {
                    self.chat_delta = None;
                }
                if !text.trim().is_empty() {
                    self.push(session_id, at_ms, EntryKind::Text(text.clone()));
                }
            }
            (_, Block::ToolCall { id, name, args, .. }) => {
                self.apply_call(session_id, id, name, args, at_ms);
            }
            (
                _,
                Block::ToolResult {
                    id,
                    name,
                    is_error,
                    content,
                },
            ) => self.apply_result(session_id, id, name, *is_error, content, at_ms),
            (
                _,
                Block::Ask {
                    message,
                    options,
                    child_id,
                    answer,
                },
            ) => self.apply_ask(session_id, message, options, child_id, answer, at_ms),
            (
                _,
                Block::ChildEvent {
                    child_id,
                    kind,
                    text,
                    origin,
                },
            ) => {
                // The event follows every earlier order to the child in this
                // thread, so each of them ends with it.
                for entry in &mut self.entries {
                    if let EntryKind::Order(order) = &mut entry.kind
                        && entry.thread == session_id
                        && order.child.as_deref() == Some(child_id.as_str())
                    {
                        order.outcome = Some((*kind).into());
                    }
                }
                self.push(
                    session_id,
                    at_ms,
                    EntryKind::ChildEvent {
                        child: child_id.clone(),
                        kind: *kind,
                        text: text.clone(),
                        origin: origin.clone(),
                    },
                );
            }
            (_, Block::ContextCleared { reason, .. }) => {
                self.push(session_id, at_ms, EntryKind::Cleared(reason.clone()));
            }
            _ => {}
        }
    }

    /// Fills the thread's open question with `text` when the question is the
    /// thread's newest entry and the session asked it itself. A surfaced
    /// question is answered by the store, not by a text in this thread.
    fn answer_open_ask(&mut self, thread: &str, text: &str) -> bool {
        let newest = self
            .entries
            .iter_mut()
            .rev()
            .filter(|entry| entry.thread == thread)
            .find(|entry| !matches!(entry.kind, EntryKind::Fold(_)));
        let Some(Entry {
            kind:
                EntryKind::Ask {
                    raised_from: None,
                    answer: slot @ None,
                    ..
                },
            ..
        }) = newest
        else {
            return false;
        };
        *slot = Some(text.to_string());
        true
    }

    fn apply_ask(
        &mut self,
        thread: &str,
        message: &str,
        options: &[String],
        child_id: &Option<String>,
        answer: &Option<String>,
        at_ms: Option<u64>,
    ) {
        let is_root = thread == self.root_id;
        if let Some(answer) = answer {
            // The store records a routed answer by appending the answered ask
            // again; it fills the open one.
            let open = self
                .entries
                .iter_mut()
                .rev()
                .find_map(|entry| match &mut entry.kind {
                    EntryKind::Ask {
                        message: asked,
                        answer: slot @ None,
                        origin,
                        ..
                    } if entry.thread == thread && asked == message => Some((slot, origin.clone())),
                    _ => None,
                });
            if let Some((slot, origin)) = open {
                *slot = Some(answer.clone());
                if is_root {
                    self.push(
                        thread,
                        at_ms,
                        EntryKind::Answer {
                            to: origin.unwrap_or_else(|| thread.to_string()),
                            text: answer.clone(),
                        },
                    );
                }
                return;
            }
        }
        // A surfaced question carries the origin of the child's ask event
        // that raised it.
        let origin = child_id.as_ref().map(|child| {
            self.entries
                .iter()
                .rev()
                .find_map(|entry| match &entry.kind {
                    EntryKind::ChildEvent {
                        child: from,
                        kind: ChildEventKind::Ask,
                        origin,
                        ..
                    } if entry.thread == thread && from == child => {
                        Some(origin.clone().unwrap_or_else(|| child.clone()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| child.clone())
        });
        if is_root && answer.is_none() {
            self.question_leaf = origin.clone();
        }
        self.push(
            thread,
            at_ms,
            EntryKind::Ask {
                message: message.to_string(),
                options: options.to_vec(),
                raised_from: child_id.clone(),
                origin,
                answer: answer.clone(),
            },
        );
    }

    fn apply_call(
        &mut self,
        session_id: &str,
        id: &str,
        name: &str,
        args: &Value,
        at_ms: Option<u64>,
    ) {
        let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
        match name {
            "spawn" => self.push(
                session_id,
                at_ms,
                EntryKind::Order(Order {
                    call_id: id.to_string(),
                    child: None,
                    persona: Some(text("persona")),
                    text: text("instructions"),
                    outcome: None,
                }),
            ),
            "message_child" => self.push(
                session_id,
                at_ms,
                EntryKind::Order(Order {
                    call_id: id.to_string(),
                    child: Some(text("id")),
                    persona: None,
                    text: text("text"),
                    outcome: None,
                }),
            ),
            "edit" | "file_write" => {
                let key = call_key(session_id, id);
                let written = if name == "edit" {
                    text("new")
                } else {
                    text("content")
                };
                self.now_edit = Some(NowEdit {
                    by: session_id.to_string(),
                    path: text("path"),
                    tail: last_lines(&written, 2),
                    finished: false,
                    call: key.clone(),
                });
                if self.pending.len() >= MAX_PENDING_CALLS {
                    self.pending.clear();
                }
                self.pending.insert(
                    key,
                    PendingCall {
                        by: session_id.to_string(),
                        name: name.to_string(),
                        args: args.clone(),
                    },
                );
            }
            "ask" => {}
            _ => self.fold(session_id, fold_item(name, args)),
        }
    }

    fn apply_result(
        &mut self,
        session_id: &str,
        id: &str,
        name: &str,
        is_error: bool,
        content: &Value,
        at_ms: Option<u64>,
    ) {
        match name {
            // A spawn's result names the child its order went to; a failed
            // call means the order never reached a child.
            "spawn" | "message_child" => {
                let order = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.kind {
                        EntryKind::Order(order)
                            if entry.thread == session_id && order.call_id == id =>
                        {
                            Some(order)
                        }
                        _ => None,
                    });
                let Some(order) = order else {
                    return;
                };
                if is_error {
                    order.outcome = Some(OrderOutcome::Failed);
                } else if name == "spawn" {
                    order.child = content["child_id"].as_str().map(str::to_string);
                }
            }
            "edit" | "file_write" => {
                let key = call_key(session_id, id);
                let Some(call) = self.pending.remove(&key) else {
                    return;
                };
                if let Some(now) = &mut self.now_edit
                    && now.call == key
                {
                    now.finished = true;
                }
                if is_error {
                    return;
                }
                let path = call.args["path"].as_str().unwrap_or_default().to_string();
                let added = content["added"].as_u64().unwrap_or(0);
                let removed = content["removed"].as_u64().unwrap_or(0);
                let created = content["created"].as_bool() == Some(true);
                let diff = if call.name == "edit" {
                    edit_diff(
                        call.args["old"].as_str().unwrap_or_default(),
                        call.args["new"].as_str().unwrap_or_default(),
                    )
                } else {
                    Vec::new()
                };
                self.push(
                    &call.by,
                    at_ms,
                    EntryKind::Edit {
                        path: path.clone(),
                        added,
                        removed,
                        diff: diff.clone(),
                    },
                );
                self.record_file(FileChange {
                    path,
                    op: if created {
                        FileOp::Created
                    } else {
                        FileOp::Edited
                    },
                    lines: Some((added, removed)),
                    by: call.by,
                    at_ms,
                    diff,
                });
            }
            // A failed command may still have changed files, so the error
            // flag is not checked.
            "shell" => {
                let Some(files) = content["files"].as_array() else {
                    return;
                };
                for file in files {
                    let Some(path) = file["path"].as_str() else {
                        continue;
                    };
                    let op = match file["op"].as_str() {
                        Some("created") => FileOp::Created,
                        Some("deleted") => FileOp::Deleted,
                        _ => FileOp::Edited,
                    };
                    self.record_file(FileChange {
                        path: path.to_string(),
                        op,
                        lines: None,
                        by: session_id.to_string(),
                        at_ms,
                        diff: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Moves `change`'s path to the front of the file list, summed with what
    /// the list already held for it.
    fn record_file(&mut self, mut change: FileChange) {
        if let Some(index) = self.files.iter().position(|file| file.path == change.path) {
            let earlier = self.files.remove(index);
            change.op = match (earlier.op, change.op) {
                (_, FileOp::Deleted) => FileOp::Deleted,
                // A file the crew created is still new after an edit.
                (FileOp::Created, FileOp::Edited) => FileOp::Created,
                (_, op) => op,
            };
            change.lines = match (earlier.lines, change.lines) {
                (Some((a, r)), Some((added, removed))) => Some((a + added, r + removed)),
                (earlier, None) => earlier,
                (None, now) => now,
            };
            if change.diff.is_empty() {
                change.diff = earlier.diff;
            }
        }
        self.files.insert(0, change);
        self.files.truncate(MAX_FILES);
    }

    /// Counts a tool call into the thread's folded line. Only a folded line
    /// that is the thread's newest entry counts it, so any other entry of the
    /// thread starts a new line.
    fn fold(&mut self, thread: &str, item: FoldItem) {
        let newest = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.thread == thread);
        if let Some(Entry {
            kind: EntryKind::Fold(items),
            ..
        }) = newest
        {
            if let Some((_, count)) = items.iter_mut().find(|(kind, _)| *kind == item) {
                *count += 1;
            } else if items.len() < MAX_FOLD_ITEMS {
                items.push((item, 1));
            }
            return;
        }
        self.push(thread, None, EntryKind::Fold(vec![(item, 1)]));
    }

    fn push(&mut self, thread: &str, at_ms: Option<u64>, kind: EntryKind) {
        if self.entries.len() >= MAX_CHAT_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            thread: thread.to_string(),
            at_ms,
            kind,
        });
    }
}

/// Whether `entry` belongs in the thread of session `id`: the session's own
/// entries, and its reports and failures, which sit in its parent's thread.
/// Its questions are its own ask entries, so the parent's copy is left out.
fn in_thread(id: &str, entry: &Entry) -> bool {
    entry.thread == id
        || matches!(&entry.kind, EntryKind::ChildEvent { child, kind, .. }
            if child == id && *kind != ChildEventKind::Ask)
}

/// The entries the thread of session `id` draws, oldest first.
pub fn thread_entries<'a>(crew: &'a Crew, id: &'a str) -> impl Iterator<Item = &'a Entry> {
    crew.entries
        .iter()
        .filter(move |entry| in_thread(id, entry))
}

/// The sessions from `id` up its `parent_id` chain to the root, `id` first.
/// A chain longer than the crew is cut there, so a loop in the list ends.
pub fn path_to_root(members: &[Member], id: &str) -> Vec<String> {
    let mut path = vec![id.to_string()];
    while path.len() <= members.len() {
        let current = path.last().map(String::as_str).unwrap_or_default();
        let Some(parent) = members
            .iter()
            .find(|member| member.id == current)
            .and_then(|member| member.parent_id.clone())
        else {
            break;
        };
        path.push(parent);
    }
    path
}

/// The sessions a surfaced question climbs while the root waits on it: the
/// origin leaf first, the root last. Empty when the root waits on none.
pub fn question_path(crew: &Crew) -> Vec<String> {
    crew.question_leaf
        .as_deref()
        .map(|leaf| path_to_root(&crew.members, leaf))
        .unwrap_or_default()
}

/// A member's state word in the tree. While the root waits on a surfaced
/// question, its origin leaf asks you and each session between them waits.
pub fn crew_word(crew: &Crew, member: &Member) -> StateWord {
    let path = question_path(crew);
    if path.len() > 1 {
        if path[0] == member.id {
            return StateWord::AsksYou;
        }
        if path[1..path.len() - 1].contains(&member.id) {
            return StateWord::Waiting;
        }
    }
    state_word(member)
}

/// What an order row shows after the child's tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    /// A spawn whose child the session list does not show yet.
    Starting,
    /// The child's live state, until its next event.
    Live(StateWord),
    Ended(OrderOutcome),
}

impl OrderState {
    pub fn text(self) -> Cow<'static, str> {
        match self {
            OrderState::Starting => "starting".into(),
            OrderState::Live(word) => word.text(),
            OrderState::Ended(OrderOutcome::Done) => "done".into(),
            OrderState::Ended(OrderOutcome::Asked) => "asked".into(),
            OrderState::Ended(OrderOutcome::Failed) => "failed".into(),
        }
    }

    pub fn colour(self) -> Color {
        match self {
            OrderState::Starting => WORKING,
            OrderState::Live(word) => word.colour(),
            OrderState::Ended(OrderOutcome::Done) => MUTED,
            OrderState::Ended(OrderOutcome::Asked) => NEEDS_YOU,
            OrderState::Ended(OrderOutcome::Failed) => STOPPED,
        }
    }
}

pub fn order_state(crew: &Crew, order: &Order) -> OrderState {
    if let Some(outcome) = order.outcome {
        return OrderState::Ended(outcome);
    }
    match order.child.as_deref().and_then(|id| crew.member(id)) {
        Some(member) => OrderState::Live(crew_word(crew, member)),
        None => OrderState::Starting,
    }
}

/// The child an order went to, or the persona a spawn starts.
fn order_party(order: &Order) -> Party {
    match (&order.child, &order.persona) {
        (Some(child), _) => Party::Session(child.clone()),
        (None, Some(persona)) => Party::Persona(persona.clone()),
        (None, None) => Party::Persona("agent".into()),
    }
}

/// What passes between two parties of a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    Order,
    Report,
    /// A child's question to its parent.
    Ask,
    Failure,
    /// The root's question to the reader.
    Question,
    Answer,
}

impl FlowKind {
    fn label(self) -> &'static str {
        match self {
            FlowKind::Order => "order",
            FlowKind::Report => "reported",
            FlowKind::Ask => "asked",
            FlowKind::Failure => "failed",
            FlowKind::Question => "asks",
            FlowKind::Answer => "answer",
        }
    }
}

impl From<ChildEventKind> for FlowKind {
    fn from(kind: ChildEventKind) -> Self {
        match kind {
            ChildEventKind::Report => FlowKind::Report,
            ChildEventKind::Ask => FlowKind::Ask,
            ChildEventKind::Failure => FlowKind::Failure,
        }
    }
}

/// One order, report, question, answer or failure in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowItem<'a> {
    pub from: Party,
    pub to: Party,
    pub kind: FlowKind,
    pub text: &'a str,
    pub at_ms: Option<u64>,
}

/// Everything that passed between the parties of the tree, newest first.
pub fn flow(crew: &Crew) -> Vec<FlowItem<'_>> {
    crew.entries
        .iter()
        .rev()
        .filter_map(|entry| {
            let owner = || Party::Session(entry.thread.clone());
            let (from, to, kind, text) = match &entry.kind {
                EntryKind::Order(order) => {
                    (owner(), order_party(order), FlowKind::Order, &order.text)
                }
                EntryKind::ChildEvent {
                    child, kind, text, ..
                } => (Party::Session(child.clone()), owner(), (*kind).into(), text),
                EntryKind::Ask { message, .. } if entry.thread == crew.root_id => {
                    (owner(), Party::You, FlowKind::Question, message)
                }
                EntryKind::Answer { to, text } => (
                    Party::You,
                    Party::Session(to.clone()),
                    FlowKind::Answer,
                    text,
                ),
                _ => return None,
            };
            Some(FlowItem {
                from,
                to,
                kind,
                text: text.as_str(),
                at_ms: entry.at_ms,
            })
        })
        .collect()
}

/// The newest order a member received or report it sent; for the root, the
/// reader's newest message.
pub fn newest_exchange<'a>(crew: &'a Crew, id: &str) -> Option<(FlowKind, &'a str)> {
    let is_root = id == crew.root_id;
    crew.entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::Order(order) if order.child.as_deref() == Some(id) => {
                Some((FlowKind::Order, order.text.as_str()))
            }
            EntryKind::ChildEvent {
                child, kind, text, ..
            } if child == id => Some(((*kind).into(), text.as_str())),
            EntryKind::Received(text) if is_root && entry.thread == id => {
                Some((FlowKind::Order, text.as_str()))
            }
            _ => None,
        })
}

/// One member's row in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub id: String,
    /// The root is at depth 0, under the reader.
    pub depth: usize,
    /// The connector lines before the member's tag.
    pub lead: String,
    /// The connector lines before the rows drawn under the member.
    pub under: String,
}

/// The crew as a tree: the root, then each member under its `parent_id`,
/// siblings in the order the crew lists them. A member whose parent is not in
/// the crew hangs under the root. Empty until the session list names the root.
pub fn tree_order(crew: &Crew) -> Vec<TreeNode> {
    let Some(root) = crew.member(&crew.root_id) else {
        return Vec::new();
    };
    let parent_of = |member: &Member| -> String {
        match &member.parent_id {
            Some(parent) if crew.member(parent).is_some() => parent.clone(),
            _ => crew.root_id.clone(),
        }
    };
    let mut nodes = Vec::new();
    let mut seen = vec![root.id.clone()];
    // Each frame is a member, its depth, the lines its ancestors draw, and
    // whether it is the last of its siblings.
    let mut stack: Vec<(&Member, usize, String, bool)> = vec![(root, 0, String::new(), true)];
    while let Some((member, depth, ancestors, last)) = stack.pop() {
        let (branch, rest) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        nodes.push(TreeNode {
            id: member.id.clone(),
            depth,
            lead: format!("{ancestors}{branch}"),
            under: format!("{ancestors}{rest}"),
        });
        let children: Vec<&Member> = crew
            .members
            .iter()
            .filter(|child| child.id != crew.root_id && parent_of(child) == member.id)
            .filter(|child| !seen.contains(&child.id))
            .collect();
        seen.extend(children.iter().map(|child| child.id.clone()));
        let count = children.len();
        let ancestors = format!("{ancestors}{rest}");
        // Pushed in reverse so the first child is drawn first.
        for (index, child) in children.into_iter().enumerate().rev() {
            stack.push((child, depth + 1, ancestors.clone(), index + 1 == count));
        }
    }
    nodes
}

/// Tool call ids come from the provider, so the session is part of the key.
fn call_key(session_id: &str, id: &str) -> String {
    format!("{session_id}/{id}")
}

fn last_lines(text: &str, count: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(count)..]
        .iter()
        .map(|line| line.to_string())
        .collect()
}

fn fold_item(name: &str, args: &Value) -> FoldItem {
    match name {
        "file_read" => FoldItem::Read,
        "grep" | "glob" => FoldItem::Search,
        "shell" => {
            let command = args["command"].as_str().unwrap_or_default();
            let first = command
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .trim();
            FoldItem::Run(clip(first, FOLD_COMMAND_CHARS))
        }
        "webfetch" => FoldItem::Fetch,
        "skill" => FoldItem::Skill(args["name"].as_str().unwrap_or_default().to_string()),
        "todowrite" => FoldItem::Tasks,
        other => FoldItem::Other(other.to_string()),
    }
}

fn times(count: usize) -> String {
    match count {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// A folded line's text: "read 4 files · ran npm test once".
pub fn fold_text(items: &[(FoldItem, usize)]) -> String {
    items
        .iter()
        .map(|(item, count)| match item {
            FoldItem::Read => format!("read {}", plural(*count, "file", "files")),
            FoldItem::Search => format!("searched {}", times(*count)),
            FoldItem::Run(command) => format!("ran {command} {}", times(*count)),
            FoldItem::Fetch => format!("fetched {}", plural(*count, "page", "pages")),
            FoldItem::Skill(skill) => format!("loaded skill {skill}"),
            FoldItem::Tasks => "updated the tasks".to_string(),
            FoldItem::Other(tool) => format!("used {tool} {}", times(*count)),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The lines an edit changed: the old text's lines after the start and end it
/// shares with the new text, then the new text's, at most `MAX_DIFF_LINES`.
pub fn edit_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let start = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = old[start..old.len() - end].iter().map(|text| DiffLine {
        added: false,
        text: text.to_string(),
    });
    let added = new[start..new.len() - end].iter().map(|text| DiffLine {
        added: true,
        text: text.to_string(),
    });
    removed.chain(added).take(MAX_DIFF_LINES).collect()
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// The local `HH:MM` a stamp names.
pub fn clock(at_ms: u64) -> Option<String> {
    Local
        .timestamp_millis_opt(at_ms as i64)
        .single()
        .map(|time| time.format("%H:%M").to_string())
}

/// How long ago a stamp was: "12s", "4m", "2h", "3d".
pub fn age(now_ms: u64, at_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A task bar `cells` wide: done cells in green, in-progress cells in blue,
/// the rest dim.
pub fn task_bar(done: usize, in_progress: usize, total: usize, cells: usize) -> Vec<Span<'static>> {
    if total == 0 {
        return Vec::new();
    }
    let done_cells = (done * cells / total).min(cells);
    let progress_cells = (in_progress * cells)
        .div_ceil(total)
        .min(cells - done_cells);
    let rest = cells - done_cells - progress_cells;
    [(done_cells, ADDED), (progress_cells, ACCENT), (rest, MUTED)]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, colour)| Span::styled("━".repeat(count), Style::default().fg(colour)))
        .collect()
}

/// A member's persona and name, or a placeholder for a session the crew does
/// not list.
fn who(crew: &Crew, id: &str) -> (String, String) {
    match crew.member(id) {
        Some(member) => (member.persona.clone(), member.name.clone()),
        None => ("agent".into(), short_id(id)),
    }
}

fn party_name(crew: &Crew, party: &Party) -> String {
    match party {
        Party::You => "You".into(),
        Party::Session(id) => who(crew, id).1,
        Party::Persona(persona) => display_name(persona),
    }
}

/// A party's tag and name, or "You" for the reader.
fn party_spans(crew: &Crew, party: &Party, colour: Option<Color>) -> Vec<Span<'static>> {
    let bold = colour
        .map_or_else(Style::default, |colour| Style::default().fg(colour))
        .add_modifier(Modifier::BOLD);
    match party {
        Party::You => vec![Span::styled(
            "You",
            Style::default().fg(BRASS).add_modifier(Modifier::BOLD),
        )],
        Party::Session(id) => {
            let (persona, name) = who(crew, id);
            vec![tag_span(&persona), Span::raw(" "), Span::styled(name, bold)]
        }
        Party::Persona(persona) => vec![
            tag_span(persona),
            Span::raw(" "),
            Span::styled(display_name(persona), bold),
        ],
    }
}

/// The party a session's questions and reports go to: the reader for the
/// root, the parent for a child.
fn upward(crew: &Crew, id: &str) -> Party {
    if id == crew.root_id {
        return Party::You;
    }
    crew.member(id)
        .and_then(|member| member.parent_id.clone())
        .map_or(Party::Persona("parent".into()), Party::Session)
}

/// The names from the root down to member `id`: "Lead › Builder".
pub fn member_path(crew: &Crew, id: &str) -> String {
    let mut path = path_to_root(&crew.members, id);
    path.reverse();
    path.iter()
        .map(|id| who(crew, id).1)
        .collect::<Vec<_>>()
        .join(" › ")
}

fn dim() -> Style {
    Style::default().fg(MUTED)
}

/// `<from> → <to> · HH:MM`, or `<from> · HH:MM` with no recipient.
fn head_line(
    crew: &Crew,
    from: &Party,
    to: Option<&Party>,
    at_ms: Option<u64>,
    colour: Option<Color>,
) -> TuiLine<'static> {
    let mut spans = party_spans(crew, from, colour);
    if let Some(to) = to {
        spans.push(Span::styled(format!(" → {}", party_name(crew, to)), dim()));
    }
    if let Some(time) = at_ms.and_then(clock) {
        spans.push(Span::styled(format!(" · {time}"), dim()));
    }
    TuiLine::from(spans)
}

/// `text` wrapped under a head line, indented, in `style`.
fn body_lines(text: &str, width: usize, style: Style) -> Vec<TuiLine<'static>> {
    let inner = width.saturating_sub(CHAT_INDENT.len()).max(1);
    wrap_text(text, inner)
        .into_iter()
        .map(|row| TuiLine::from(vec![Span::raw(CHAT_INDENT), Span::styled(row, style)]))
        .collect()
}

/// `lead` and `text` on one line, wrapped to `width` and cut to
/// `MAX_EVENT_ROWS` rows, the last cut row ending in '…'.
fn short_lines(
    lead: Vec<Span<'static>>,
    text: &str,
    width: usize,
    style: Style,
) -> Vec<TuiLine<'static>> {
    let lead_width: usize = lead.iter().map(|span| span.content.chars().count()).sum();
    let inner = width.saturating_sub(lead_width).max(1);
    let mut rows = wrap_text(text, inner);
    if rows.len() > MAX_EVENT_ROWS {
        rows.truncate(MAX_EVENT_ROWS);
        if let Some(last) = rows.last_mut() {
            *last = format!("{}…", clip(last, inner.saturating_sub(1).max(1)));
        }
    }
    let pad = " ".repeat(lead_width);
    let mut lead = Some(lead);
    rows.into_iter()
        .map(|row| {
            let mut spans = lead.take().unwrap_or_else(|| vec![Span::raw(pad.clone())]);
            spans.push(Span::styled(row, style));
            TuiLine::from(spans)
        })
        .collect()
}

fn diff_lines(diff: &[DiffLine], width: usize) -> Vec<TuiLine<'static>> {
    let inner = width.saturating_sub(CHAT_INDENT.len() + 2).max(1);
    diff.iter()
        .map(|line| {
            let (sign, colour) = if line.added {
                ("+ ", ADDED)
            } else {
                ("− ", REMOVED)
            };
            TuiLine::from(vec![
                Span::raw(CHAT_INDENT),
                Span::styled(
                    format!("{sign}{}", clip(&line.text, inner)),
                    Style::default().fg(colour),
                ),
            ])
        })
        .collect()
}

fn counts_spans(added: u64, removed: u64) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("+{added}"), Style::default().fg(ADDED)),
        Span::raw(" "),
        Span::styled(format!("−{removed}"), Style::default().fg(REMOVED)),
    ]
}

/// The first non-blank line of `text`.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// One order row: `→ <tag> <Child>  <first line>  <state>`.
fn order_line(crew: &Crew, order: &Order, width: usize) -> TuiLine<'static> {
    let state = order_state(crew, order);
    let mut spans = vec![Span::styled(format!("{CHAT_INDENT}→ "), dim())];
    spans.extend(party_spans(crew, &order_party(order), None));
    let state_text = state.text().into_owned();
    let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let room = width
        .saturating_sub(used + state_text.chars().count() + 4)
        .max(1);
    spans.push(Span::raw(format!(
        "  {}  ",
        clip(first_line(&order.text), room)
    )));
    spans.push(Span::styled(
        state_text,
        Style::default().fg(state.colour()),
    ));
    clip_spans(spans, width)
}

/// The Chat view: the root's thread, with its streaming text last.
pub fn chat_lines(crew: &Crew, width: usize) -> Vec<TuiLine<'static>> {
    thread_lines(crew, &crew.root_id, width)
}

/// A session's thread, wrapped to `width`: the orders it received, its text,
/// edits and folded tool calls, its questions with their answers, its orders
/// to its children with their state, and the reports between it and them.
/// The root's thread is the Chat: its received texts are the reader's.
pub fn thread_lines(crew: &Crew, id: &str, width: usize) -> Vec<TuiLine<'static>> {
    let is_root = id == crew.root_id;
    let me = Party::Session(id.to_string());
    let up = upward(crew, id);
    let mut lines = Vec::new();
    for entry in thread_entries(crew, id) {
        let at_ms = entry.at_ms;
        match &entry.kind {
            EntryKind::Received(text) => {
                lines.push(TuiLine::default());
                let head = if is_root {
                    head_line(crew, &Party::You, None, at_ms, None)
                } else {
                    head_line(crew, &up, Some(&me), at_ms, None)
                };
                lines.push(head);
                lines.extend(body_lines(text, width, Style::default()));
            }
            EntryKind::Text(text) => {
                lines.push(TuiLine::default());
                lines.push(head_line(crew, &me, None, at_ms, None));
                lines.extend(body_lines(text, width, Style::default()));
            }
            EntryKind::Fold(items) => {
                let text = clip(
                    &fold_text(items),
                    width.saturating_sub(CHAT_INDENT.len()).max(1),
                );
                lines.push(TuiLine::from(vec![
                    Span::raw(CHAT_INDENT),
                    Span::styled(text, dim()),
                ]));
            }
            EntryKind::Edit {
                path,
                added,
                removed,
                diff,
            } => {
                let mut spans = vec![
                    Span::raw(CHAT_INDENT),
                    Span::styled(format!("✎ {path}  "), Style::default().fg(ACCENT)),
                ];
                spans.extend(counts_spans(*added, *removed));
                lines.push(TuiLine::from(spans));
                lines.extend(diff_lines(diff, width));
            }
            EntryKind::Order(order) => lines.push(order_line(crew, order, width)),
            EntryKind::ChildEvent {
                child, kind, text, ..
            } => {
                // In this thread a child reports to the session; in the
                // child's own thread the session is the child.
                let (from, to) = if entry.thread == id {
                    (Party::Session(child.clone()), me.clone())
                } else {
                    (me.clone(), Party::Session(entry.thread.clone()))
                };
                let style = if *kind == ChildEventKind::Failure {
                    Style::default().fg(STOPPED)
                } else {
                    dim()
                };
                let (persona, name) = match &from {
                    Party::Session(from) => who(crew, from),
                    _ => ("agent".into(), party_name(crew, &from)),
                };
                let verb = match kind {
                    ChildEventKind::Report => {
                        format!(" {name} reported to {}: ", party_name(crew, &to))
                    }
                    ChildEventKind::Ask => format!(" {name} asked {}: ", party_name(crew, &to)),
                    ChildEventKind::Failure => format!(" {name} failed: "),
                };
                lines.extend(short_lines(
                    vec![tag_span(&persona), Span::styled(verb, style)],
                    text,
                    width,
                    style,
                ));
            }
            EntryKind::Ask {
                message,
                options,
                origin,
                answer,
                ..
            } => {
                let amber = Style::default().fg(NEEDS_YOU);
                lines.push(TuiLine::default());
                lines.push(head_line(crew, &me, Some(&up), at_ms, Some(NEEDS_YOU)));
                lines.extend(body_lines(&format!("? {message}"), width, amber));
                if !options.is_empty() {
                    let choices = options
                        .iter()
                        .map(|option| format!("[{option}]"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    lines.extend(body_lines(&choices, width, amber));
                }
                if let Some(leaf) = origin {
                    let mut route: Vec<String> = path_to_root(&crew.members, leaf)
                        .iter()
                        .take_while(|step| *step != id)
                        .map(|step| who(crew, step).1)
                        .collect();
                    route.push(who(crew, id).1);
                    route.push(party_name(crew, &up));
                    let leaf_name = who(crew, leaf).1;
                    lines.extend(body_lines(
                        &format!("asked by {leaf_name} · {}", route.join(" › ")),
                        width,
                        dim(),
                    ));
                    if is_root && answer.is_none() {
                        lines.extend(body_lines(
                            &format!("your answer goes to {leaf_name} word for word"),
                            width,
                            dim(),
                        ));
                    }
                }
                if let Some(answer) = answer {
                    let head = if is_root {
                        head_line(crew, &Party::You, None, None, None)
                    } else {
                        TuiLine::from(Span::styled("Answer", dim().add_modifier(Modifier::BOLD)))
                    };
                    lines.push(head);
                    lines.extend(body_lines(answer, width, Style::default()));
                }
            }
            EntryKind::Answer { .. } => {}
            EntryKind::Rejected => {
                lines.push(TuiLine::from(Span::styled(
                    "  ~ you rejected the question",
                    dim(),
                )));
            }
            EntryKind::Cleared(reason) => {
                lines.push(TuiLine::from(Span::styled(
                    clip(&format!("── context cleared: {reason}"), width.max(1)),
                    dim(),
                )));
            }
        }
    }
    if is_root && let Some(delta) = &crew.chat_delta {
        lines.push(TuiLine::default());
        lines.push(head_line(crew, &me, None, None, None));
        lines.extend(body_lines(delta, width, Style::default()));
    }
    if lines.first().is_some_and(|line| line.spans.is_empty()) {
        lines.remove(0);
    }
    lines
}

/// The Crew view: the tree under "You", each member as its tag, name, state
/// word and caption with its newest order or report under it, `picked`
/// marked; then the flow between the parties, newest first.
pub fn crew_lines(crew: &Crew, picked: &str, width: usize) -> Vec<TuiLine<'static>> {
    let heading = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut lines = vec![TuiLine::from(vec![
        Span::raw("  "),
        Span::styled(
            "You",
            Style::default().fg(BRASS).add_modifier(Modifier::BOLD),
        ),
    ])];
    for node in tree_order(crew) {
        let Some(member) = crew.member(&node.id) else {
            continue;
        };
        let is_picked = node.id == picked;
        let marker = if is_picked { "› " } else { "  " };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(ACCENT)),
            Span::styled(node.lead.clone(), dim()),
        ];
        let name_style = if is_picked {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        spans.push(tag_span(&member.persona));
        spans.push(Span::styled(format!(" {} ", member.name), name_style));
        spans.extend(word_spans(crew, member, true));
        lines.push(clip_spans(spans, width));
        if let Some((kind, text)) = newest_exchange(crew, &node.id) {
            lines.push(clip_spans(
                vec![
                    Span::raw("  "),
                    Span::styled(format!("{}   ", node.under), dim()),
                    Span::styled(format!("{}: {}", kind.label(), first_line(text)), dim()),
                ],
                width,
            ));
        }
    }
    lines.push(TuiLine::default());
    lines.push(TuiLine::from(Span::styled("FLOW", heading)));
    let items = flow(crew);
    if items.is_empty() {
        lines.push(TuiLine::from(Span::styled(
            "nothing between agents yet",
            dim(),
        )));
    }
    for item in items {
        let style = match item.kind {
            FlowKind::Failure => Style::default().fg(STOPPED),
            FlowKind::Question | FlowKind::Ask => Style::default().fg(NEEDS_YOU),
            _ => dim(),
        };
        let mut spans = party_spans(crew, &item.from, None);
        spans.push(Span::styled(" → ", dim()));
        spans.extend(party_spans(crew, &item.to, None));
        if let Some(time) = item.at_ms.and_then(clock) {
            spans.push(Span::styled(format!(" · {time}"), dim()));
        }
        spans.push(Span::styled(
            format!("  {}: {}", item.kind.label(), first_line(item.text)),
            style,
        ));
        lines.push(clip_spans(spans, width));
    }
    lines
}

/// A member's tag, state word and caption, as the crew line and the Tasks
/// view show them.
fn member_spans(crew: &Crew, member: &Member, caption: bool) -> Vec<Span<'static>> {
    let mut spans = vec![tag_span(&member.persona), Span::raw(" ")];
    spans.extend(word_spans(crew, member, caption));
    spans
}

/// A member's state word in the tree, then what it is doing while it works
/// when `caption` is set.
fn word_spans(crew: &Crew, member: &Member, caption: bool) -> Vec<Span<'static>> {
    let word = crew_word(crew, member);
    let mut spans = vec![Span::styled(
        word.text().into_owned(),
        Style::default().fg(word.colour()),
    )];
    if caption
        && word == StateWord::Working
        && let Some(text) = crew
            .activity
            .get(&member.id)
            .and_then(|phase| activity_caption(phase, &crew.members))
    {
        spans.push(Span::styled(format!(" {text}"), dim()));
    }
    spans
}

/// The crew line under the status line: each member's tag, state word and
/// what it is doing, clipped to `width`.
pub fn crew_line(crew: &Crew, width: usize) -> TuiLine<'static> {
    let mut spans = Vec::new();
    for (index, member) in crew.members.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", dim()));
        }
        spans.extend(member_spans(crew, member, true));
    }
    clip_spans(spans, width)
}

/// Cuts spans to `width` characters, ending the cut one with '…'.
pub fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> TuiLine<'static> {
    let mut used = 0;
    let mut kept = Vec::new();
    for span in spans {
        let len = span.content.chars().count();
        if used + len <= width {
            used += len;
            kept.push(span);
            continue;
        }
        let room = width.saturating_sub(used);
        if room > 0 {
            kept.push(Span::styled(clip(&span.content, room), span.style));
        }
        break;
    }
    TuiLine::from(kept)
}

/// The Tasks view: a progress bar, then the task list grouped by status,
/// each row `kind  content  <owner tag> state`.
pub fn tasks_lines(crew: &Crew, width: usize) -> Vec<TuiLine<'static>> {
    let mut lines = Vec::new();
    if crew.todos.is_empty() {
        lines.push(TuiLine::from(Span::styled("no tasks yet", dim())));
        return lines;
    }
    let total = crew.todos.len();
    let done = crew
        .todos
        .iter()
        .filter(|item| item.status == TodoStatus::Done)
        .count();
    let in_progress = crew
        .todos
        .iter()
        .filter(|item| item.status == TodoStatus::InProgress)
        .count();
    let mut bar = task_bar(
        done,
        in_progress,
        total,
        width.saturating_sub(16).clamp(4, 30),
    );
    bar.push(Span::styled(format!("  {done} of {total} done"), dim()));
    lines.push(TuiLine::from(bar));
    for (heading, items) in task_groups(&crew.todos) {
        lines.push(TuiLine::default());
        lines.push(TuiLine::from(Span::styled(
            heading,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for item in items {
            let kind = item.kind.as_deref().unwrap_or("");
            let owner = item
                .owner
                .as_deref()
                .and_then(|owner| crew.member(owner))
                .map(|member| member_spans(crew, member, false))
                .unwrap_or_default();
            let owner_width: usize = owner.iter().map(|span| span.content.chars().count()).sum();
            let content_room = width.saturating_sub(2 + 9 + owner_width + 2).max(8);
            let content_style = if item.status == TodoStatus::Done {
                dim()
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(format!("  {kind:<8} "), dim()),
                Span::styled(
                    format!("{:<content_room$}", clip(&item.content, content_room)),
                    content_style,
                ),
            ];
            if !owner.is_empty() {
                spans.push(Span::raw("  "));
                spans.extend(owner);
            }
            lines.push(clip_spans(spans, width));
        }
    }
    lines
}

/// The Files view. `NOW` leads with the newest edit, then `CHANGED IN THIS
/// SESSION` lists every changed path, newest first, with `selected` marked.
/// With `detail` set, the selected file's diff lines replace the list.
pub fn files_lines(
    crew: &Crew,
    selected: Option<usize>,
    detail: bool,
    width: usize,
    now_ms: u64,
) -> Vec<TuiLine<'static>> {
    let heading = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    if detail && let Some(file) = selected.and_then(|index| crew.files.get(index)) {
        let mut spans = vec![Span::styled(format!("{}  ", file.path), heading)];
        if let Some((added, removed)) = file.lines {
            spans.extend(counts_spans(added, removed));
        }
        spans.push(Span::styled("  (esc back)", dim()));
        lines.push(clip_spans(spans, width));
        if file.diff.is_empty() {
            lines.push(TuiLine::from(Span::styled(
                "no diff lines: the file was written whole or changed by a command",
                dim(),
            )));
        } else {
            lines.extend(diff_lines(&file.diff, width));
        }
        return lines;
    }
    if let Some(now) = &crew.now_edit {
        let (persona, name) = who(crew, &now.by);
        let verb = if now.finished { "edited" } else { "is editing" };
        lines.push(TuiLine::from(Span::styled("NOW", heading)));
        lines.push(clip_spans(
            vec![
                tag_span(&persona),
                Span::raw(format!(" {name} {verb} {}", now.path)),
            ],
            width,
        ));
        for tail in &now.tail {
            lines.push(clip_spans(
                vec![Span::raw(CHAT_INDENT), Span::styled(tail.clone(), dim())],
                width,
            ));
        }
        lines.push(TuiLine::default());
    }
    lines.push(TuiLine::from(Span::styled(
        "CHANGED IN THIS SESSION",
        heading,
    )));
    if crew.files.is_empty() {
        lines.push(TuiLine::from(Span::styled("nothing changed yet", dim())));
        return lines;
    }
    let path_room = width.saturating_sub(4 + 14 + 20).max(12);
    for (index, file) in crew.files.iter().enumerate() {
        let marker = if selected == Some(index) {
            "› "
        } else {
            "  "
        };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(ACCENT)),
            Span::raw(format!("{}  ", file.op.letter())),
            Span::raw(format!("{:<path_room$}", clip(&file.path, path_room))),
        ];
        match file.lines {
            Some((added, removed)) => {
                let counts = format!("+{added} −{removed}");
                spans.push(Span::raw("  "));
                spans.extend(counts_spans(added, removed));
                spans.push(Span::raw(
                    " ".repeat(12usize.saturating_sub(counts.chars().count())),
                ));
            }
            None => spans.push(Span::raw(" ".repeat(14))),
        }
        let (_, name) = who(crew, &file.by);
        let when = file
            .at_ms
            .map(|at_ms| format!(" · {}", age(now_ms, at_ms)))
            .unwrap_or_default();
        spans.push(Span::styled(format!("  {name}{when}"), dim()));
        let mut line = clip_spans(spans, width);
        if selected == Some(index) {
            line = line.style(Style::default().add_modifier(Modifier::BOLD));
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use bosun_common::session::Message;
    use bosun_common::session::Session;
    use bosun_common::session::SessionOverview;
    use serde_json::json;

    use super::*;

    fn line_text(line: &TuiLine) -> String {
        line.to_string()
    }

    fn texts(lines: &[TuiLine]) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    fn view(id: &str, owner: &str, parent: Option<&str>, persona: &str) -> SessionView {
        SessionView {
            session: Session {
                id: id.into(),
                node: "n1".into(),
                repo_url: None,
                git_ref: None,
                dir: "/work".into(),
                model: "m".into(),
                persona: Some(persona.into()),
                parent_id: parent.map(str::to_string),
                owner_id: owner.into(),
                permission: bosun_common::session::Permission::ReadWrite,
                allowed_tools: "*".into(),
                mcp_servers: "".into(),
                state: SessionState::Running,
                interrupt_cause: None,
                created_at_secs: 0,
                prompt: None,
                summary: None,
            },
            overview: SessionOverview::default(),
        }
    }

    fn message(role: Role, block: Block) -> Event {
        Event::Message {
            at_ms: None,
            message: Message { role, block },
        }
    }

    fn call(id: &str, name: &str, args: Value) -> Event {
        message(
            Role::Assistant,
            Block::ToolCall {
                id: id.into(),
                name: name.into(),
                args,
                continues_completion: false,
            },
        )
    }

    fn result(id: &str, name: &str, content: Value) -> Event {
        message(
            Role::User,
            Block::ToolResult {
                id: id.into(),
                name: name.into(),
                is_error: false,
                content,
            },
        )
    }

    fn text(role: Role, text: &str) -> Event {
        message(role, Block::Text { text: text.into() })
    }

    /// A crew of a lead `root` and one builder `child`.
    fn crew() -> Crew {
        let mut crew = Crew::new("root");
        crew.set_members(crew_members(
            "root",
            &[
                view("root", "root", None, "lead"),
                view("child", "root", Some("root"), "builder"),
            ],
            None,
        ));
        crew
    }

    #[test]
    fn the_five_known_personas_fly_their_flags_in_their_colours() {
        let cases = [
            ("lead", Flag::P, "Ld", FLAG_WHITE, FLAG_BLUE),
            ("Architect", Flag::K, "Ar", FLAG_BLUE, FLAG_YELLOW),
            ("builder", Flag::O, "Bu", FLAG_YELLOW, FLAG_RED),
            ("reviewer", Flag::U, "Rv", FLAG_RED, FLAG_WHITE),
            ("researcher", Flag::G, "Rs", FLAG_YELLOW, FLAG_BLUE),
        ];
        for (persona, flag, tag, fg, bg) in cases {
            assert_eq!(persona_flag(persona), flag, "{persona}");
            assert_eq!(tag_text(persona), tag, "{persona}");
            let span = tag_span(persona);
            assert_eq!(span.content, tag);
            assert_eq!(
                (span.style.fg, span.style.bg),
                (Some(fg), Some(bg)),
                "{persona}"
            );
        }
    }

    #[test]
    fn another_persona_gets_a_spare_flag_that_stays_the_same() {
        for persona in ["designer", "tester", "ops", "x"] {
            let flag = persona_flag(persona);
            assert!(SPARE_FLAGS.contains(&flag), "{persona}: {flag:?}");
            assert_eq!(persona_flag(persona), flag, "the hash is stable");
            assert_eq!(
                persona_flag(&persona.to_uppercase()),
                flag,
                "case does not change the flag"
            );
        }
        assert_eq!(tag_text("designer"), "De");
        assert_eq!(tag_text("x"), "X ");
        // The FNV-1a of "designer" picks the same spare flag on every machine.
        assert_eq!(fnv1a(b"designer"), 0xd4f1_9184);
        assert_eq!(persona_flag("designer"), Flag::D);
        assert_eq!(persona_flag("tester"), Flag::X);
    }

    #[test]
    fn crew_members_put_the_root_first_and_number_a_repeated_persona() {
        let mut second = view("b2", "root", Some("root"), "builder");
        second.session.created_at_secs = 5;
        let mut first = view("b1", "root", Some("root"), "builder");
        first.session.created_at_secs = 1;
        let members = crew_members(
            "root",
            &[
                second,
                view("other", "other", None, "lead"),
                first,
                view("root", "root", None, "lead"),
            ],
            None,
        );
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["Lead", "Builder", "Builder 2"]);
        assert_eq!(members[0].live_children, 2);
    }

    #[test]
    fn a_session_without_a_persona_takes_the_default() {
        let mut root = view("root", "root", None, "x");
        root.session.persona = None;
        let members = crew_members("root", &[root.clone()], Some("architect"));
        assert_eq!(members[0].persona, "architect");
        let members = crew_members("root", &[root], None);
        assert_eq!(members[0].persona, "lead");
    }

    #[test]
    fn state_words_follow_the_member() {
        let mut member = crew().members[1].clone();
        member.state = SessionState::Running;
        assert_eq!(state_word(&member), StateWord::Working);
        member.state = SessionState::WaitingForInput;
        assert_eq!(state_word(&member), StateWord::Done);
        member.live_children = 2;
        assert_eq!(state_word(&member), StateWord::WaitingOn(2));
        assert_eq!(StateWord::WaitingOn(2).text(), "waiting on 2");
        member.asking = true;
        assert_eq!(state_word(&member), StateWord::NeedsYou);
        member.asking = false;
        member.state = SessionState::Interrupted;
        assert_eq!(state_word(&member), StateWord::Stopped);

        let mut root = crew().members[0].clone();
        root.state = SessionState::WaitingForInput;
        root.live_children = 0;
        assert_eq!(state_word(&root), StateWord::Idle);
    }

    #[test]
    fn captions_name_what_a_tool_works_on() {
        let members = crew().members;
        let started = |name: &str, target: Option<&str>| ActivityPhase::ToolStarted {
            name: name.into(),
            target: target.map(str::to_string),
        };
        let cases = [
            (started("edit", Some("src/winsw.ts")), "editing winsw.ts"),
            (started("file_write", Some("a/b.md")), "editing b.md"),
            (started("file_read", Some("README.md")), "reading README.md"),
            (started("grep", Some("fn main")), "searching for fn main"),
            (started("glob", Some("*.rs")), "searching for *.rs"),
            (started("shell", Some("npm test")), "running npm test"),
            (
                started("webfetch", Some("https://x.io")),
                "reading https://x.io",
            ),
            (started("skill", Some("tdd")), "loading skill tdd"),
            (started("spawn", Some("reviewer")), "starting a reviewer"),
            (started("message_child", Some("child")), "messaging Builder"),
            (
                started("message_child", Some("0123456789")),
                "messaging 01234567",
            ),
            (started("ask", None), "asking"),
            (started("todowrite", None), "running todowrite"),
            (ActivityPhase::WakeStarted, "thinking"),
            (
                ActivityPhase::CompactionStarted { input_tokens: 1 },
                "compacting",
            ),
        ];
        for (phase, expected) in cases {
            assert_eq!(
                activity_caption(&phase, &members).as_deref(),
                Some(expected),
                "{phase:?}"
            );
        }
        let dropped = ActivityPhase::WakeDropped { reason: "x".into() };
        assert_eq!(activity_caption(&dropped, &members), None);
    }

    fn child_event(child: &str, kind: ChildEventKind, text: &str, origin: Option<&str>) -> Event {
        message(
            Role::Assistant,
            Block::ChildEvent {
                child_id: child.into(),
                kind,
                text: text.into(),
                origin: origin.map(str::to_string),
            },
        )
    }

    fn ask(text: &str, child_id: Option<&str>, answer: Option<&str>) -> Event {
        message(
            Role::Assistant,
            Block::Ask {
                message: text.into(),
                options: vec!["yes".into(), "no".into()],
                child_id: child_id.map(str::to_string),
                answer: answer.map(str::to_string),
            },
        )
    }

    /// A crew three deep: lead `root`, builder `mid` under it, researcher
    /// `leaf` under `mid`.
    fn deep_crew() -> Crew {
        let mut mid = view("mid", "root", Some("root"), "builder");
        mid.session.created_at_secs = 1;
        let mut leaf = view("leaf", "root", Some("mid"), "researcher");
        leaf.session.created_at_secs = 2;
        let mut crew = Crew::new("root");
        crew.set_members(crew_members(
            "root",
            &[view("root", "root", None, "lead"), mid, leaf],
            None,
        ));
        crew
    }

    fn kinds<'a>(entries: impl Iterator<Item = &'a Entry>) -> Vec<&'a EntryKind> {
        entries.map(|entry| &entry.kind).collect()
    }

    fn orders(crew: &Crew) -> Vec<&Order> {
        crew.entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Order(order) => Some(order),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_chat_holds_the_roots_thread_only() {
        let mut crew = crew();
        crew.apply("root", &text(Role::User, "please start"));
        crew.apply("child", &text(Role::User, "build the parser"));
        crew.apply("child", &text(Role::Assistant, "builder says"));
        crew.apply("child", &call("1", "file_read", json!({"path": "a"})));
        crew.apply("root", &text(Role::Assistant, "lead says"));
        assert_eq!(
            kinds(thread_entries(&crew, "root")),
            vec![
                &EntryKind::Received("please start".into()),
                &EntryKind::Text("lead says".into()),
            ]
        );
        let chat = texts(&chat_lines(&crew, 60)).join("\n");
        assert!(chat.contains("lead says"), "{chat}");
        assert!(
            !chat.contains("builder says") && !chat.contains("build the parser"),
            "a child's thread stays out of the chat: {chat}"
        );
        assert_eq!(
            kinds(thread_entries(&crew, "child")),
            vec![
                &EntryKind::Received("build the parser".into()),
                &EntryKind::Text("builder says".into()),
                &EntryKind::Fold(vec![(FoldItem::Read, 1)]),
            ]
        );
    }

    #[test]
    fn an_order_shows_the_childs_live_state_until_its_next_event() {
        let mut crew = crew();
        crew.apply(
            "root",
            &call(
                "1",
                "spawn",
                json!({"persona": "builder", "instructions": "build it\nwith tests"}),
            ),
        );
        assert_eq!(order_state(&crew, orders(&crew)[0]), OrderState::Starting);
        crew.apply("root", &result("1", "spawn", json!({"child_id": "child"})));
        assert_eq!(orders(&crew)[0].child.as_deref(), Some("child"));
        assert_eq!(
            order_state(&crew, orders(&crew)[0]),
            OrderState::Live(StateWord::Working)
        );

        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Report, "built", None),
        );
        assert_eq!(
            order_state(&crew, orders(&crew)[0]),
            OrderState::Ended(OrderOutcome::Done)
        );

        // A later order is live again until the child's next event, which
        // ends every order before it.
        crew.apply(
            "root",
            &call(
                "2",
                "message_child",
                json!({"id": "child", "text": "and docs"}),
            ),
        );
        assert_eq!(
            order_state(&crew, orders(&crew)[1]),
            OrderState::Live(StateWord::Working)
        );
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Ask, "which docs?", Some("child")),
        );
        let states: Vec<OrderState> = orders(&crew)
            .iter()
            .map(|o| order_state(&crew, o))
            .collect();
        assert_eq!(states, vec![OrderState::Ended(OrderOutcome::Asked); 2]);
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Failure, "crashed", None),
        );
        assert_eq!(
            order_state(&crew, orders(&crew)[1]),
            OrderState::Ended(OrderOutcome::Failed)
        );

        // An order to another child is not ended by this child's events.
        crew.apply(
            "root",
            &call("3", "message_child", json!({"id": "other", "text": "hi"})),
        );
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Report, "ok", None),
        );
        assert_eq!(orders(&crew)[2].outcome, None);

        // A spawn that fails never reaches a child.
        crew.apply(
            "root",
            &call(
                "4",
                "spawn",
                json!({"persona": "nobody", "instructions": "x"}),
            ),
        );
        crew.apply(
            "root",
            &message(
                Role::User,
                Block::ToolResult {
                    id: "4".into(),
                    name: "spawn".into(),
                    is_error: true,
                    content: json!({"error": "unknown persona"}),
                },
            ),
        );
        assert_eq!(
            order_state(&crew, orders(&crew)[3]),
            OrderState::Ended(OrderOutcome::Failed)
        );
    }

    #[test]
    fn a_surfaced_question_names_its_leaf_and_the_path_to_the_reader() {
        let mut crew = deep_crew();
        crew.apply("leaf", &ask("which db?", None, None));
        crew.apply(
            "mid",
            &child_event("leaf", ChildEventKind::Ask, "which db?", Some("leaf")),
        );
        crew.apply("mid", &ask("which db?", Some("leaf"), None));
        crew.apply(
            "root",
            &child_event("mid", ChildEventKind::Ask, "which db?", Some("leaf")),
        );
        crew.apply("root", &ask("which db?", Some("mid"), None));

        let root_ask = thread_entries(&crew, "root")
            .find_map(|entry| match &entry.kind {
                EntryKind::Ask {
                    origin,
                    raised_from,
                    ..
                } => Some((origin.clone(), raised_from.clone())),
                _ => None,
            })
            .expect("the root's question");
        assert_eq!(root_ask, (Some("leaf".into()), Some("mid".into())));
        assert_eq!(
            path_to_root(&crew.members, "leaf"),
            vec!["leaf", "mid", "root"]
        );
        assert_eq!(question_path(&crew), vec!["leaf", "mid", "root"]);

        let words: Vec<StateWord> = crew.members.iter().map(|m| crew_word(&crew, m)).collect();
        assert_eq!(
            words,
            vec![StateWord::NeedsYou, StateWord::Waiting, StateWord::AsksYou],
            "root, mid, leaf"
        );
        let chat = texts(&chat_lines(&crew, 80)).join("\n");
        assert!(chat.contains("Researcher › Builder › Lead › You"), "{chat}");

        // The store routes the reader's answer to the leaf and appends the
        // answered question to the root.
        crew.apply("leaf", &text(Role::User, "postgres"));
        crew.apply("root", &ask("which db?", Some("mid"), Some("postgres")));
        assert!(question_path(&crew).is_empty(), "the root waits on nothing");
        assert_ne!(crew_word(&crew, &crew.members[2]), StateWord::AsksYou);
        let answers: Vec<(Option<&String>, &str)> = crew
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Ask { answer, .. } => Some((answer.as_ref(), entry.thread.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![
                (Some(&"postgres".to_string()), "leaf"),
                (None, "mid"),
                (Some(&"postgres".to_string()), "root"),
            ],
            "the leaf's own ask and the root's surfaced one are answered"
        );
        assert_eq!(flow(&crew)[0].kind, FlowKind::Answer);
        assert_eq!(flow(&crew)[0].to, Party::Session("leaf".into()));
    }

    #[test]
    fn an_ask_event_without_an_origin_names_its_child() {
        let mut crew = crew();
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Ask, "push?", None),
        );
        crew.apply("root", &ask("push?", Some("child"), None));
        assert_eq!(question_path(&crew), vec!["child", "root"]);
        assert_eq!(crew_word(&crew, &crew.members[1]), StateWord::AsksYou);

        // A redirect while the question is open is a message, not its answer.
        crew.apply("root", &text(Role::User, "do something else"));
        assert!(question_path(&crew).is_empty());
        assert!(matches!(
            crew.entries.last().map(|entry| &entry.kind),
            Some(EntryKind::Received(text)) if text == "do something else"
        ));
    }

    #[test]
    fn the_tree_puts_each_member_under_its_parent_at_any_depth() {
        let mut builder = view("b", "root", Some("root"), "builder");
        builder.session.created_at_secs = 1;
        let mut reviewer = view("r", "root", Some("root"), "reviewer");
        reviewer.session.created_at_secs = 2;
        let mut grandchild = view("g", "root", Some("b"), "researcher");
        grandchild.session.created_at_secs = 3;
        let mut orphan = view("o", "root", Some("gone"), "architect");
        orphan.session.created_at_secs = 4;
        let mut crew = Crew::new("root");
        crew.set_members(crew_members(
            "root",
            &[
                orphan,
                grandchild,
                reviewer,
                builder,
                view("root", "root", None, "lead"),
            ],
            None,
        ));
        let nodes: Vec<(&str, usize)> = tree_order(&crew)
            .iter()
            .map(|node| (crew.member(&node.id).unwrap().id.as_str(), node.depth))
            .collect::<Vec<_>>()
            .into_iter()
            .collect();
        assert_eq!(
            nodes,
            vec![("root", 0), ("b", 1), ("g", 2), ("r", 1), ("o", 1)],
            "a grandchild sits under its parent; a member whose parent left hangs under the root"
        );
        let order = tree_order(&crew);
        // The grandchild's parent has siblings below it, so its line runs on.
        assert_eq!(order[2].lead.chars().filter(|c| *c == '│').count(), 1);
        assert!(order[4].lead.ends_with("└─ "), "the last sibling closes");
        assert!(
            tree_order(&Crew::new("root")).is_empty(),
            "no root, no tree"
        );
    }

    #[test]
    fn the_flow_lists_what_passed_between_parties_newest_first() {
        let mut crew = crew();
        crew.apply(
            "root",
            &call(
                "1",
                "spawn",
                json!({"persona": "builder", "instructions": "build"}),
            ),
        );
        crew.apply("root", &result("1", "spawn", json!({"child_id": "child"})));
        crew.apply("child", &text(Role::Assistant, "working on it"));
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Report, "built", None),
        );
        crew.apply("root", &ask("ship it?", None, None));
        crew.apply("root", &text(Role::User, "yes"));
        let items: Vec<(FlowKind, Party, Party)> = flow(&crew)
            .into_iter()
            .map(|item| (item.kind, item.from, item.to))
            .collect();
        let root = || Party::Session("root".into());
        let child = || Party::Session("child".into());
        assert_eq!(
            items,
            vec![
                (FlowKind::Answer, Party::You, root()),
                (FlowKind::Question, root(), Party::You),
                (FlowKind::Report, child(), root()),
                (FlowKind::Order, root(), child()),
            ]
        );
        assert_eq!(
            newest_exchange(&crew, "child"),
            Some((FlowKind::Report, "built"))
        );
    }

    #[test]
    fn a_members_thread_holds_its_orders_text_asks_and_reports() {
        let mut crew = crew();
        crew.apply("child", &text(Role::User, "build it"));
        crew.apply("child", &text(Role::Assistant, "built"));
        crew.apply("child", &ask("push?", None, None));
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Ask, "push?", Some("child")),
        );
        crew.apply("child", &text(Role::User, "no"));
        crew.apply(
            "root",
            &child_event("child", ChildEventKind::Report, "built", None),
        );
        let thread = kinds(thread_entries(&crew, "child"));
        assert_eq!(thread.len(), 4, "{thread:?}");
        assert!(matches!(thread[0], EntryKind::Received(_)));
        assert!(matches!(thread[1], EntryKind::Text(_)));
        assert!(
            matches!(thread[2], EntryKind::Ask { answer: Some(answer), .. } if answer == "no"),
            "the parent's answer fills the child's question"
        );
        assert!(
            matches!(
                thread[3],
                EntryKind::ChildEvent {
                    kind: ChildEventKind::Report,
                    ..
                }
            ),
            "its report to the parent is in its thread"
        );
    }

    #[test]
    fn tool_calls_between_messages_fold_into_one_line_per_thread() {
        let mut crew = crew();
        for id in ["1", "2", "3", "4"] {
            crew.apply("child", &call(id, "file_read", json!({"path": "a"})));
        }
        crew.apply("root", &call("5", "grep", json!({"pattern": "x"})));
        crew.apply(
            "child",
            &call("6", "shell", json!({"command": "npm test\nnpm run lint"})),
        );
        assert_eq!(crew.entries.len(), 2, "one folded line per thread");
        assert_eq!(
            kinds(thread_entries(&crew, "child")),
            vec![&EntryKind::Fold(vec![
                (FoldItem::Read, 4),
                (FoldItem::Run("npm test".into()), 1)
            ])]
        );

        // A message starts a new folded line.
        crew.apply("child", &text(Role::Assistant, "read them"));
        crew.apply("child", &call("7", "file_read", json!({"path": "b"})));
        assert_eq!(crew.entries.len(), 4);
    }

    #[test]
    fn the_readers_answer_joins_the_roots_own_question() {
        let mut crew = crew();
        crew.apply("root", &ask("push?", None, None));
        assert!(crew.members[0].asking, "an open ask marks the member");
        assert!(
            question_path(&crew).is_empty(),
            "the root's own question has no leaf"
        );
        crew.apply("root", &text(Role::User, "yes"));
        assert!(!crew.members[0].asking);
        let thread = kinds(thread_entries(&crew, "root"));
        assert!(
            matches!(thread[0], EntryKind::Ask { answer: Some(answer), .. } if answer == "yes"),
            "{thread:?}"
        );
        assert!(
            matches!(thread[1], EntryKind::Answer { to, .. } if to == "root"),
            "the flow records the answer"
        );
    }

    #[test]
    fn reasoning_and_context_rows_stay_out_of_the_threads() {
        let mut crew = crew();
        crew.apply(
            "root",
            &message(Role::Assistant, Block::Reasoning { text: "hm".into() }),
        );
        crew.apply(
            "root",
            &Event::ModelCall {
                at_ms: None,
                model: "m".into(),
                provider: "p".into(),
                kind: "completion".into(),
                input_tokens: None,
                cached_input_tokens: None,
                output_tokens: None,
                cost: None,
            },
        );
        crew.apply(
            "root",
            &Event::Activity {
                at_ms: 1,
                phase: ActivityPhase::WakeStarted,
            },
        );
        assert!(crew.entries.is_empty());
        assert_eq!(crew.activity.get("root"), Some(&ActivityPhase::WakeStarted));
    }

    #[test]
    fn an_edit_shows_its_counts_and_diff_lines() {
        let mut crew = crew();
        crew.apply(
            "child",
            &call(
                "1",
                "edit",
                json!({"path": "src/a.rs", "old": "keep\nold line\nend", "new": "keep\nnew line\nend"}),
            ),
        );
        let now = crew.now_edit.clone().expect("the edit is now");
        assert!(!now.finished);
        assert_eq!(now.tail, vec!["new line", "end"]);
        crew.apply(
            "child",
            &result(
                "1",
                "edit",
                json!({"replaced": true, "added": 1, "removed": 1}),
            ),
        );
        assert!(crew.now_edit.as_ref().unwrap().finished);
        assert_eq!(
            kinds(thread_entries(&crew, "child")),
            vec![&EntryKind::Edit {
                path: "src/a.rs".into(),
                added: 1,
                removed: 1,
                diff: vec![
                    DiffLine {
                        added: false,
                        text: "old line".into()
                    },
                    DiffLine {
                        added: true,
                        text: "new line".into()
                    },
                ],
            }]
        );
        assert!(
            chat_lines(&crew, 60).is_empty(),
            "a child's edit is not the chat's"
        );
    }

    #[test]
    fn edit_diff_keeps_at_most_six_changed_lines() {
        let old = (0..10)
            .map(|i| format!("o{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..10)
            .map(|i| format!("n{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = edit_diff(&old, &new);
        assert_eq!(diff.len(), MAX_DIFF_LINES);
        assert!(
            diff.iter().all(|line| !line.added),
            "removed lines come first"
        );
        assert_eq!(edit_diff("same", "same"), Vec::new());
    }

    #[test]
    fn the_files_list_sums_edits_writes_and_shell_changes_newest_first() {
        let mut crew = crew();
        crew.apply(
            "child",
            &call(
                "1",
                "file_write",
                json!({"path": "new.md", "content": "a\nb"}),
            ),
        );
        crew.apply(
            "child",
            &result(
                "1",
                "file_write",
                json!({"created": true, "added": 2, "removed": 0}),
            ),
        );
        crew.apply(
            "child",
            &call(
                "2",
                "edit",
                json!({"path": "new.md", "old": "a", "new": "c"}),
            ),
        );
        crew.apply(
            "child",
            &result(
                "2",
                "edit",
                json!({"replaced": true, "added": 1, "removed": 1}),
            ),
        );
        crew.apply(
            "root",
            &result(
                "3",
                "shell",
                json!({"stdout": "", "files": [
                    {"path": "gone.rs", "op": "deleted"},
                    {"path": "made.rs", "op": "created"}
                ]}),
            ),
        );
        let rows: Vec<_> = crew
            .files
            .iter()
            .map(|f| (f.path.clone(), f.op, f.lines, f.by.clone()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("made.rs".into(), FileOp::Created, None, "root".into()),
                ("gone.rs".into(), FileOp::Deleted, None, "root".into()),
                (
                    "new.md".into(),
                    FileOp::Created,
                    Some((3, 1)),
                    "child".into()
                ),
            ]
        );
        assert_eq!(crew.files[2].diff.len(), 2, "the edit's diff is kept");

        let lines = texts(&files_lines(&crew, Some(2), false, 80, 0));
        assert!(lines[0] == "NOW" && lines[1].starts_with("Bu Builder edited new.md"));
        let row = lines
            .iter()
            .find(|line| line.contains("new.md") && line.contains("C  "))
            .expect("the file row");
        assert!(row.starts_with("› C  new.md"), "{row}");
        assert!(row.contains("+3 −1") && row.contains("Builder"), "{row}");

        let detail = texts(&files_lines(&crew, Some(2), true, 80, 0));
        assert!(detail[0].starts_with("new.md"));
        assert_eq!(&detail[1..], ["   − a", "   + c"]);
    }

    #[test]
    fn a_failed_edit_finishes_but_changes_nothing() {
        let mut crew = crew();
        crew.apply(
            "child",
            &call("1", "edit", json!({"path": "a", "old": "x", "new": "y"})),
        );
        crew.apply(
            "child",
            &message(
                Role::User,
                Block::ToolResult {
                    id: "1".into(),
                    name: "edit".into(),
                    is_error: true,
                    content: json!({"error": "no match"}),
                },
            ),
        );
        assert!(crew.files.is_empty());
        assert!(crew.entries.is_empty());
        assert!(crew.now_edit.unwrap().finished);
    }

    #[test]
    fn tasks_group_by_status_with_the_owners_tag_and_state() {
        let mut crew = crew();
        crew.apply(
            "child",
            &Event::Todos {
                at_ms: None,
                items: vec![json!({"id": "x", "content": "child list", "status": "todo"})],
            },
        );
        assert!(crew.todos.is_empty(), "only the root's list is the crew's");
        crew.apply(
            "root",
            &Event::Todos {
                at_ms: None,
                items: vec![
                    json!({"id": "1", "content": "write docs", "status": "done", "kind": "other"}),
                    json!({"id": "2", "content": "build parser", "status": "in_progress", "kind": "build", "owner": "child"}),
                    json!({"id": "3", "content": "test it", "status": "todo"}),
                    json!({"id": "4", "status": "todo"}),
                ],
            },
        );
        let groups: Vec<(&str, Vec<&str>)> = task_groups(&crew.todos)
            .into_iter()
            .map(|(heading, items)| (heading, items.iter().map(|i| i.content.as_str()).collect()))
            .collect();
        assert_eq!(
            groups,
            vec![
                ("IN PROGRESS", vec!["build parser"]),
                ("TO DO", vec!["test it"]),
                ("DONE", vec!["write docs"]),
            ]
        );
        let lines = texts(&tasks_lines(&crew, 60));
        assert!(lines[0].ends_with("1 of 3 done"), "{}", lines[0]);
        let row = lines
            .iter()
            .find(|line| line.contains("build parser"))
            .unwrap();
        assert!(row.starts_with("  build    build parser"), "{row}");
        assert!(row.ends_with("Bu working"), "{row}");
    }

    #[test]
    fn task_bar_splits_done_in_progress_and_the_rest() {
        let bar = task_bar(3, 1, 7, 14);
        let cells: Vec<(usize, Option<Color>)> = bar
            .iter()
            .map(|span| (span.content.chars().count(), span.style.fg))
            .collect();
        assert_eq!(
            cells,
            vec![(6, Some(ADDED)), (2, Some(ACCENT)), (6, Some(MUTED))]
        );
        assert!(task_bar(0, 0, 0, 10).is_empty());
        let full: usize = task_bar(7, 0, 7, 10)
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        assert_eq!(full, 10);
    }

    #[test]
    fn the_crew_line_shows_each_member_with_its_state_and_caption() {
        let mut crew = crew();
        crew.members[0].state = SessionState::WaitingForInput;
        crew.apply(
            "child",
            &Event::Activity {
                at_ms: 1,
                phase: ActivityPhase::ToolStarted {
                    name: "edit".into(),
                    target: Some("src/winsw.ts".into()),
                },
            },
        );
        assert_eq!(
            line_text(&crew_line(&crew, 80)),
            "Ld waiting on 1  Bu working editing winsw.ts"
        );
        assert_eq!(line_text(&crew_line(&crew, 24)), "Ld waiting on 1  Bu wor…");
    }

    #[test]
    fn age_moves_from_seconds_to_days() {
        assert_eq!(age(59_000, 0), "59s");
        assert_eq!(age(60_000, 0), "1m");
        assert_eq!(age(7_200_000, 0), "2h");
        assert_eq!(age(3 * 86_400_000, 0), "3d");
        assert_eq!(age(0, 5_000), "0s", "a stamp in the future reads as now");
    }

    #[test]
    fn the_entries_are_capped_across_the_tree() {
        let mut crew = crew();
        for index in 0..MAX_CHAT_ENTRIES + 3 {
            let thread = if index % 2 == 0 { "root" } else { "child" };
            crew.apply(thread, &text(Role::User, &format!("m{index}")));
        }
        assert_eq!(crew.entries.len(), MAX_CHAT_ENTRIES);
    }
}
