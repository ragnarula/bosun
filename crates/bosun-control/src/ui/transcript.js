// The transcript's block renderers. Each one returns the element it built, and
// the caller decides which transcript it goes into and whether that transcript
// follows it.
//
// A transcript has a record the renderers read and write: `askBox`, the box of
// the most recent question, which the durable answered-ask event updates in
// place instead of drawing the question twice; and `callArgs`, the arguments of
// the `message_child` calls drawn in it, by call id. The session's transcript,
// a crew member's Log and a page read back each pass their own.

import { sessions } from './session-list.js';
import { openMember } from './history.js';
import { USER_REJECTED_TEXT } from './composer.js';
import { renderMarkdown } from './markdown.js';
import { childName, watchControl } from './member-log.js';

export { clip, drawAssistant, drawBlock, drawLine, drawLiveParagraph, modelCallLine };

// A durable event's stamp as the reader's local clock time. The wire carries
// UTC; the browser converts it, and an empty locale list keeps the reader's
// own separators. The hour cycle is explicit because the locale picks it, and
// the h24 cycle renders the day's first hour as 24 rather than 00; h23 pins
// the 00-23 clock.
function clockTime(atMs) {
  return new Date(atMs).toLocaleTimeString([], { hourCycle: 'h23' });
}

// An entry whose event carries no stamp (stored before the field existed)
// appends as itself, with no row and no time.
function stampRow(el, atMs) {
  if (atMs == null) return el;
  const row = document.createElement('div');
  row.className = 'stamp-row';
  const ts = document.createElement('span');
  ts.className = 'ts';
  ts.textContent = clockTime(atMs);
  row.appendChild(ts);
  row.appendChild(el);
  return row;
}

function drawLine(kind, text, atMs) {
  const line = document.createElement('div');
  line.className = 'line ' + kind;
  line.textContent = text;
  return stampRow(line, atMs);
}

// Tool traffic renders as one subtle strip so assistant text is not buried
// under long tool output. The strip shows a per-tool glyph, the tool name,
// and the args; the payload stays hidden until the strip is clicked.
function toolGlyph(name) {
  // Mirrors the TUI's `tool_glyph` in `cmd/bosun/src/attach.rs`; keep both in
  // step with the canonical tool list in `crates/bosun-common/src/tool.rs`.
  const glyphs = {
    shell: '$',
    'file_read': '→',
    'file_write': '✎',
    edit: '✎',
    grep: '⌕',
    glob: '✱',
    ask: '?',
    todowrite: '✓',
    history_read: '⎇',
    webfetch: '↗',
    skill: '⚒',
    spawn: '⊕',
    message_child: '⇄',
    session_status: '◉',
  };
  // A hasOwnProperty guard keeps a tool literally named `constructor` or
  // `toString` from printing Object.prototype's function source.
  return Object.prototype.hasOwnProperty.call(glyphs, name) ? glyphs[name] : '⚙';
}

// The compact one-line label for a tool call, opencode-style: the tool name
// plus the arguments that say what it acted on, never the raw JSON. Falls
// back to the first string argument for unknown tools. Whitespace is
// collapsed so a multi-line command or heredoc stays one row; the full args
// live in the hidden body.
function toolSummary(name, args) {
  if (args === null || typeof args !== 'object' || Array.isArray(args)) return name;
  const s = (value) =>
    typeof value === 'string' ? value.replace(/\s+/g, ' ').trim() : '';
  const first = Object.values(args).find((value) => typeof value === 'string') || '';
  switch (name) {
    case 'shell': {
      const command = s(args.command);
      return command ? name + ' ' + clip(command, TOOL_SUMMARY_LIMIT) : name;
    }
    case 'file_read':
    case 'file_write':
    case 'edit':
      return name + (s(args.path) ? ' ' + clip(s(args.path), TOOL_SUMMARY_LIMIT) : '');
    case 'grep':
      return name + (s(args.pattern) ? ' "' + clip(s(args.pattern), TOOL_SUMMARY_LIMIT) + '"' : '');
    case 'glob':
      return name + (s(args.pattern) ? ' "' + clip(s(args.pattern), TOOL_SUMMARY_LIMIT) + '"' : '');
    case 'ask':
      return name + (s(args.message) ? ' "' + clip(s(args.message), TOOL_SUMMARY_LIMIT) + '"' : '');
    case 'todowrite':
      return args.items && args.items.length ? name + ' (' + args.items.length + ')' : name;
    case 'git':
      return Array.isArray(args.args) && args.args.length
        ? 'git ' + clip(args.args.join(' '), TOOL_SUMMARY_LIMIT)
        : 'git';
    case 'webfetch':
      return name + (s(args.url) ? ' ' + clip(s(args.url), TOOL_SUMMARY_LIMIT) : '');
    case 'skill':
      return name + (s(args.name) ? ' "' + clip(s(args.name), TOOL_SUMMARY_LIMIT) + '"' : '');
    case 'spawn':
      return name + (s(args.persona) ? ' ' + clip(s(args.persona), TOOL_SUMMARY_LIMIT) : '');
    case 'message_child':
      return name + (s(args.id) ? ' ' + clip(s(args.id), TOOL_SUMMARY_LIMIT) : '');
    default:
      return first ? name + ' ' + clip(s(first), TOOL_SUMMARY_LIMIT) : name;
  }
}

function clip(text, max) {
  const chars = Array.from(text);
  return chars.length > max ? chars.slice(0, max).join('') + '…' : text;
}

// Mirrors the TUI's `clip` in `cmd/bosun/src/attach.rs`; keep the two in step.

// The one-line label for a shell command or path is clipped so a very long
// command does not wrap the tool strip into many lines.
const TOOL_SUMMARY_LIMIT = 120;

// One-line tool strip: the glyph, the tool name, and a compact label of what
// it acted on. The full args stay hidden until the line is clicked.
function drawToolStrip(name, args, atMs, id, record) {
  // A `message_child` result answers `{"ok": true}`, so the child it names is
  // the one its call named, and only that call's arguments are kept. Keeping
  // them for every call would hold a `file_write`'s whole body for as long as
  // the transcript lives.
  if (id && name === 'message_child') record.callArgs.set(id, args);
  const line = document.createElement('div');
  line.className = 'line tool';
  const glyph = document.createElement('span');
  glyph.className = 'glyph';
  glyph.textContent = toolGlyph(name);
  const toggle = document.createElement('span');
  toggle.className = 'toggle';
  toggle.textContent = '▸';
  const summary = document.createElement('span');
  summary.textContent = toolSummary(name, args);
  const body = document.createElement('span');
  body.hidden = true;
  body.textContent = readableValue(args);
  line.appendChild(toggle);
  line.appendChild(glyph);
  line.appendChild(summary);
  line.appendChild(body);
  line.addEventListener('click', () => {
    const wasOpen = !body.hidden;
    if (wasOpen) {
      body.hidden = true;
      line.classList.remove('open');
      toggle.textContent = '▸';
      return;
    }
    body.hidden = false;
    line.classList.add('open');
    toggle.textContent = '▾';
  });
  return stampRow(line, atMs);
}

// A tool result nests under its call as a bordered block, so the call and
// its outcome read as one unit. Long output is clipped with a click to
// expand; error results render red with the tool name kept. An empty result
// (a write that returned `{}`) adds nothing beyond the call's strip.
// The preview is shorter than the TUI's `MAX_INLINE_CHARS` (1000) because
// the full text is one click away here, where the TUI row is final.
const TOOL_RESULT_PREVIEW = 400;

// One completion's thinking, collapsed. The label carries its size so the
// reader can judge whether to open it without it being shown twice.
function drawReasoningPanel(text, atMs) {
  const body = text == null ? '' : String(text);
  if (body === '') return null;
  const line = document.createElement('div');
  line.className = 'line tool reasoning';
  const toggle = document.createElement('span');
  toggle.className = 'toggle';
  toggle.textContent = '\u25b8';
  const glyph = document.createElement('span');
  glyph.className = 'glyph';
  glyph.textContent = '\u22ee';
  const label = document.createElement('span');
  label.textContent = 'thinking \u00b7 ' + body.length + ' chars';
  const full = document.createElement('span');
  full.className = 'body';
  full.hidden = true;
  full.textContent = body;
  line.appendChild(toggle);
  line.appendChild(glyph);
  line.appendChild(label);
  line.appendChild(full);
  line.addEventListener('click', () => {
    if (!full.hidden) {
      full.hidden = true;
      line.classList.remove('open');
      toggle.textContent = '\u25b8';
      return;
    }
    full.hidden = false;
    line.classList.add('open');
    toggle.textContent = '\u25be';
  });
  return stampRow(line, atMs);
}

// The child a result names, or undefined. Two tools are asked, and only by
// name: a `spawn` result carries the child it made in its own content, and a
// `message_child` result answers `ok`, so its child comes from the arguments of
// the call it answers. Another tool's result may hold a `child_id` of its own —
// an MCP tool's JSON — and another tool's `id` argument is not a child at all.
// A call that failed names no child: the one it meant may not exist.
function namedChild(content, name, error, id, record) {
  if (error) return undefined;
  if (name === 'spawn') {
    const made = content && typeof content === 'object' ? content.child_id : undefined;
    return typeof made === 'string' && made ? made : undefined;
  }
  if (name !== 'message_child' || !id) return undefined;
  const args = record.callArgs.get(id);
  return args && typeof args.id === 'string' && args.id ? args.id : undefined;
}

function drawToolResult(name, payload, error, atMs, id, content, record) {
  const text = payload == null ? '' : String(payload);
  const named = namedChild(content, name, error, id, record);
  // A `message_child` call's arguments go as its result renders: their child is
  // known now. A call that never gets an answer leaves them in the record,
  // which goes with the transcript it was drawn in.
  if (id && name === 'message_child') record.callArgs.delete(id);
  if (!error && text === '') return null;
  const line = document.createElement('div');
  line.className = 'tool-result' + (error ? ' error' : '');
  if (error) {
    const label = document.createElement('span');
    label.className = 'error-label';
    label.textContent = name + ' failed';
    line.appendChild(label);
  }
  const preview = clip(text, TOOL_RESULT_PREVIEW);
  const body = document.createElement('span');
  body.textContent = preview;
  line.appendChild(body);
  if (named) line.appendChild(watchControl(named));
  if (preview !== text) {
    line.classList.add('clipped');
    const hint = document.createElement('span');
    hint.className = 'expand-hint';
    hint.textContent = 'tap to expand';
    line.appendChild(hint);
    line.addEventListener('click', () => {
      if (line.classList.contains('expanded')) {
        body.textContent = preview;
        hint.textContent = 'tap to expand';
        line.classList.remove('expanded');
        line.classList.add('clipped');
      } else {
        body.textContent = text;
        hint.textContent = 'tap to collapse';
        line.classList.add('expanded');
        line.classList.remove('clipped');
      }
    });
  }
  return stampRow(line, atMs);
}

// A tool result for a human reader: strings read raw and structure flattens
// to `key: value` and `- item` lines instead of JSON.
function readableValue(value) {
  if (typeof value === 'string') return value;
  if (value === null) return 'null';
  if (Array.isArray(value)) {
    return value.map((item) => '- ' + readableValue(item)).join('\n');
  }
  if (typeof value === 'object') {
    return Object.keys(value)
      .map((key) => key + ': ' + readableValue(value[key]))
      .join('\n');
  }
  return String(value);
}

function drawMsg(role, text, atMs) {
  const paragraph = document.createElement('p');
  paragraph.className = 'msg ' + role;
  // A rejection is a durable user action, not a typed message; it renders
  // as a muted note so it never reads as words the user composed.
  if (role === 'user' && text === USER_REJECTED_TEXT) paragraph.classList.add('rejected');
  paragraph.textContent = text;
  return stampRow(paragraph, atMs);
}

// Assistant turns render markdown like the terminal client; user text stays
// plain. Renders the same subset the TUI parses: headings, emphasis, inline
// code, code fences, links, lists, blockquotes, rules and tables.
function drawAssistant(text, atMs) {
  const container = document.createElement('div');
  container.className = 'msg assistant';
  container.appendChild(renderMarkdown(text));
  return stampRow(container, atMs);
}

// The paragraph live deltas stream into until the turn's durable text arrives
// (which supersedes the stream) or another message begins. It is not durable,
// so it carries no time.
function drawLiveParagraph() {
  const paragraph = document.createElement('p');
  paragraph.className = 'msg assistant';
  return paragraph;
}

function askBoxKey(block) {
  return (block.child_id || '') + '\u0000' + block.message;
}

// The durable ask record in the transcript. When an ask is answered, the
// control plane appends a matching answered-ask event after the surface it
// resolves; the box updates in place so the same question never renders twice,
// and nothing new is returned.
function drawAskBox(block, atMs, record) {
  const key = askBoxKey(block);
  const open = record.askBox;
  if (open && open.key === key && !open.answered) {
    if (block.answer) {
      const note = document.createElement('div');
      note.className = 'answer';
      note.textContent = 'answered: ' + block.answer;
      open.el.appendChild(note);
      open.answered = true;
    }
    return null;
  }
  const box = document.createElement('div');
  box.className = 'ask';
  box.dataset.key = key;
  const question = document.createElement('div');
  if (block.child_id) {
    const to = document.createElement('span');
    to.className = 'ask-to';
    to.textContent = 'child ' + block.child_id + ' asks: ';
    question.appendChild(to);
  }
  question.appendChild(document.createTextNode(block.message));
  box.appendChild(question);
  const options = document.createElement('ul');
  options.className = 'options';
  for (const option of block.options || []) {
    const item = document.createElement('li');
    item.textContent = option;
    options.appendChild(item);
  }
  box.appendChild(options);
  if (block.answer) {
    const note = document.createElement('div');
    note.className = 'answer';
    note.textContent = 'answered: ' + block.answer;
    box.appendChild(note);
  }
  if (block.child_id) box.appendChild(watchControl(block.child_id));
  record.askBox = { key, el: box, answered: !!block.answer };
  return stampRow(box, atMs);
}

// One durable message block, drawn with `record`, the ask record and call
// arguments of the transcript it goes into. Returns the element, or null when
// the block adds nothing: an empty result, or an answer that joined the box of
// its question. The caller owns the state around it: the session's last
// message and its live paragraph.
function drawBlock(message, atMs, record) {
  const block = message.block;
  switch (block.kind) {
    case 'text':
      return drawMsg(message.role === 'user' ? 'user' : 'assistant', block.text, atMs);
    case 'tool_call': {
      const args = block.args == null ? {} : block.args;
      return drawToolStrip(block.name, args, atMs, block.id, record);
    }
    case 'tool_result': {
      const content = block.content;
      const payload =
        typeof content === 'string' || content === null
          ? String(content)
          : readableValue(content);
      return drawToolResult(block.name, payload, block.is_error, atMs, block.id, content, record);
    }
    case 'ask':
      return drawAskBox(block, atMs, record);
    case 'reasoning':
      return drawReasoningPanel(block.text, atMs);
    case 'summary':
      return drawLine('summary', block.text, atMs);
    case 'context_size':
      // How full the context was when the last completion finished. The line is
      // the loop's own count, not an estimate, and its percentage is the only
      // warning there is.
      return drawLine(
        'context',
        'context: ' + block.tokens + ' / ' + block.window + ' tokens (' +
          Math.floor((block.tokens * 100) / block.window) + '%), compaction at ' + block.compact_at,
        atMs
      );
    case 'context_cleared':
      // The break the session made in its own thread: a divider naming why it
      // cleared, not something anyone said. The fresh instructions stand below
      // it as the message the session continues from, so this line does not
      // repeat them.
      return drawLine(
        'cleared',
        'context cleared: ' + block.reason + ' · continuing from a fresh prompt',
        atMs
      );
    case 'child_event': {
      // Child activity renders as one line whose name and watch control open
      // the child's screen.
      const line = document.createElement('div');
      line.className = 'line child-report';
      const head = document.createElement('span');
      head.textContent = '[child ';
      line.appendChild(head);
      const child = sessions.find((session) => session.id === block.child_id);
      const link = document.createElement('span');
      link.className = 'child-link';
      link.textContent = childName(child) || block.child_id;
      link.title = block.child_id;
      link.addEventListener('click', () => openMember(block.child_id));
      line.appendChild(link);
      line.appendChild(watchControl(block.child_id));
      const tail = document.createElement('span');
      tail.textContent = ' ' + (block.event_kind || 'report') + '] ' + block.text;
      line.appendChild(tail);
      return stampRow(line, atMs);
    }
    default:
      return drawLine('unknown', JSON.stringify(block), atMs);
  }
}

// The transcript line for a model call. Mirrors the terminal's `event_lines`
// in `cmd/bosun/src/attach.rs`: the model, the call kind, and only the counts
// that are present, with the cost at four decimals. The wire field for the
// Rust `kind` is `call_kind`, renamed to avoid the event's own `kind` tag.
function modelCallLine(event) {
  const parts = [];
  if (event.input_tokens != null) parts.push(event.input_tokens + ' in');
  if (event.cached_input_tokens != null) parts.push(event.cached_input_tokens + ' cached');
  if (event.output_tokens != null) parts.push(event.output_tokens + ' out');
  // The terminal formats the cost with Rust's `{:.4}` (half to even), this
  // uses `toFixed(4)` (half away from zero). The two differ only on an exact
  // half at the fourth decimal; the difference is accepted for display.
  if (event.cost != null) parts.push('$' + event.cost.toFixed(4));
  const detail = parts.length ? ' (' + parts.join(', ') + ')' : '';
  return event.model + ' ' + event.call_kind + detail;
}
