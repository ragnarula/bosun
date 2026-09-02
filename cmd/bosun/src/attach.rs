//! Interactive terminal client for a session: renders the live transcript
//! over SSE in a two-pane TUI (scrollable output, pinned input box) and sends
//! the user's input back over the session API.

use std::io;
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
use bosun_common::session::SessionState;
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
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use futures_util::Stream;
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line as TuiLine;
use ratatui::text::Span;
use ratatui::widgets::Block as TuiBlock;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::markdown::markdown_rows;
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
/// Submitted messages kept for arrow-key recall.
const MAX_HISTORY: usize = 200;

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
    pub session_state: SessionState,
}

impl ClientState {
    pub fn new(permission: Permission, session_state: SessionState) -> Self {
        Self {
            lines: Vec::new(),
            input: String::new(),
            last_seq: 0,
            pending_delta: None,
            permission,
            session_state,
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
        if let Event::State { state } = event {
            self.session_state = *state;
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
                        text: format!("{prefix}{name} {}", output_text(content)),
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

/// Renders a tool-call arg value on one line, cutting long text.
fn inline_value(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    clip(&text, MAX_INLINE_CHARS)
}

/// Renders a tool result, cutting long text. A result is the JSON the tool
/// returned; strings stay raw and structure flattens to `key: value` and
/// `- item` lines, so the content reads directly rather than as escaped JSON.
fn output_text(value: &Value) -> String {
    clip(&readable(value), MAX_INLINE_CHARS)
}

fn readable(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| format!("- {}", readable(item)))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, item)| format!("{key}: {}", readable(item)))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn clip(text: &str, max: usize) -> String {
    let mut clipped: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        clipped.push('…');
    }
    clipped
}

/// Word-wraps text to `width` columns, splitting on newlines first. A word
/// longer than the width is hard-wrapped at character width.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
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

/// The wrapped, prefixed transcript rows as styled spans: the durable lines
/// plus the live delta, cut to `width` columns. Assistant text is rendered as
/// markdown; everything else keeps the per-kind prefix and color.
fn transcript_rows(state: &ClientState, width: usize) -> Vec<(LineKind, Vec<Span<'static>>)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in &state.lines {
        if line.kind == LineKind::Assistant {
            for row in markdown_rows(&line.text, width) {
                rows.push((LineKind::Assistant, row));
            }
            continue;
        }
        let mut style = Style::default().fg(row_color(line.kind, &line.text));
        if line.kind == LineKind::Status {
            style = style.add_modifier(Modifier::BOLD);
        }
        let prefix = prefix_for(line);
        let prefix_width = prefix.chars().count();
        let inner = width.saturating_sub(prefix_width).max(1);
        let wrapped = wrap_text(&line.text, inner);
        for (i, row) in wrapped.iter().enumerate() {
            let indent = if i == 0 {
                prefix.to_string()
            } else {
                " ".repeat(prefix_width)
            };
            rows.push((
                line.kind,
                vec![
                    Span::styled(indent, style),
                    Span::styled(row.clone(), style),
                ],
            ));
        }
    }
    if let Some(pending) = &state.pending_delta {
        for row in markdown_rows(pending, width) {
            rows.push((LineKind::Assistant, row));
        }
    }
    rows
}

/// The color an error tool result renders with, distinct from the kind color.
fn row_color(kind: LineKind, text: &str) -> Color {
    if kind == LineKind::ToolResult && text.starts_with("error: ") {
        Color::Red
    } else {
        kind_color(kind)
    }
}

/// The per-kind prefix that aligns the transcript: messages start at the
/// margin, tool activity and meta rows are indented, state rows are separated.
fn prefix_for(line: &Line) -> &'static str {
    match line.kind {
        LineKind::User => "you: ",
        LineKind::Assistant => "",
        LineKind::ToolCall => "  → ",
        LineKind::ToolResult => "  ← ",
        LineKind::Ask => "  ? ",
        LineKind::Summary => "  ⤷ ",
        LineKind::Subagent => "  ⤷ ",
        LineKind::ModelCall => "  ",
        LineKind::Status => "── ",
    }
}

/// The color each transcript kind renders with.
fn kind_color(kind: LineKind) -> Color {
    match kind {
        LineKind::User => Color::Green,
        LineKind::Assistant => Color::White,
        LineKind::ToolCall => Color::Magenta,
        LineKind::ToolResult => Color::Blue,
        LineKind::Ask => Color::Yellow,
        LineKind::Summary | LineKind::Subagent | LineKind::ModelCall => Color::DarkGray,
        LineKind::Status => Color::Cyan,
    }
}

/// The `(kind, spans)` rows become styled TUI lines for the output pane; the
/// spans already carry the markdown and kind styling.
fn span_rows_to_lines(rows: &[(LineKind, Vec<Span<'static>>)]) -> Vec<TuiLine<'static>> {
    rows.iter()
        .map(|(_, spans)| TuiLine::from(spans.clone()))
        .collect()
}

/// The full client UI state, including the scroll position.
#[derive(Debug)]
pub struct App {
    pub session: Session,
    pub state: ClientState,
    /// Rows of the output pane that are hidden above the viewport.
    pub scroll: usize,
    /// Follow the newest rows instead of holding a manual scroll position.
    pub follow: bool,
    /// The output pane's inner height from the last draw, for paging.
    viewport: u16,
    /// The bottom scroll position from the last draw; scrolling to it
    /// resumes auto-follow.
    max_scroll: usize,
    connected: bool,
    /// Submitted message inputs, oldest first, for arrow-key recall.
    history: Vec<String>,
    /// Position in `history` the input currently shows; equal to
    /// `history.len()` while editing a fresh message.
    history_pos: usize,
    /// The draft being edited before arrow-key recall replaced it.
    draft: String,
}

impl App {
    pub fn new(session: Session) -> Self {
        let permission = session.permission;
        let session_state = session.state;
        Self {
            session,
            state: ClientState::new(permission, session_state),
            scroll: 0,
            follow: true,
            viewport: 0,
            max_scroll: 0,
            connected: false,
            history: Vec::new(),
            history_pos: 0,
            draft: String::new(),
        }
    }

    /// Moves the input to the previous submitted message, saving the current
    /// draft the first time the user leaves it.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_pos == self.history.len() {
            self.draft = std::mem::take(&mut self.state.input);
            self.history_pos = self.history.len() - 1;
        } else if self.history_pos > 0 {
            self.history_pos -= 1;
        }
        self.state.input = self.history[self.history_pos].clone();
    }

    /// Moves the input back toward the fresh draft after recall.
    fn history_down(&mut self) {
        if self.history_pos >= self.history.len() {
            return;
        }
        self.history_pos += 1;
        if self.history_pos == self.history.len() {
            self.state.input = std::mem::take(&mut self.draft);
        } else {
            self.state.input = self.history[self.history_pos].clone();
        }
    }

    /// Records a submitted message so the next session can recall it.
    fn history_submit(&mut self, content: &str) {
        if !content.is_empty() && self.history.last().map(String::as_str) != Some(content) {
            self.history.push(content.to_string());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        self.history_pos = self.history.len();
        self.draft.clear();
    }
}

/// Draws the whole screen: status bar, scrollable output pane, and the input
/// box pinned to the bottom with a visible cursor.
fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let status = status_line(app);
    frame.render_widget(Paragraph::new(status), chunks[0]);

    let output = chunks[1];
    let inner = output.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let width = inner.width as usize;
    let rows = transcript_rows(&app.state, width);
    app.viewport = inner.height;
    let max_scroll = rows.len().saturating_sub(inner.height as usize);
    app.max_scroll = max_scroll;
    let scroll = if app.follow {
        max_scroll
    } else {
        app.scroll.min(max_scroll)
    };
    app.scroll = scroll;
    let scroll_y = scroll.min(u16::MAX as usize) as u16;

    let tui_lines = span_rows_to_lines(&rows);
    let transcript = Paragraph::new(tui_lines)
        .block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    "transcript",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
        )
        .scroll((scroll_y, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, output);

    let input = chunks[2];
    let (text, tail_len) = input_row(&app.state.input, input.width.saturating_sub(2) as usize);
    let input_widget = Paragraph::new(text)
        .block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    "message  ·  esc/^C interrupt  ·  ^P permission  ·  ^Q quit  ·  ↑/↓ history  ·  pgup/pgdn scroll",
                    Style::default().fg(Color::DarkGray),
                )),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(input_widget, input);
    frame.set_cursor_position((input.x + 1 + 2 + tail_len as u16, input.y + 1));
}

fn status_line(app: &App) -> TuiLine<'static> {
    let id = clip(&app.session.id, 12);
    let state = state_name(app.state.session_state);
    let connection = if app.connected {
        "connected"
    } else {
        "connecting"
    };
    TuiLine::from(vec![
        Span::styled("session", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {id} · {} · ", app.session.model)),
        Span::styled(state, Style::default().fg(Color::Cyan)),
        Span::raw(format!(" · {} · ", permission_name(app.state.permission))),
        Span::styled(connection, Style::default().fg(Color::DarkGray)),
    ])
}

/// The input box content and the length of the visible text tail. The text is
/// the input tail that fits the box, prefixed with a prompt; the caller places
/// the cursor after it.
fn input_row(input: &str, inner_width: usize) -> (TuiLine<'static>, usize) {
    let inner_width = inner_width.max(1);
    let prompt_len = 2;
    let visible = inner_width.saturating_sub(prompt_len);
    let chars: Vec<char> = input.chars().collect();
    let tail: String = if chars.len() > visible {
        chars[chars.len() - visible..].iter().collect()
    } else {
        input.to_string()
    };
    let line = TuiLine::from(vec![
        Span::styled("> ", Style::default().fg(Color::DarkGray)),
        Span::raw(tail.clone()),
    ]);
    (line, tail.chars().count())
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
    let response = client
        .get(format!("{cp_url}/sessions/{session_id}"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("session {session_id} is not available"))?;
    crate::maybe_print_update_notice(response.headers());
    let session: Session = response.json().await.context("failed to parse session")?;

    terminal::enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;

    let quit = Arc::new(AtomicBool::new(false));
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    spawn_input_thread(input_tx, Arc::clone(&quit))?;

    // A SIGINT/SIGTERM from outside (kill -INT <pid>, or a Ctrl-C in the
    // terminal that owns the process) must exit cleanly: without a handler
    // the process dies without restoring the terminal, leaving the shell in
    // raw mode on the alternate screen.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    spawn_signal_waiter(stop_tx);

    let mut app = App::new(session);

    let result = run_attach(
        &mut terminal,
        &client,
        cp_url,
        session_id,
        &mut app,
        input_rx,
        stop_rx,
    )
    .await;
    quit.store(true, Ordering::Relaxed);
    let _ = terminal.show_cursor();
    result
}

/// Watches for SIGINT and SIGTERM and resolves `stop` on the first one. The
/// terminal restore guard then runs, so an external kill leaves the terminal
/// usable.
fn spawn_signal_waiter(stop: tokio::sync::oneshot::Sender<()>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::SignalKind;
            use tokio::signal::unix::signal;
            let mut sigint =
                signal(SignalKind::interrupt()).expect("failed to install the SIGINT handler");
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to install the SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = stop.send(());
    });
}

/// Restores the terminal when attach exits, including on error or panic.
struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
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
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    app: &mut App,
    mut input_rx: mpsc::UnboundedReceiver<TermEvent>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    redraw(terminal, app)?;
    let mut noticed = false;
    loop {
        let opened = tokio::select! {
            result = open_stream(client, cp_url, session_id, app.state.last_seq) => result,
            _ = &mut stop_rx => return Ok(()),
        };
        if let Ok(stream) = opened {
            // The stream restarted: live deltas do not survive a reconnect,
            // and the outage is over, so the next drop reports again.
            app.state.pending_delta = None;
            noticed = false;
            app.connected = true;
            redraw(terminal, app)?;
            let outcome = stream_events(
                terminal,
                app,
                &mut input_rx,
                client,
                cp_url,
                session_id,
                stream,
                &mut stop_rx,
            )
            .await?;
            if outcome == StreamOutcome::Exited {
                return Ok(());
            }
            app.connected = false;
            if !noticed {
                noticed = true;
                app.state.push_line(Line {
                    kind: LineKind::Status,
                    text: "connection lost; reconnecting".to_string(),
                });
                redraw(terminal, app)?;
            }
        } else if !noticed {
            noticed = true;
            app.state.push_line(Line {
                kind: LineKind::Status,
                text: "connection lost; reconnecting".to_string(),
            });
            redraw(terminal, app)?;
        }
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = &mut stop_rx => return Ok(()),
        }
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

#[allow(clippy::too_many_arguments)] // the stream loop needs the whole attach context
async fn stream_events(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    input_rx: &mut mpsc::UnboundedReceiver<TermEvent>,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    stream: impl Stream<Item = Result<SseEvent, SseError>>,
    stop_rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<StreamOutcome> {
    tokio::pin!(stream);
    loop {
        tokio::select! {
            event = input_rx.recv() => {
                let Some(event) = event else {
                    return Ok(StreamOutcome::Exited);
                };
                let action = handle_term_event(app, client, cp_url, session_id, event).await?;
                if action == Action::Exit {
                    return Ok(StreamOutcome::Exited);
                }
                redraw(terminal, app)?;
            }
            item = stream.next() => match item {
                Some(Ok(sse)) => {
                    apply_sse(&mut app.state, &sse);
                    redraw(terminal, app)?;
                }
                Some(Err(_)) | None => return Ok(StreamOutcome::Reconnect),
            },
            _ = &mut *stop_rx => return Ok(StreamOutcome::Exited),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Continue,
    Exit,
}

async fn handle_term_event(
    app: &mut App,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    event: TermEvent,
) -> anyhow::Result<Action> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, client, cp_url, session_id, key).await
        }
        TermEvent::Paste(text) => {
            push_input(&mut app.state, &text);
            Ok(Action::Continue)
        }
        _ => Ok(Action::Continue),
    }
}

async fn handle_key(
    app: &mut App,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
    key: KeyEvent,
) -> anyhow::Result<Action> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            interrupt(app, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_permission(app, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => Ok(Action::Exit),
        // Esc interrupts like Ctrl-C; Ctrl-Q and /exit quit.
        KeyCode::Esc => {
            interrupt(app, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        KeyCode::Enter => submit_input(app, client, cp_url, session_id).await,
        KeyCode::Backspace | KeyCode::Delete => {
            app.state.input.pop();
            Ok(Action::Continue)
        }
        // Up and Down recall previous inputs; PgUp/PgDn scroll the transcript.
        KeyCode::Up => {
            app.history_up();
            Ok(Action::Continue)
        }
        KeyCode::Down => {
            app.history_down();
            Ok(Action::Continue)
        }
        KeyCode::PageUp => {
            app.follow = false;
            app.scroll = app.scroll.saturating_sub(app.viewport.max(1) as usize);
            Ok(Action::Continue)
        }
        KeyCode::PageDown => {
            app.follow = false;
            app.scroll = app.scroll.saturating_add(app.viewport.max(1) as usize);
            if app.scroll >= app.max_scroll {
                app.follow = true;
            }
            Ok(Action::Continue)
        }
        KeyCode::End => {
            app.follow = true;
            Ok(Action::Continue)
        }
        KeyCode::Char(c) => {
            push_input(&mut app.state, &c.to_string());
            Ok(Action::Continue)
        }
        _ => Ok(Action::Continue),
    }
}

async fn submit_input(
    app: &mut App,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
) -> anyhow::Result<Action> {
    // Sending a message is the user saying "look at what comes next", so the
    // transcript returns to the newest rows even after a manual scroll-up.
    app.follow = true;
    let content = std::mem::take(&mut app.state.input);
    match content.as_str() {
        "/exit" => Ok(Action::Exit),
        "/permission" => {
            toggle_permission(app, client, cp_url, session_id).await;
            Ok(Action::Continue)
        }
        _ => {
            if !content.is_empty() {
                app.history_submit(&content);
                send_message(app, client, cp_url, session_id, &content).await;
            }
            Ok(Action::Continue)
        }
    }
}

async fn send_message(
    app: &mut App,
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
            app.state.push_line(Line {
                kind: LineKind::Status,
                text: "request timed out".into(),
            });
            return;
        }
    };
    if let Err(error) = result {
        app.state.push_line(Line {
            kind: LineKind::Status,
            text: format!("send failed: {error}"),
        });
    }
}

async fn interrupt(app: &mut App, client: &reqwest::Client, cp_url: &str, session_id: &str) {
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
            app.state.push_line(Line {
                kind: LineKind::Status,
                text: "request timed out".into(),
            });
            return;
        }
    };
    if let Err(error) = result {
        app.state.push_line(Line {
            kind: LineKind::Status,
            text: format!("interrupt failed: {error}"),
        });
    }
}

async fn toggle_permission(
    app: &mut App,
    client: &reqwest::Client,
    cp_url: &str,
    session_id: &str,
) {
    let next = match app.state.permission {
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
            app.state.push_line(Line {
                kind: LineKind::Status,
                text: "request timed out".into(),
            });
            return;
        }
    };
    match result {
        Ok(_) => app.state.permission = next,
        Err(error) => app.state.push_line(Line {
            kind: LineKind::Status,
            text: format!("permission change failed: {error}"),
        }),
    }
}

/// Renders the current frame.
fn redraw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    terminal
        .draw(|frame| draw(frame, app))
        .context("failed to render the terminal")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bosun_common::session::Message;
    use bosun_common::session::SessionState;
    use serde_json::json;

    use super::*;

    #[test]
    fn wrap_hard_breaks_words_longer_than_the_width() {
        assert_eq!(wrap_text("abcdefghij", 5), vec!["abcde", "fghij"]);
        assert_eq!(wrap_text("abcdefghijk", 5), vec!["abcde", "fghij", "k"]);
        assert_eq!(
            wrap_text("one abcdefghij two", 5),
            vec!["one", "abcde", "fghij", "two"]
        );
        assert_eq!(wrap_text("ab", 5), vec!["ab"]);
    }

    #[test]
    fn tool_results_render_json_content_as_readable_lines() {
        assert_eq!(output_text(&json!("plain")), "plain");
        assert_eq!(
            output_text(&json!({ "stdout": "hi", "stderr": "", "exit_code": 0 })),
            "exit_code: 0\nstderr: \nstdout: hi"
        );
        assert_eq!(output_text(&json!({ "content": "a\nb" })), "content: a\nb");
        assert_eq!(
            output_text(&json!({ "paths": ["a", "b"] })),
            "paths: - a\n- b"
        );
        assert_eq!(
            output_text(&json!([{ "line": 1, "path": "a.rs", "text": "fn" }])),
            "- line: 1\npath: a.rs\ntext: fn"
        );
    }

    #[test]
    fn tool_results_are_cut_to_the_inline_limit() {
        let long = "x".repeat(MAX_INLINE_CHARS + 10);
        let rendered = output_text(&json!({ "content": long }));
        assert_eq!(rendered.chars().count(), MAX_INLINE_CHARS + 1);
        assert!(rendered.ends_with('…'));
    }

    fn row_texts(rows: &[(LineKind, Vec<Span<'static>>)]) -> Vec<String> {
        rows.iter()
            .map(|(_, spans)| TuiLine::from(spans.clone()).to_string())
            .collect()
    }

    #[test]
    fn transcript_rows_align_each_kind_and_include_the_delta() {
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
        state.push_line(Line {
            kind: LineKind::User,
            text: "hello".into(),
        });
        state.push_line(Line {
            kind: LineKind::ToolCall,
            text: "shell {\"cmd\":\"ls\"}".into(),
        });
        state.push_line(Line {
            kind: LineKind::Status,
            text: "state: waiting_for_input".into(),
        });
        state.pending_delta = Some("streaming".into());

        let rows = transcript_rows(&state, 40);
        assert_eq!(
            row_texts(&rows),
            vec![
                "you: hello",
                "  → shell {\"cmd\":\"ls\"}",
                "── state: waiting_for_input",
                "streaming"
            ]
        );
        let kinds: Vec<LineKind> = rows.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::User,
                LineKind::ToolCall,
                LineKind::Status,
                LineKind::Assistant,
            ]
        );
    }

    #[test]
    fn transcript_rows_wrap_long_rows() {
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
        state.push_line(Line {
            kind: LineKind::Assistant,
            text: "one two three four".into(),
        });
        let rows = transcript_rows(&state, 10);
        assert_eq!(row_texts(&rows), vec!["one two", "three four"]);
    }

    #[test]
    fn assistant_rows_render_markdown() {
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
        state.push_line(Line {
            kind: LineKind::Assistant,
            text: "## Done\n\nIt **works** now.".into(),
        });
        let rows = transcript_rows(&state, 40);
        assert_eq!(row_texts(&rows), vec!["Done", "", "It works now."]);
        let (kind, spans) = &rows[0];
        assert_eq!(*kind, LineKind::Assistant);
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn input_row_tails_a_long_input_and_reports_the_visible_length() {
        let (line, tail_len) = input_row("hello", 10);
        assert_eq!(line.to_string(), "> hello");
        assert_eq!(tail_len, 5);

        let (line, tail_len) = input_row(&"x".repeat(50), 10);
        let text = line.to_string();
        assert_eq!(text.chars().count(), 10, "the input tail fits the box");
        assert_eq!(tail_len, 8);
    }

    #[test]
    fn input_is_capped_at_max_input_chars() {
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
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
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
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
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
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
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);

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
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
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
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
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
        let mut state = ClientState::new(Permission::ReadWrite, SessionState::WaitingForInput);
        let event = Event::Permission {
            permission: Permission::ReadOnly,
        };
        assert!(state.apply_event(1, &event));
        assert_eq!(state.permission, Permission::ReadOnly);
    }

    fn test_session() -> Session {
        Session {
            id: "s1".into(),
            node: "n1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/tmp".into(),
            model: "m".into(),
            permission: Permission::ReadWrite,
            state: SessionState::WaitingForInput,
            created_at_secs: 0,
            prompt: None,
        }
    }

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        let mut text = String::new();
        for x in 0..width {
            text.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(""));
        }
        text
    }

    #[test]
    fn follow_keeps_the_newest_rows_visible() {
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let mut app = App::new(test_session());
        for i in 0..50 {
            app.state.push_line(Line {
                kind: LineKind::Assistant,
                text: format!("line {i}"),
            });
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        // Layout: status row 0, output rows 1..=6 (inner 2..=5), input 7..=9.
        let bottom = buffer_row_text(terminal.backend().buffer(), 5, 40);
        assert!(
            bottom.contains("line 49"),
            "the newest row must be visible at the bottom: {bottom:?}"
        );

        for i in 50..60 {
            app.state.push_line(Line {
                kind: LineKind::Assistant,
                text: format!("line {i}"),
            });
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let bottom = buffer_row_text(terminal.backend().buffer(), 5, 40);
        assert!(
            bottom.contains("line 59"),
            "the view must follow to the newest row: {bottom:?}"
        );
    }

    #[tokio::test]
    async fn paging_up_disables_follow_and_paging_to_the_bottom_resumes_it() {
        let client = reqwest::Client::new();
        let mut app = App::new(test_session());
        app.follow = true;
        app.max_scroll = 100;
        app.viewport = 30;

        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.follow = true;
        handle_key(&mut app, &client, "http://x", "s1", key(KeyCode::PageUp))
            .await
            .unwrap();
        assert!(!app.follow, "paging up must stop auto-follow");

        // Paging back to the bottom resumes auto-follow, no End needed.
        app.follow = false;
        app.scroll = 80;
        handle_key(&mut app, &client, "http://x", "s1", key(KeyCode::PageDown))
            .await
            .unwrap();
        assert!(app.follow, "paging to the bottom must resume auto-follow");
    }

    #[test]
    fn up_and_down_arrows_recall_submitted_inputs() {
        let mut app = App::new(test_session());
        app.history_submit("first message");
        app.history_submit("second message");

        // Up recalls the newest, then the previous one.
        app.history_up();
        assert_eq!(app.state.input, "second message");
        app.history_up();
        assert_eq!(app.state.input, "first message");

        // Down walks back to the draft that was being edited.
        app.draft = "draft in progress".into();
        app.history_pos = app.history.len();
        app.state.input = app.draft.clone();
        app.history_up();
        assert_eq!(app.state.input, "second message");
        app.history_down();
        assert_eq!(app.state.input, "draft in progress");
    }

    #[test]
    fn history_keeps_the_last_entry_and_skips_duplicates() {
        let mut app = App::new(test_session());
        for i in 0..(MAX_HISTORY + 5) {
            app.history_submit(&format!("message {i}"));
        }
        assert_eq!(app.history.len(), MAX_HISTORY);
        assert_eq!(app.history[0], "message 5");

        app.history_submit("repeat");
        app.history_submit("repeat");
        assert_eq!(app.history.last().map(String::as_str), Some("repeat"));
        assert_eq!(
            app.history
                .iter()
                .filter(|m| m.as_str() == "repeat")
                .count(),
            1,
            "consecutive duplicates must not be stored twice"
        );
    }

    #[tokio::test]
    async fn escape_interrupts_instead_of_exiting() {
        let client = reqwest::Client::new();
        let mut app = App::new(test_session());
        let action = handle_key(
            &mut app,
            &client,
            "http://x",
            "s1",
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(action, Action::Continue, "Esc must interrupt, not exit");
        assert!(
            app.state
                .lines
                .iter()
                .any(|line| line.text.starts_with("interrupt failed")
                    || line.text.starts_with("request timed out")),
            "Esc must issue an interrupt request"
        );
    }

    #[tokio::test]
    async fn submitting_re_enables_follow() {
        let client = reqwest::Client::new();
        let mut app = App::new(test_session());
        app.follow = false;

        let action = submit_input(&mut app, &client, "http://x", "s1")
            .await
            .unwrap();
        assert_eq!(action, Action::Continue);
        assert!(
            app.follow,
            "sending a message must return to the newest rows"
        );
    }
}
