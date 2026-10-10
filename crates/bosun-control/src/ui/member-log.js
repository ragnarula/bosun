// A crew member's Log: the member's own transcript, on its own event stream,
// drawn by the transcript's block renderers.

import { $ } from './dom.js';
import { showStatus } from './common.js';
import { opened } from './session-view.js';
import { openMember } from './history.js';
import { clip, drawAssistant, drawBlock } from './transcript.js';

export { childName, closeMemberLog, followMemberLog, watchControl };

const memberLog = $('member-log');

/// How wide a child's name may be before it is cut: one line of a row.
const CHILD_NAME_MAX = 60;

// What a child is called in the transcript: its own one-line summary once its
// model has written one, otherwise the first line of the instructions it was
// spawned with. The id is the last resort.
function childName(child) {
  if (!child) return '';
  if (child.summary) return clip(child.summary, CHILD_NAME_MAX);
  const first = (child.prompt || '')
    .split('\n')
    .map((line) => line.trim())
    .find((line) => line);
  return first ? clip(first, CHILD_NAME_MAX) : child.id;
}

// The control that opens a child's screen, wherever the transcript names one.
function watchControl(childId) {
  const watch = document.createElement('button');
  watch.type = 'button';
  watch.className = 'child-watch';
  watch.textContent = 'watch';
  watch.title = 'open child ' + childId;
  watch.addEventListener('click', (event) => {
    // The line it sits in may expand on a tap of its own.
    event.stopPropagation();
    openMember(childId);
  });
  return watch;
}

// Follows `id`'s transcript in the member screen's Log. The previous member's
// stream closes first, so one member's Log is followed at a time. The open
// session's `memberLog` is the followed member's state, or null.
function followMemberLog(id) {
  const s = opened;
  if (!id || !s) return;
  if (s.memberLog && s.memberLog.id === id) return;
  closeMemberLog();
  // The Log has its own follow flag and its own record for the block
  // renderers, so a member's question and the frame that answers it are one
  // box there and the session's record is untouched.
  const log = { id, es: null, stick: true, askBox: null, callArgs: new Map() };
  s.memberLog = log;
  // EventSource reconnects automatically; the member's durable frames carry
  // their seq as the SSE id, so the browser resumes with Last-Event-ID.
  log.es = new EventSource('/sessions/' + encodeURIComponent(id) + '/events');
  log.es.onmessage = (event) => {
    if (opened !== s || s.memberLog !== log) return;
    try {
      handleFrame(log, JSON.parse(event.data));
    } catch (error) {
      showStatus('member events: ' + error.message);
    }
  };
  log.es.onerror = () => {
    if (opened === s && s.memberLog === log) showStatus('member events: stream lost, reconnecting');
  };
}

function closeMemberLog() {
  if (opened && opened.memberLog) {
    opened.memberLog.es.close();
    opened.memberLog = null;
  }
  memberLog.textContent = '';
}

// Only the member's durable messages draw here. Its state, activity and live
// text describe a session the header does not show, so they are left out.
function handleFrame(log, frame) {
  if (!frame.event || frame.event.kind !== 'message') return;
  const message = frame.event.message;
  const atMs = frame.event.at_ms;
  const drawn = message.role === 'assistant' && message.block.kind === 'text'
    ? drawAssistant(message.block.text, atMs)
    : drawBlock(message, atMs, log);
  if (drawn) memberLog.appendChild(drawn);
  followLog();
}

// The Log stays at the member's newest line until the reader scrolls it up.
function followLog() {
  const log = opened && opened.memberLog;
  if (log && log.stick) memberLog.scrollTop = memberLog.scrollHeight;
}

memberLog.addEventListener('scroll', () => {
  const log = opened && opened.memberLog;
  if (!log) return;
  log.stick = memberLog.scrollTop + memberLog.clientHeight >= memberLog.scrollHeight - 40;
});
// A tap that opens a line makes the Log taller.
memberLog.addEventListener('click', followLog);
