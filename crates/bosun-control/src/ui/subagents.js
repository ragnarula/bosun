// The subagent panel: one followed child session, on its own event stream.

import {
  btnChildCollapse,
  childList,
  childPanel,
  childPanelDot,
  childPanelTitle,
  childTranscript,
} from './dom.js';
import { showStatus } from './common.js';
import { sessions } from './session-list.js';
import { opened } from './session-view.js';
import { clip, drawAssistant, drawBlock } from './transcript.js';

export { childName, closeChildPanel, renderChildList, updateChildPanelDot, watchControl };

// ---- Subagent panel ----
// One child at a time, followed on its own event stream while the session stays
// exactly where it is. The panel writes no history entry, carries no composer
// and adds no watch-only banner: it is a second reader of a child's own
// transcript, not a second session view. It starts closed, and closing the
// session takes it with it: the open session's `panel` is the followed child's
// state, or null while the panel is closed.

// The child the panel follows, or null.
function followed() {
  return opened && opened.panel ? opened.panel.id : null;
}

/// How wide a child's name may be before it is cut: one line of a panel row.
const CHILD_NAME_MAX = 60;

// What a child is called: its own one-line summary once its model has written
// one, otherwise the first line of the instructions it was spawned with. The id
// is the last resort, and it stays in the title and the list's detail column, so
// two children whose names read alike are still tellable apart.
function childName(child) {
  if (!child) return '';
  if (child.summary) return clip(child.summary, CHILD_NAME_MAX);
  const first = (child.prompt || '')
    .split('\n')
    .map((line) => line.trim())
    .find((line) => line);
  return first ? clip(first, CHILD_NAME_MAX) : child.id;
}

// The control that follows a child in the panel, wherever the transcript names
// one: the child's line carries it, and so does every other block that names a
// child, so a reader can follow from where the child is mentioned.
function watchControl(childId) {
  const watch = document.createElement('button');
  watch.type = 'button';
  watch.className = 'child-watch';
  watch.textContent = 'watch';
  watch.title = 'follow child ' + childId + ' in the panel';
  watch.addEventListener('click', (event) => {
    // The line it sits in may expand on a tap of its own.
    event.stopPropagation();
    followChild(childId);
  });
  return watch;
}

// The open session's direct children, running and stopped, each with its state
// and its name. The followed one is marked, and a row follows its child.
function renderChildList() {
  // No open session means no children: every root would match a null parent.
  const children = opened
    ? sessions.filter((session) => session.parent_id === opened.id)
    : [];
  childList.textContent = '';
  childList.hidden = children.length === 0;
  if (!children.length) {
    const note = document.createElement('span');
    note.className = 'child-list-note';
    note.textContent = 'no children';
    childList.appendChild(note);
    childList.hidden = false;
    return;
  }
  for (const child of children) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'child-row' + (child.id === followed() ? ' followed' : '');
    row.title = child.id;
    const dot = document.createElement('span');
    dot.className = 'dot ' + child.state;
    row.appendChild(dot);
    const name = document.createElement('span');
    name.className = 'child-row-name';
    name.textContent = childName(child);
    row.appendChild(name);
    const id = document.createElement('span');
    id.className = 'child-row-id';
    id.textContent = child.id.slice(0, 8);
    row.appendChild(id);
    row.addEventListener('click', () => followChild(child.id));
    childList.appendChild(row);
  }
}

function updateChildPanelDot() {
  const child = sessions.find((session) => session.id === followed());
  childPanelDot.className = child ? 'dot ' + child.state : 'dot';
  // The name arrives with the child's first summary, so the poll refreshes it.
  // A child the list has dropped keeps the name it had: this is a header, not a
  // status line.
  if (child) {
    childPanelTitle.textContent = childName(child);
    childPanelTitle.title = child.id;
  }
}

// Follows `id` in the panel: the previous child's stream closes first, so one
// child is followed at a time. Nothing here touches the address bar or the
// history: the session on screen keeps its entry and its place.
function followChild(id) {
  const s = opened;
  if (!id || !s) return;
  if (s.panel && s.panel.id === id) {
    childPanel.hidden = false;
    return;
  }
  closeChildPanel();
  // The panel's transcript has its own follow flag and its own record for the
  // block renderers, so a child's question and the frame that answers it are
  // one box there and the session's record is untouched.
  const panel = { id, es: null, stick: true, askBox: null, callArgs: new Map() };
  s.panel = panel;
  updateChildPanelDot();
  renderChildList();
  childPanel.hidden = false;
  // EventSource reconnects automatically; the child's durable frames carry
  // their seq as the SSE id, so the browser resumes with Last-Event-ID.
  panel.es = new EventSource('/sessions/' + encodeURIComponent(id) + '/events');
  panel.es.onmessage = (event) => {
    if (opened !== s || s.panel !== panel) return;
    try {
      handleChildFrame(panel, JSON.parse(event.data));
    } catch (error) {
      showStatus('child events: ' + error.message);
    }
  };
  panel.es.onerror = () => {
    if (opened === s && s.panel === panel) showStatus('child events: stream lost, reconnecting');
  };
}

// Collapsing leaves the session's view exactly as it was: the panel closes the
// child's stream and drops the lines it drew, and nothing else changes.
function closeChildPanel() {
  if (opened && opened.panel) {
    opened.panel.es.close();
    opened.panel = null;
  }
  childTranscript.textContent = '';
  childPanelTitle.textContent = '';
  updateChildPanelDot();
  childPanel.hidden = true;
  renderChildList();
}

// The panel's own frame handler: a child's durable messages render into the
// panel and nothing else does. The session-state frames — the header dot, the
// status label, the activity console, the ask composer — describe the session
// the pane is showing, so a child's frames never touch them. The child's live
// paragraph and last message are not the session's either.
function handleChildFrame(panel, frame) {
  if (!frame.event || frame.event.kind !== 'message') return;
  const message = frame.event.message;
  const atMs = frame.event.at_ms;
  const drawn = message.role === 'assistant' && message.block.kind === 'text'
    ? drawAssistant(message.block.text, atMs)
    : drawBlock(message, atMs, panel);
  if (drawn) childTranscript.appendChild(drawn);
  followPanel();
}

// The panel's transcript stays at the child's newest line until the reader
// scrolls the panel up.
function followPanel() {
  const panel = opened && opened.panel;
  if (panel && panel.stick) childTranscript.scrollTop = childTranscript.scrollHeight;
}

childTranscript.addEventListener('scroll', () => {
  const panel = opened && opened.panel;
  if (!panel) return;
  panel.stick =
    childTranscript.scrollTop + childTranscript.clientHeight >=
    childTranscript.scrollHeight - 40;
});
// A tap that opens a line makes the panel's transcript taller.
childTranscript.addEventListener('click', followPanel);

btnChildCollapse.addEventListener('click', closeChildPanel);
