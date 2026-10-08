//! The crew a session tree is drawn as in the terminal: each member's flag tag
//! and state word, and the chat, task list and file list built from the tree
//! stream. `bosun list` and `bosun open` both draw from here.

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

/// Chat entries kept in memory; the oldest drop when the cap is hit.
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
/// How far back a child's report looks for the post that already shows it.
const REPORT_LOOKBACK: usize = 200;
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
fn fnv1a(bytes: &[u8]) -> u32 {
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
            StateWord::Working => "working".into(),
            StateWord::WaitingOn(count) => format!("waiting on {count}").into(),
            StateWord::Done => "done".into(),
            StateWord::Idle => "idle".into(),
            StateWord::Stopped => "stopped".into(),
        }
    }

    pub fn colour(self) -> Color {
        match self {
            StateWord::NeedsYou => NEEDS_YOU,
            StateWord::Working | StateWord::WaitingOn(_) => WORKING,
            StateWord::Done | StateWord::Idle => MUTED,
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

/// Who a chat entry or a task is addressed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipient {
    You,
    /// The parent of the member that wrote the entry.
    Parent,
    Session(String),
    /// A persona a spawn starts, before the child has an id.
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

/// One entry of the crew chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEntry {
    Post {
        from: String,
        to: Recipient,
        text: String,
        at_ms: Option<u64>,
    },
    You {
        text: String,
        at_ms: Option<u64>,
    },
    /// The tool calls one member made between messages, counted by kind.
    Fold {
        from: String,
        items: Vec<(FoldItem, usize)>,
    },
    Edit {
        from: String,
        path: String,
        added: u64,
        removed: u64,
        diff: Vec<DiffLine>,
    },
    Ask {
        from: String,
        to: Recipient,
        message: String,
        options: Vec<String>,
        answer: Option<String>,
        at_ms: Option<u64>,
    },
    Failure {
        from: String,
        text: String,
    },
    /// The reader rejected the question on screen.
    Rejected,
}

impl ChatEntry {
    /// The member that wrote the entry; None for the reader's own entries.
    pub fn from(&self) -> Option<&str> {
        match self {
            ChatEntry::Post { from, .. }
            | ChatEntry::Fold { from, .. }
            | ChatEntry::Edit { from, .. }
            | ChatEntry::Ask { from, .. }
            | ChatEntry::Failure { from, .. } => Some(from),
            ChatEntry::You { .. } | ChatEntry::Rejected => None,
        }
    }
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

/// What the tree stream says about a crew: the chat, the task list, the files
/// changed and each member's newest activity. Members and their states come
/// from the session list, and the stream's state events keep them current.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crew {
    pub root_id: String,
    pub members: Vec<Member>,
    pub chat: Vec<ChatEntry>,
    /// The root's streaming text, until the durable text replaces it.
    pub chat_delta: Option<String>,
    pub todos: Vec<TodoItem>,
    /// Newest change first.
    pub files: Vec<FileChange>,
    pub now_edit: Option<NowEdit>,
    pub activity: HashMap<String, ActivityPhase>,
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
                self.apply_message(session_id, is_root, message, *at_ms);
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

    fn apply_message(
        &mut self,
        session_id: &str,
        is_root: bool,
        message: &Message,
        at_ms: Option<u64>,
    ) {
        let from = session_id.to_string();
        let to = if is_root {
            Recipient::You
        } else {
            Recipient::Parent
        };
        match (&message.role, &message.block) {
            // A child's user text is its parent's instructions or a routed
            // answer; the parent's call or the answered ask already shows it.
            (Role::User, Block::Text { .. }) if !is_root => {}
            (Role::User, Block::Text { text }) if text == USER_REJECTED_TEXT => {
                self.push(ChatEntry::Rejected);
            }
            (Role::User, Block::Text { text }) => {
                // The reader's answer to the root's open question shows under it.
                if let Some(ChatEntry::Ask { answer, from, .. }) = self
                    .chat
                    .iter_mut()
                    .rev()
                    .find(|entry| !matches!(entry, ChatEntry::Fold { .. }))
                    && answer.is_none()
                    && *from == self.root_id
                {
                    *answer = Some(text.clone());
                    return;
                }
                self.push(ChatEntry::You {
                    text: text.clone(),
                    at_ms,
                });
            }
            (Role::Assistant, Block::Text { text }) => {
                if is_root {
                    self.chat_delta = None;
                }
                if !text.trim().is_empty() {
                    self.push(ChatEntry::Post {
                        from,
                        to,
                        text: text.clone(),
                        at_ms,
                    });
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
                    answer,
                    ..
                },
            ) => {
                if let Some(answer) = answer {
                    // A routed answer appends the ask again with its answer.
                    let open = self.chat.iter_mut().rev().find_map(|entry| match entry {
                        ChatEntry::Ask {
                            from: asker,
                            message: asked,
                            answer: slot @ None,
                            ..
                        } if *asker == from && *asked == *message => Some(slot),
                        _ => None,
                    });
                    if let Some(slot) = open {
                        *slot = Some(answer.clone());
                        return;
                    }
                }
                self.push(ChatEntry::Ask {
                    from,
                    to,
                    message: message.clone(),
                    options: options.clone(),
                    answer: answer.clone(),
                    at_ms,
                });
            }
            (
                _,
                Block::ChildEvent {
                    child_id,
                    kind,
                    text,
                    ..
                },
            ) => match kind {
                ChildEventKind::Report => {
                    let posted = self.chat.iter().rev().take(REPORT_LOOKBACK).any(|entry| {
                        matches!(entry, ChatEntry::Post { from, text: posted, .. }
                            if from == child_id && posted.trim() == text.trim())
                    });
                    if !posted {
                        self.push(ChatEntry::Post {
                            from: child_id.clone(),
                            to: Recipient::Session(from),
                            text: text.clone(),
                            at_ms,
                        });
                    }
                }
                // The child's own ask block shows the question.
                ChildEventKind::Ask => {}
                ChildEventKind::Failure => self.push(ChatEntry::Failure {
                    from: child_id.clone(),
                    text: text.clone(),
                }),
            },
            _ => {}
        }
    }

    fn apply_call(
        &mut self,
        session_id: &str,
        id: &str,
        name: &str,
        args: &Value,
        at_ms: Option<u64>,
    ) {
        let from = session_id.to_string();
        let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
        match name {
            "spawn" => self.push(ChatEntry::Post {
                from,
                to: Recipient::Persona(text("persona")),
                text: text("instructions"),
                at_ms,
            }),
            "message_child" => self.push(ChatEntry::Post {
                from,
                to: Recipient::Session(text("id")),
                text: text("text"),
                at_ms,
            }),
            "edit" | "file_write" => {
                let key = call_key(session_id, id);
                let written = if name == "edit" {
                    text("new")
                } else {
                    text("content")
                };
                self.now_edit = Some(NowEdit {
                    by: from.clone(),
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
                        by: from,
                        name: name.to_string(),
                        args: args.clone(),
                    },
                );
            }
            "ask" => {}
            _ => self.fold(from, fold_item(name, args)),
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
                self.push(ChatEntry::Edit {
                    from: call.by.clone(),
                    path: path.clone(),
                    added,
                    removed,
                    diff: diff.clone(),
                });
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

    /// Counts a tool call into the member's folded line. Only the folded
    /// lines after the newest message are searched, so a message from anyone
    /// starts a new line.
    fn fold(&mut self, from: String, item: FoldItem) {
        for entry in self.chat.iter_mut().rev() {
            let ChatEntry::Fold {
                from: folder,
                items,
            } = entry
            else {
                break;
            };
            if *folder != from {
                continue;
            }
            if let Some((_, count)) = items.iter_mut().find(|(kind, _)| *kind == item) {
                *count += 1;
            } else if items.len() < MAX_FOLD_ITEMS {
                items.push((item, 1));
            }
            return;
        }
        self.push(ChatEntry::Fold {
            from,
            items: vec![(item, 1)],
        });
    }

    fn push(&mut self, entry: ChatEntry) {
        if self.chat.len() >= MAX_CHAT_ENTRIES {
            self.chat.remove(0);
        }
        self.chat.push(entry);
    }
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

fn recipient_name(crew: &Crew, from: &str, to: &Recipient) -> String {
    match to {
        Recipient::You => "You".into(),
        Recipient::Parent => crew
            .member(from)
            .and_then(|member| member.parent_id.as_deref())
            .map_or_else(|| "parent".into(), |parent| who(crew, parent).1),
        Recipient::Session(id) => who(crew, id).1,
        Recipient::Persona(persona) => display_name(persona),
    }
}

fn dim() -> Style {
    Style::default().fg(MUTED)
}

/// `<tag> <Name> → <To> · HH:MM`.
fn head_line(
    crew: &Crew,
    from: &str,
    to: &Recipient,
    at_ms: Option<u64>,
    colour: Option<Color>,
) -> TuiLine<'static> {
    let (persona, name) = who(crew, from);
    let name_style = colour.map_or_else(
        || Style::default().add_modifier(Modifier::BOLD),
        |colour| Style::default().fg(colour).add_modifier(Modifier::BOLD),
    );
    let mut spans = vec![
        tag_span(&persona),
        Span::raw(" "),
        Span::styled(name, name_style),
        Span::styled(format!(" → {}", recipient_name(crew, from, to)), dim()),
    ];
    if let Some(time) = at_ms.and_then(clock) {
        spans.push(Span::styled(format!(" · {time}"), dim()));
    }
    TuiLine::from(spans)
}

fn you_head(at_ms: Option<u64>) -> TuiLine<'static> {
    let mut spans = vec![Span::styled(
        "You",
        Style::default().fg(BRASS).add_modifier(Modifier::BOLD),
    )];
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

/// The Chat view: every entry, or only `filter`'s when a member is picked,
/// wrapped to `width`, with the root's streaming text last.
pub fn chat_lines(crew: &Crew, filter: Option<&str>, width: usize) -> Vec<TuiLine<'static>> {
    let mut lines = Vec::new();
    let text_style = Style::default();
    for entry in &crew.chat {
        if let Some(filter) = filter
            && entry.from() != Some(filter)
        {
            continue;
        }
        match entry {
            ChatEntry::Post {
                from,
                to,
                text,
                at_ms,
            } => {
                lines.push(TuiLine::default());
                lines.push(head_line(crew, from, to, *at_ms, None));
                lines.extend(body_lines(text, width, text_style));
            }
            ChatEntry::You { text, at_ms } => {
                lines.push(TuiLine::default());
                lines.push(you_head(*at_ms));
                lines.extend(body_lines(text, width, text_style));
            }
            ChatEntry::Fold { from, items } => {
                let (persona, _) = who(crew, from);
                let text = clip(&fold_text(items), width.saturating_sub(3).max(1));
                lines.push(TuiLine::from(vec![
                    tag_span(&persona),
                    Span::raw(" "),
                    Span::styled(text, dim()),
                ]));
            }
            ChatEntry::Edit {
                from,
                path,
                added,
                removed,
                diff,
            } => {
                let (persona, _) = who(crew, from);
                let mut spans = vec![
                    tag_span(&persona),
                    Span::raw(" "),
                    Span::styled(format!("✎ {path}  "), Style::default().fg(ACCENT)),
                ];
                spans.extend(counts_spans(*added, *removed));
                lines.push(TuiLine::from(spans));
                lines.extend(diff_lines(diff, width));
            }
            ChatEntry::Ask {
                from,
                to,
                message,
                options,
                answer,
                at_ms,
            } => {
                let amber = Style::default().fg(NEEDS_YOU);
                lines.push(TuiLine::default());
                lines.push(head_line(crew, from, to, *at_ms, Some(NEEDS_YOU)));
                lines.extend(body_lines(&format!("? {message}"), width, amber));
                if !options.is_empty() {
                    let choices = options
                        .iter()
                        .map(|option| format!("[{option}]"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    lines.extend(body_lines(&choices, width, amber));
                }
                if let Some(answer) = answer {
                    lines.push(you_head(None));
                    lines.extend(body_lines(answer, width, text_style));
                }
            }
            ChatEntry::Failure { from, text } => {
                let (persona, name) = who(crew, from);
                let red = Style::default().fg(STOPPED);
                lines.push(TuiLine::from(vec![
                    tag_span(&persona),
                    Span::raw(" "),
                    Span::styled(format!("{name} failed"), red.add_modifier(Modifier::BOLD)),
                ]));
                lines.extend(body_lines(text, width, red));
            }
            ChatEntry::Rejected => {
                lines.push(TuiLine::from(Span::styled(
                    "  ~ you rejected the question",
                    dim(),
                )));
            }
        }
    }
    if let Some(delta) = &crew.chat_delta
        && filter.is_none_or(|filter| filter == crew.root_id)
    {
        lines.push(TuiLine::default());
        lines.push(head_line(crew, &crew.root_id, &Recipient::You, None, None));
        lines.extend(body_lines(delta, width, text_style));
    }
    if lines.first().is_some_and(|line| line.spans.is_empty()) {
        lines.remove(0);
    }
    lines
}

/// A member's tag, state word and caption, as the crew line and the Tasks
/// view show them.
fn member_spans(crew: &Crew, member: &Member, caption: bool) -> Vec<Span<'static>> {
    let word = state_word(member);
    let mut spans = vec![
        tag_span(&member.persona),
        Span::raw(" "),
        Span::styled(word.text().into_owned(), Style::default().fg(word.colour())),
    ];
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

    #[test]
    fn a_childs_user_text_is_never_in_the_chat() {
        let mut crew = crew();
        crew.apply("child", &text(Role::User, "build the parser"));
        crew.apply("root", &text(Role::User, "please start"));
        assert_eq!(
            crew.chat,
            vec![ChatEntry::You {
                text: "please start".into(),
                at_ms: None,
            }]
        );
    }

    #[test]
    fn a_spawn_and_a_message_to_a_child_are_directed_posts() {
        let mut crew = crew();
        crew.apply(
            "root",
            &call(
                "1",
                "spawn",
                json!({"persona": "builder", "instructions": "build it"}),
            ),
        );
        crew.apply(
            "root",
            &call(
                "2",
                "message_child",
                json!({"id": "child", "text": "status?"}),
            ),
        );
        crew.apply("child", &text(Role::Assistant, "on it"));
        let lines = texts(&chat_lines(&crew, None, 60));
        assert_eq!(
            lines,
            vec![
                "Ld Lead → Builder",
                "   build it",
                "",
                "Ld Lead → Builder",
                "   status?",
                "",
                "Bu Builder → Lead",
                "   on it",
            ]
        );
    }

    #[test]
    fn a_report_already_posted_by_the_child_is_not_shown_again() {
        let report = |text: &str| {
            message(
                Role::Assistant,
                Block::ChildEvent {
                    child_id: "child".into(),
                    kind: ChildEventKind::Report,
                    text: text.into(),
                    origin: None,
                },
            )
        };
        let mut crew = crew();
        crew.apply("child", &text(Role::Assistant, "parser done"));
        crew.apply("root", &report("parser done"));
        assert_eq!(crew.chat.len(), 1, "the child's own post shows the report");

        // A report whose text the stream never carried is the child's post.
        crew.apply("root", &report("tests pass"));
        assert_eq!(
            crew.chat.last(),
            Some(&ChatEntry::Post {
                from: "child".into(),
                to: Recipient::Session("root".into()),
                text: "tests pass".into(),
                at_ms: None,
            })
        );

        crew.apply(
            "root",
            &message(
                Role::Assistant,
                Block::ChildEvent {
                    child_id: "child".into(),
                    kind: ChildEventKind::Failure,
                    text: "crashed".into(),
                    origin: None,
                },
            ),
        );
        assert_eq!(
            crew.chat.last(),
            Some(&ChatEntry::Failure {
                from: "child".into(),
                text: "crashed".into(),
            })
        );
    }

    #[test]
    fn tool_calls_between_messages_fold_into_one_line_per_member() {
        let mut crew = crew();
        for id in ["1", "2", "3", "4"] {
            crew.apply("child", &call(id, "file_read", json!({"path": "a"})));
        }
        crew.apply("root", &call("5", "grep", json!({"pattern": "x"})));
        crew.apply(
            "child",
            &call("6", "shell", json!({"command": "npm test\nnpm run lint"})),
        );
        assert_eq!(crew.chat.len(), 2, "one folded line per member");
        let lines = texts(&chat_lines(&crew, None, 80));
        assert_eq!(
            lines,
            vec!["Bu read 4 files · ran npm test once", "Ld searched once"]
        );

        // A message starts a new folded line.
        crew.apply("child", &text(Role::Assistant, "read them"));
        crew.apply("child", &call("7", "file_read", json!({"path": "b"})));
        assert_eq!(crew.chat.len(), 4);
    }

    #[test]
    fn asks_show_their_options_and_the_answer_under_you() {
        let mut crew = crew();
        let ask = |answer: Option<&str>| {
            message(
                Role::Assistant,
                Block::Ask {
                    message: "push?".into(),
                    options: vec!["yes".into(), "no".into()],
                    child_id: None,
                    answer: answer.map(str::to_string),
                },
            )
        };
        crew.apply("root", &ask(None));
        assert!(crew.members[0].asking, "an open ask marks the member");
        assert_eq!(
            texts(&chat_lines(&crew, None, 60)),
            vec!["Ld Lead → You", "   ? push?", "   [yes] [no]"]
        );
        crew.apply("root", &text(Role::User, "yes"));
        assert!(!crew.members[0].asking);
        assert_eq!(crew.chat.len(), 1, "the answer joins the ask");
        assert_eq!(texts(&chat_lines(&crew, None, 60))[3..], ["You", "   yes"]);

        // A routed answer appends the ask again; it fills the open one.
        let mut crew = self::crew();
        crew.apply("child", &ask(None));
        crew.apply("child", &ask(Some("no")));
        assert_eq!(crew.chat.len(), 1);
        assert!(matches!(
            &crew.chat[0],
            ChatEntry::Ask { answer: Some(answer), to: Recipient::Parent, .. } if answer == "no"
        ));
    }

    #[test]
    fn reasoning_and_context_rows_stay_out_of_the_chat() {
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
        assert!(crew.chat.is_empty());
        assert_eq!(crew.activity.get("root"), Some(&ActivityPhase::WakeStarted));
    }

    #[test]
    fn the_filter_shows_only_one_members_posts() {
        let mut crew = crew();
        crew.apply("root", &text(Role::Assistant, "lead says"));
        crew.apply("child", &text(Role::Assistant, "builder says"));
        let lines = texts(&chat_lines(&crew, Some("child"), 60));
        assert_eq!(lines, vec!["Bu Builder → Lead", "   builder says"]);
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
            texts(&chat_lines(&crew, None, 60)),
            vec!["Bu ✎ src/a.rs  +1 −1", "   − old line", "   + new line"]
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
        assert!(crew.chat.is_empty());
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
    fn the_chat_list_is_capped() {
        let mut crew = crew();
        for index in 0..MAX_CHAT_ENTRIES + 3 {
            crew.apply("root", &text(Role::User, &format!("m{index}")));
        }
        assert_eq!(crew.chat.len(), MAX_CHAT_ENTRIES);
    }
}
