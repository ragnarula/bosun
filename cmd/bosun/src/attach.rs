//! Interactive terminal client for a session: renders the live transcript
//! over SSE and sends the user's input back over the session API.

use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use bosun_agent::sse::SseError;
use bosun_agent::sse::SseEvent;
use bosun_agent::sse::sse_stream;
use bosun_common::session::Block;
use bosun_common::session::Event;
use bosun_common::session::Permission;
use bosun_common::session::Role;
use bosun_common::session::Session;
use crossterm::cursor;
use crossterm::event;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::Event as TermEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::execute;
use crossterm::terminal;
use crossterm::terminal::ClearType;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use futures_util::Stream;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::state_name;

/// How long to wait before reconnecting after the event stream ends.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How long a tool call's inline args or result may render before it is cut.
const MAX_INLINE_CHARS: usize = 1000;
/// Transcript rows kept in memory; the oldest rows scroll away.
const MAX_LINES: usize = 5000;
/// How long one client POST may take before the terminal gives up on it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum characters the input line may hold.
const MAX_INPUT_CHARS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Ask,
    Summary,
    Subagent,
    ModelCall,
    Status,
}

/// One row of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

/// The client's view of a session: the durable transcript, the input line,
/// and the stream cursor for reconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientState {
    pub lines: Vec<Line>,
    pub input: String,
    pub last_seq: i64,
    pub pending_delta: Option<String>,
    pub permission: Permission,
}

impl ClientState {
    pub fn new(permission: Permission) -> Self {
        Self {
            lines: Vec::new(),
            input: String::new(),
            last_seq: 0,
            pending_delta: None,
            permission,
        }
    }

    /// Applies one durable event from the stream. Events at or below the last
    /// applied seq are a replay and are skipped. Returns true when the event
    /// was applied.
    pub fn apply_event(&mut self, seq: i64, event: &Event) -> bool {
        if seq <= self.last_seq {
            return false;
        }
        self.last_seq = seq;
        if let Event::Permission { permission } = event {
            self.permission = *permission;
        }
        if matches!(
            event,
            Event::Message { message } if matches!(message.block, Block::Text { .. })
        ) {
            // The durable text is the turn's final text; it supersedes the
            // streaming delta that led up to it.
            self.pending_delta = None;
        }
        if matches!(event, Event::State { .. }) {
            // A state change marks the end of one turn and the start of the
            // next, so any streaming text from the prior turn is stale.
            self.pending_delta = None;
        }
        for line in event_lines(event) {
            self.push_line(line);
        }
        true
    }

    /// Appends a live text delta to the pending line.
    pub fn apply_delta(&mut self, text: &str) {
        match &mut self.pending_delta {
            Some(pending) => pending.push_str(text),
            None => self.pending_delta = Some(text.to_string()),
        }
    }

    fn push_line(&mut self, line: Line) {
        if self.lines.len() >= MAX_LINES {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }
}

/// Maps a durable event to the transcript lines it contributes.
fn event_lines(event: &Event) -> Vec<Line> {
    match event {
        Event::Message { message } => {
            let kind = match message.role {
                Role::User => LineKind::User,
                Role::Assistant => LineKind::Assistant,
            };
            match &message.block {
                Block::Text { text } => vec![Line {
                    kind,
                    text: text.clone(),
                }],
                Block::ToolCall { name, args, .. } => vec![Line {
                    kind: LineKind::ToolCall,
                    text: format!("{name} {}", inline_value(args)),
                }],
                Block::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let prefix = if *is_error { "error: " } else { "" };
                    vec![Line {
                        kind: LineKind::ToolResult,
                        text: format!("{prefix}{name} {}", inline_value(content)),
                    }]
                }
                Block::Ask {
                    message, options, ..
                } => vec![Line {
                    kind: LineKind::Ask,
                    text: format!("{message} [{}]", options.join(", ")),
                }],
                Block::Summary { text } => vec![Line {
                    kind: LineKind::Summary,
                    text: text.clone(),
                }],
                Block::Subagent {
                    subagent_type,
                    status,
                    text,
                } => vec![Line {
                    kind: LineKind::Subagent,
                    text: format!("{subagent_type} {status}: {text}"),
                }],
            }
        }
        Event::State { state } => vec![Line {
            kind: LineKind::Status,
            text: format!("state: {}", state_name(*state)),
        }],
        Event::Permission { permission } => vec![Line {
            kind: LineKind::Status,
            text: format!("permission: {}", permission_name(*permission)),
        }],
        Event::ModelCall {
            model,
            kind,
            input_tokens,
            output_tokens,
            cost,
            ..
        } => {
            let mut detail = Vec::new();
            if let Some(input) = input_tokens {
                detail.push(format!("{input} in"));
            }
            if let Some(output) = output_tokens {
                detail.push(format!("{output} out"));
            }
            if let Some(cost) = cost {
                detail.push(format!("${cost:.4}"));
            }
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", detail.join(", "))
            };
            vec![Line {
                kind: LineKind::ModelCall,
                text: format!("{model} {kind}{detail}"),
            }]
        }
    }
}

fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::ReadOnly => "read_only",
        Permission::ReadWrite => "read_write",
    }
}

/// Renders a tool-call arg or result value on one line, cutting long text.
fn inline_value(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    clip(&text, MAX_INLINE_CHARS)
}

fn clip(text: &str, max: usize) -> String {
    let mut clipped: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        clipped.push('…');
    }
    clipped
}

/// Renders the transcript tail, the pending delta, and the input line into a
/// screen of `height` rows and `width` columns. The last row is the input.
fn render(state: &ClientState, width: usize, height: usize) -> String {
    let width = width.max(1);
    let height = height.max(1);
    let mut rows: Vec<String> = Vec::new();
    for line in &state.lines {
        rows.extend(wrap(&render_line(line), width));
    }
    if let Some(pending) = &state.pending_delta {
        rows.extend(wrap(pending, width));
    }
    let input_row = format!("> {}", input_tail(&state.input, width));
    let keep = height.saturating_sub(1).max(1);
    let scroll = rows.len().saturating_sub(keep);
    let mut screen: Vec<String> = rows.into_iter().skip(scroll).collect();
    screen.push(input_row);
    screen.join("\n")
}

fn render_line(line: &Line) -> String {
    match line.kind {
        LineKind::User => format!("you: {}", line.text),
        LineKind::Assistant => line.text.clone(),
        LineKind::ToolCall => format!("tool call: {}", line.text),
        LineKind::ToolResult => format!("tool result: {}", line.text),
        LineKind::Ask => format!("ask: {}", line.text),
        LineKind::Summary => format!("summary: {}", line.text),
        LineKind::Subagent => format!("subagent: {}", line.text),
        LineKind::ModelCall => format!("model: {}", line.text),
        LineKind::Status => format!("* {}", line.text),
    }
}

/// Word-wraps text to `width` columns, splitting on newlines first. A word
/// longer than the width is hard-wrapped at character width.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        for word in paragraph.split_whitespace() {
            if word.chars().count() > width {
                if !row.is_empty() {
                    rows.push(row);
                    row = String::new();
                }
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    if chunk.len() == width {
                        rows.push(chunk.iter().collect());
                    } else {
                        row = chunk.iter().collect();
                    }
                }
                continue;
            }
            if row.is_empty() {
                row.push_str(word);
            } else if row.chars().count() + 1 + word.chars().count() <= width {
                row.push(' ');
                row.push_str(word);
            } else {
                rows.push(row);
                row = word.to_string();
            }
        }
        if !row.is_empty() {
            rows.push(row);
        } else if text.contains('\n') {
            rows.push(String::new());
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// The visible tail of the input line, so a long input keeps its end on
/// screen where the user is typing.
fn input_tail(input: &str, width: usize) -> String {
    let keep = width.saturating_sub(2);
    let chars: Vec<char> = input.chars().collect();
    if chars.len() > keep {
        chars[chars.len() - keep..].iter().collect()
    } else {
        input.to_string()
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SseFrame {
    Live { delta: String },
    Durable { seq: i64, event: Event },
}

/// Applies one SSE event to the state: a live delta appends to the pending
/// line, a durable event is replayed through `apply_event`.
fn apply_sse(state: &mut ClientState, sse: &SseEvent) {
    let Ok(frame) = serde_json::from_str::<SseFrame>(&sse.data) else {
        return;
    };
    match frame {
        SseFrame::Live { delta } => state.apply_delta(&delta),
        SseFrame::Durable { seq, event } => {
            state.apply_event(seq, &event);
        }
    }
}

/// Attaches to a session: renders its live transcript and forwards the user's
/// input, interrupt, and permission changes until the user exits.
pub async fn attach(cp_url: &str, session_id: &str) -> anyhow::Result<()> {
    let client = crate::cp_client()?;
    let session: Session = client
        .get(format!("{cp_url}/sessions/{session_id}"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?
        .error_for_status()
        .with_context(|| format!("session {session_id} is not available"))?
        .json()
        .await
        .context("failed to parse session")?;

    let quit = Arc::new(AtomicBool::new(false));
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    spawn_input_thread(input_tx, Arc::clone(&quit))?;

    terminal::enable_raw_mode()?;
    let _restore = RestoreTerminal;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    )?;

    let result = run_attach(
        &client,
        cp_url,
        session_id,
        ClientState::new(session.permission),
        input_rx,
    )
    .await;
    quit.store(true, Ordering::Relaxed);
    result
}

/// Restores the terminal when attach exits, including on error or panic.
struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            cursor::Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}

/// Reads terminal events on a background thread. Polls with a timeout so the
/// thread also exits when the quit flag is set and the terminal is restored.
fn spawn_input_thread(
    tx: mpsc::UnboundedSender<TermEvent>,
    quit: Arc<AtomicBool>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("bosun-input".to_string())
        .spawn(move || {
            while !quit.load(Ordering::Relaxed) {
                if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                    match event::read() {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        })
        .map(|_| ())
}

/// Appends text to the input line up to the cap. Reports when the cap
/// truncates the text.
fn push_input(state: &mut ClientState, text: &str) {
    let room = MAX_INPUT_CHARS.saturating_sub(state.input.chars().count());
    let kept: String = text.chars().take(room).collect();
    state.input.push_str(&kept);
    if kept.chars().count() < text.chars().count() {
        state.push_line(Line {
            kind: LineKind::Status,
            text: "input limit reached".into(),
        });
    }
}

async fn run_attach(
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    mut state: ClientState,
    mut input_rx: mpsc::UnboundedReceiver<TermEvent>,
) -> anyhow::Result<()> {
    draw(&state)?;
    let mut noticed = false;
    loop {
        if let Ok(stream) = open_stream(client, cp_url, session_id, state.last_seq).await {
            // The stream restarted: live deltas do not survive a reconnect,
            // and the outage is over, so the next drop reports again.
            state.pending_delta = None;
            noticed = false;
            draw(&state)?;
            let outcome = stream_events(
                &mut state,
                &mut input_rx,
                client,
                cp_url,
                session_id,
                stream,
            )
            .await?;
            if outcome == StreamOutcome::Exited {
                return Ok(());
            }
            if !noticed {
                noticed = true;
                state.push_line(Line {
                    kind: LineKind::Status,
                    text: "connection lost; reconnecting".to_string(),
                });
                draw(&state)?;
            }
        } else if !noticed {
            noticed = true;
            state.push_line(Line {
                kind: LineKind::Status,
                text: "connection lost; reconnecting".to_string(),
            });
            draw(&state)?;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn open_stream(
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    after: i64,
) -> anyhow::Result<impl Stream<Item = Result<SseEvent, SseError>>> {
    let response = client
        .get(format!("{cp_url}/sessions/{session_id}/events"))
        .query(&[("after", after)])
        .send()
        .await
        .context("failed to reach the control plane")?
        .error_for_status()
        .context("the control plane returned an error")?;
    Ok(sse_stream(response.bytes_stream()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamOutcome {
    Exited,
    Reconnect,
}

async fn stream_events(
    state: &mut ClientState,
    input_rx: &mut mpsc::UnboundedReceiver<TermEvent>,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    stream: impl Stream<Item = Result<SseEvent, SseError>>,
) -> anyhow::Result<StreamOutcome> {
    tokio::pin!(stream);
    loop {
        tokio::select! {
            event = input_rx.recv() => {
                let Some(event) = event else {
                    return Ok(StreamOutcome::Exited);
                };
                let action = handle_term_event(state, client, cp_url, session_id, event).await?;
                if action == Action::Exit {
                    return Ok(StreamOutcome::Exited);
                }
                draw(state)?;
            }
            item = stream.next() => match item {
                Some(Ok(sse)) => {
                    apply_sse(state, &sse);
                    draw(state)?;
                }
                Some(Err(_)) | None => return Ok(StreamOutcome::Reconnect),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Continue,
    Exit,
}

async fn handle_term_event(
    state: &mut ClientState,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    event: TermEvent,
) -> anyhow::Result<Action> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(state, client, cp_url, session_id, key).await
        }
        TermEvent::Paste(text) => {
            push_input(state, &text);
            Ok(Action::Continue)
        }
        _ => Ok(Action::Continue),
    }
}

async fn handle_key(
    state: &mut ClientState,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    key: KeyEvent,
) -> anyhow::Result<Action> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            interrupt(state, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_permission(state, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => Ok(Action::Exit),
        KeyCode::Esc => Ok(Action::Exit),
        KeyCode::Enter => submit_input(state, client, cp_url, session_id).await,
        KeyCode::Backspace | KeyCode::Delete => {
            state.input.pop();
            Ok(Action::Continue)
        }
        KeyCode::Char(c) => {
            push_input(state, &c.to_string());
            Ok(Action::Continue)
        }
        _ => Ok(Action::Continue),
    }
}

async fn submit_input(
    state: &mut ClientState,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
) -> anyhow::Result<Action> {
    let content = std::mem::take(&mut state.input);
    match content.as_str() {
        "/exit" => Ok(Action::Exit),
        "/permission" => {
            toggle_permission(state, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        _ => {
            if !content.is_empty() {
                send_message(state, client, cp_url, session_id, &content).await;
            }
            Ok(Action::Continue)
        }
    }
}

async fn send_message(
    state: &mut ClientState,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    content: &str,
) {
    let send = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .post(format!("{cp_url}/sessions/{session_id}/messages"))
            .json(&serde_json::json!({ "content": content }))
            .send(),
    )
    .await;
    let result = match send {
        Ok(result) => result.and_then(reqwest::Response::error_for_status),
        Err(_) => {
            state.push_line(Line {
                kind: LineKind::Status,
                text: "request timed out".into(),
            });
            return;
        }
    };
    if let Err(error) = result {
        state.push_line(Line {
            kind: LineKind::Status,
            text: format!("send failed: {error}"),
        });
    }
}

async fn interrupt(
    state: &mut ClientState,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
) {
    let send = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .post(format!("{cp_url}/sessions/{session_id}/interrupt"))
            .send(),
    )
    .await;
    let result = match send {
        Ok(result) => result.and_then(reqwest::Response::error_for_status),
        Err(_) => {
            state.push_line(Line {
                kind: LineKind::Status,
                text: "request timed out".into(),
            });
            return;
        }
    };
    if let Err(error) = result {
        state.push_line(Line {
            kind: LineKind::Status,
            text: format!("interrupt failed: {error}"),
        });
    }
}

async fn toggle_permission(
    state: &mut ClientState,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
) {
    let next = match state.permission {
        Permission::ReadOnly => Permission::ReadWrite,
        Permission::ReadWrite => Permission::ReadOnly,
    };
    let send = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .post(format!("{cp_url}/sessions/{session_id}/permission"))
            .json(&serde_json::json!({ "permission": next }))
            .send(),
    )
    .await;
    let result = match send {
        Ok(result) => result.and_then(reqwest::Response::error_for_status),
        Err(_) => {
            state.push_line(Line {
                kind: LineKind::Status,
                text: "request timed out".into(),
            });
            return;
        }
    };
    match result {
        Ok(_) => state.permission = next,
        Err(error) => state.push_line(Line {
            kind: LineKind::Status,
            text: format!("permission change failed: {error}"),
        }),
    }
}

/// Redraws the screen from the state.
fn draw(state: &ClientState) -> io::Result<()> {
    let (width, height) = terminal::size()?;
    let screen = render(state, width as usize, height as usize);
    let mut stdout = io::stdout();
    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All)
    )?;
    write!(stdout, "{screen}")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bosun_common::session::Message;
    use bosun_common::session::SessionState;
    use serde_json::json;

    use super::*;

    #[test]
    fn render_shows_the_transcript_tail_then_input() {
        let mut state = ClientState::new(Permission::ReadWrite);
        state.lines.push(Line {
            kind: LineKind::User,
            text: "hello".into(),
        });
        state.lines.push(Line {
            kind: LineKind::Assistant,
            text: "hi there".into(),
        });
        state.pending_delta = Some("streaming".into());
        state.input = "next".into();
        assert_eq!(
            render(&state, 40, 10),
            "you: hello\nhi there\nstreaming\n> next"
        );
    }

    #[test]
    fn render_scrolls_to_the_transcript_tail() {
        let mut state = ClientState::new(Permission::ReadWrite);
        for i in 0..5 {
            state.push_line(Line {
                kind: LineKind::Assistant,
                text: format!("line {i}"),
            });
        }
        assert_eq!(render(&state, 40, 3), "line 3\nline 4\n> ");
    }

    #[test]
    fn render_wraps_long_rows_to_the_width() {
        let mut state = ClientState::new(Permission::ReadWrite);
        state.push_line(Line {
            kind: LineKind::Assistant,
            text: "one two three four".into(),
        });
        assert_eq!(render(&state, 10, 5), "one two\nthree four\n> ");
    }

    #[test]
    fn wrap_hard_breaks_words_longer_than_the_width() {
        assert_eq!(wrap("abcdefghij", 5), vec!["abcde", "fghij"]);
        assert_eq!(wrap("abcdefghijk", 5), vec!["abcde", "fghij", "k"]);
        assert_eq!(
            wrap("one abcdefghij two", 5),
            vec!["one", "abcde", "fghij", "two"]
        );
        assert_eq!(wrap("ab", 5), vec!["ab"]);
    }

    #[test]
    fn input_is_capped_at_max_input_chars() {
        let mut state = ClientState::new(Permission::ReadWrite);
        state.input = "x".repeat(MAX_INPUT_CHARS);
        push_input(&mut state, "y");
        assert_eq!(state.input.chars().count(), MAX_INPUT_CHARS);
        assert_eq!(
            state.lines.last(),
            Some(&Line {
                kind: LineKind::Status,
                text: "input limit reached".into(),
            })
        );
    }

    #[test]
    fn paste_is_trimmed_to_fit_the_input_cap() {
        let mut state = ClientState::new(Permission::ReadWrite);
        state.input = "x".repeat(MAX_INPUT_CHARS - 3);
        push_input(&mut state, "abcdef");
        assert_eq!(state.input, "x".repeat(MAX_INPUT_CHARS - 3) + "abc");
        assert_eq!(
            state.lines.last(),
            Some(&Line {
                kind: LineKind::Status,
                text: "input limit reached".into(),
            })
        );
    }

    #[test]
    fn durable_text_supersedes_the_pending_delta() {
        let mut state = ClientState::new(Permission::ReadWrite);
        state.apply_delta("building");
        state.apply_delta(" the crate");
        assert_eq!(state.pending_delta.as_deref(), Some("building the crate"));

        let event = Event::Message {
            message: Message {
                role: Role::Assistant,
                block: Block::Text {
                    text: "building the crate".into(),
                },
            },
        };
        assert!(state.apply_event(1, &event));
        assert_eq!(state.pending_delta, None);
        assert_eq!(
            state.lines,
            vec![Line {
                kind: LineKind::Assistant,
                text: "building the crate".into(),
            }]
        );
    }

    #[test]
    fn state_events_clear_the_pending_delta_at_turn_boundaries() {
        let mut state = ClientState::new(Permission::ReadWrite);

        // An interrupted turn never commits text, so its delta must not
        // linger on screen.
        state.apply_delta("phantom text");
        let interrupted = Event::State {
            state: SessionState::Interrupted,
        };
        assert!(state.apply_event(1, &interrupted));
        assert_eq!(state.pending_delta, None);

        // Deltas from the prior turn are cleared before the next turn's
        // deltas append.
        state.apply_delta("stale text");
        let running = Event::State {
            state: SessionState::Running,
        };
        assert!(state.apply_event(2, &running));
        assert_eq!(state.pending_delta, None);

        state.apply_delta("fresh text");
        assert_eq!(state.pending_delta.as_deref(), Some("fresh text"));
    }

    #[test]
    fn apply_sse_parses_live_and_durable_frames() {
        let mut state = ClientState::new(Permission::ReadWrite);
        apply_sse(
            &mut state,
            &SseEvent {
                event: None,
                data: r#"{"delta":"hel"}"#.into(),
            },
        );
        apply_sse(
            &mut state,
            &SseEvent {
                event: None,
                data: r#"{"delta":"lo"}"#.into(),
            },
        );
        assert_eq!(state.pending_delta.as_deref(), Some("hello"));

        apply_sse(
            &mut state,
            &SseEvent {
                event: None,
                data: r#"{"seq":1,"event":{"kind":"message","message":{"role":"assistant","block":{"kind":"text","text":"hello"}}}}"#
                    .into(),
            },
        );
        assert_eq!(state.pending_delta, None);
        assert_eq!(state.last_seq, 1);
        assert_eq!(
            state.lines,
            vec![Line {
                kind: LineKind::Assistant,
                text: "hello".into(),
            }]
        );
    }

    #[test]
    fn event_lines_maps_each_durable_event_kind() {
        let text = Event::Message {
            message: Message {
                role: Role::User,
                block: Block::Text { text: "go".into() },
            },
        };
        assert_eq!(
            event_lines(&text),
            vec![Line {
                kind: LineKind::User,
                text: "go".into(),
            }]
        );

        let call = Event::Message {
            message: Message {
                role: Role::Assistant,
                block: Block::ToolCall {
                    id: "1".into(),
                    name: "shell".into(),
                    args: json!({"cmd": "ls"}),
                },
            },
        };
        assert_eq!(
            event_lines(&call),
            vec![Line {
                kind: LineKind::ToolCall,
                text: "shell {\"cmd\":\"ls\"}".into(),
            }]
        );

        let result = Event::Message {
            message: Message {
                role: Role::User,
                block: Block::ToolResult {
                    id: "1".into(),
                    name: "shell".into(),
                    is_error: false,
                    content: json!("ok"),
                },
            },
        };
        assert_eq!(
            event_lines(&result),
            vec![Line {
                kind: LineKind::ToolResult,
                text: "shell ok".into(),
            }]
        );

        let error = Event::Message {
            message: Message {
                role: Role::User,
                block: Block::ToolResult {
                    id: "1".into(),
                    name: "shell".into(),
                    is_error: true,
                    content: json!("boom"),
                },
            },
        };
        assert_eq!(
            event_lines(&error),
            vec![Line {
                kind: LineKind::ToolResult,
                text: "error: shell boom".into(),
            }]
        );

        let ask = Event::Message {
            message: Message {
                role: Role::Assistant,
                block: Block::Ask {
                    message: "pick".into(),
                    options: vec!["a".into(), "b".into()],
                    answer: None,
                },
            },
        };
        assert_eq!(
            event_lines(&ask),
            vec![Line {
                kind: LineKind::Ask,
                text: "pick [a, b]".into(),
            }]
        );

        let summary = Event::Message {
            message: Message {
                role: Role::Assistant,
                block: Block::Summary {
                    text: "compacted".into(),
                },
            },
        };
        assert_eq!(
            event_lines(&summary),
            vec![Line {
                kind: LineKind::Summary,
                text: "compacted".into(),
            }]
        );

        let subagent = Event::Message {
            message: Message {
                role: Role::Assistant,
                block: Block::Subagent {
                    subagent_type: "explorer".into(),
                    status: "done".into(),
                    text: "found it".into(),
                },
            },
        };
        assert_eq!(
            event_lines(&subagent),
            vec![Line {
                kind: LineKind::Subagent,
                text: "explorer done: found it".into(),
            }]
        );

        let state = Event::State {
            state: SessionState::WaitingForInput,
        };
        assert_eq!(
            event_lines(&state),
            vec![Line {
                kind: LineKind::Status,
                text: "state: waiting_for_input".into(),
            }]
        );

        let permission = Event::Permission {
            permission: Permission::ReadOnly,
        };
        assert_eq!(
            event_lines(&permission),
            vec![Line {
                kind: LineKind::Status,
                text: "permission: read_only".into(),
            }]
        );

        let model = Event::ModelCall {
            model: "alpha".into(),
            provider: "x".into(),
            kind: "completion".into(),
            input_tokens: Some(10),
            output_tokens: Some(2),
            cost: Some(0.01),
        };
        assert_eq!(
            event_lines(&model),
            vec![Line {
                kind: LineKind::ModelCall,
                text: "alpha completion (10 in, 2 out, $0.0100)".into(),
            }]
        );
    }

    #[test]
    fn apply_event_skips_replayed_seq() {
        let mut state = ClientState::new(Permission::ReadWrite);
        let event = Event::State {
            state: SessionState::Running,
        };
        assert!(state.apply_event(5, &event));
        assert_eq!(state.last_seq, 5);
        assert_eq!(state.lines.len(), 1);

        assert!(!state.apply_event(5, &event));
        assert!(!state.apply_event(3, &event));
        assert_eq!(state.lines.len(), 1);

        assert!(state.apply_event(6, &event));
        assert_eq!(state.last_seq, 6);
        assert_eq!(state.lines.len(), 2);
    }

    #[test]
    fn permission_events_update_the_state() {
        let mut state = ClientState::new(Permission::ReadWrite);
        let event = Event::Permission {
            permission: Permission::ReadOnly,
        };
        assert!(state.apply_event(1, &event));
        assert_eq!(state.permission, Permission::ReadOnly);
    }
}
