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
import { current } from './session-view.js';
import {
  appendAssistant,
  clip,
  openAskBox,
  out,
  renderBlock,
  setOpenAskBox,
  setOut,
} from './transcript.js';

export {
  childName,
  childStick,
  closeChildPanel,
  renderChildList,
  updateChildPanelDot,
  watchControl,
};

let childFollow = null;  // the child the panel shows, or null when it is closed
let childEs = null;      // that child's own event stream
let childStick = true;   // auto-scroll for the panel's transcript, its own flag
// The ask record of the panel's transcript. The renderers keep the record of the
// box they last drew in `openAskBox`, which belongs to the session: the panel
// keeps its own, so a child's question and the frame that answers it are one box
// there and neither record is lost.
let childAskBox = null;

// ---- Subagent panel ----
// One child at a time, followed on its own event stream while the session stays
// exactly where it is. The panel writes no history entry, carries no composer
// and adds no watch-only banner: it is a second reader of a child's own
// transcript, not a second session view. It starts closed, and closing the
// session takes it with it.

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
  // No open session means no children: `closeSession` nulls `current` before
  // this runs, and every root would match a null parent.
  const children = current
    ? sessions.filter((session) => session.parent_id === current)
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
    row.className = 'child-row' + (child.id === childFollow ? ' followed' : '');
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
  const child = sessions.find((session) => session.id === childFollow);
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
  if (!id) return;
  if (childFollow === id) {
    childPanel.hidden = false;
    return;
  }
  closeChildPanel();
  childFollow = id;
  childStick = true;
  childAskBox = null;
  updateChildPanelDot();
  renderChildList();
  childPanel.hidden = false;
  // EventSource reconnects automatically; the child's durable frames carry
  // their seq as the SSE id, so the browser resumes with Last-Event-ID.
  childEs = new EventSource('/sessions/' + encodeURIComponent(id) + '/events');
  childEs.onmessage = (event) => {
    try {
      handleChildFrame(JSON.parse(event.data));
    } catch (error) {
      showStatus('child events: ' + error.message);
    }
  };
  childEs.onerror = () => {
    showStatus('child events: stream lost, reconnecting');
  };
}

// Collapsing leaves the session's view exactly as it was: the panel closes the
// child's stream and drops the lines it drew, and nothing else changes.
function closeChildPanel() {
  if (childEs) {
    childEs.close();
    childEs = null;
  }
  childFollow = null;
  childStick = true;
  childAskBox = null;
  childTranscript.textContent = '';
  childPanelTitle.textContent = '';
  updateChildPanelDot();
  childPanel.hidden = true;
  renderChildList();
}

// The panel's own frame handler: a child's durable messages render into the
// panel and nothing else does. The session-state frames — the header dot, the
// status label, the activity console, the ask composer — describe the session
// the pane is showing, so a child's frames never touch them.
function handleChildFrame(frame) {
  if (!frame.event || frame.event.kind !== 'message') return;
  // The renderers append to `out` and keep the ask record of the box they last
  // drew. A child's frame draws into the panel and with the panel's own record,
  // so its question and the frame that answers it stay one box there, and the
  // session's record and transcript are untouched.
  const previousOut = out;
  const previousAsk = openAskBox;
  setOut(childTranscript);
  setOpenAskBox(childAskBox);
  try {
    renderChildMessage(frame.event.message, frame.event.at_ms);
  } finally {
    setOut(previousOut);
    childAskBox = openAskBox;
    setOpenAskBox(previousAsk);
  }
}

// One durable message of the panel's child, drawn with the session's own block
// renderers but without its state: the child's live paragraph, its last message
// and its ask record are not the session's.
function renderChildMessage(message, atMs) {
  const block = message.block;
  if (message.role === 'assistant' && block.kind === 'text') {
    appendAssistant(block.text, atMs);
    return;
  }
  renderBlock(message, atMs);
}

// The panel's own follow flag: it stays at the child's newest line until the
// reader scrolls the panel up, and a render reads it through `scrollToBottom`.
childTranscript.addEventListener('scroll', () => {
  childStick =
    childTranscript.scrollTop + childTranscript.clientHeight >=
    childTranscript.scrollHeight - 40;
});

btnChildCollapse.addEventListener('click', closeChildPanel);
