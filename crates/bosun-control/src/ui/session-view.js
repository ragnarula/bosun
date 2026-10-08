// The session view: opening and closing a session, its header and actions
// sheet, and the frames of its event stream.

import {
  activityLog,
  askChips,
  askFree,
  askSheet,
  btnBack,
  btnClear,
  btnCopyId,
  btnFork,
  btnInterrupt,
  btnMore,
  btnPermission,
  btnPersona,
  btnSheetClose,
  btnStop,
  btnWatchOpen,
  chatRow,
  chipPermission,
  chipPermissionText,
  chipPersona,
  input,
  inputRow,
  isNarrow,
  personaName,
  rowClear,
  rowFork,
  rowPermission,
  rowPersona,
  rowStop,
  transcript,
  view,
  viewClear,
  viewDir,
  viewFork,
  viewIdCopy,
  viewNode,
  viewPermission,
  viewSheet,
  viewSheetMeta,
  viewStateDot,
  viewSummary,
  viewTitle,
  viewWaiting,
  watchBanner,
} from './dom.js';
import { post, showStatus, toastError, toastOk } from './common.js';
import { personas } from './new-session.js';
import {
  MAX_ACTIVITIES,
  appendActivityRow,
  refreshStatusLabel,
  renderActivityConsole,
  updateStatusLabel,
} from './activity.js';
import { refreshSessions, sessions } from './session-list.js';
import { markListEntry, openSession } from './history.js';
import { loadDraft, saveDraft, scheduleAskSync } from './composer.js';
import { follow, syncBtnBottom } from './scroll.js';
import { leaveScreen, showScreen } from './screens.js';
import {
  drawAssistant,
  drawBlock,
  drawLine,
  drawLiveParagraph,
  modelCallLine,
} from './transcript.js';
import { closeChildPanel } from './subagents.js';
import { TAIL_MESSAGES, startEarlier } from './earlier.js';
import { closeCrew, openCrew, personaChipContent, renderCrew } from './crew.js';
import { updateProjectLink } from './project-map.js';
import { closeNotifications } from './device.js';

export { closeSession, coarsePointer, opened, showSession };

// The open session, or null while the list shows. Everything the pane holds
// for one open session is a field of this object, so closing a session drops
// all of it at once and the next session starts from a new one. Other modules
// write its fields; only this module replaces it. A reply that arrives after an
// await compares the object it started with against this one, so a reply for a
// session the pane has left does nothing.
let opened = null;

function newSessionState(id) {
  return {
    id,
    // The session's last known state, from the list, the detail fetch or the
    // stream.
    state: null,
    // The session's event stream, and the one-second timer that advances the
    // header's elapsed count.
    es: null,
    statusTick: null,
    // Whether the transcript follows its newest line.
    stick: true,
    // The assistant paragraph live deltas stream into.
    liveEl: null,
    // The last durable message block drawn: it decides whether a question is
    // live on screen.
    lastMsg: null,
    // The transcript's record for the block renderers: its latest question's
    // box and the arguments of its unanswered `message_child` calls.
    askBox: null,
    callArgs: new Map(),
    // Loop-activity frames, for the console and the status label.
    activities: [],
    // The timer that settles the ask composer after a burst of messages.
    askSyncTimer: null,
    // The older part of the transcript, read back a page at a time: the seq
    // the next page ends before, whether the session has more, whether a read
    // is in flight, and the row at the transcript's top that asks for it.
    earlier: { before: null, more: false, loading: false, row: null },
    // The child the subagent panel follows, or null while the panel is closed.
    panel: null,
    // The crew: the session's whole tree on one stream, drawn in the Chat,
    // Tasks and Files views; null until the session's tree root is known.
    crew: null,
  };
}

// Stops what the session's object started: its stream, its timers, and the
// stream of the child its panel follows.
function stopSession(s) {
  if (s.es) s.es.close();
  closeCrew(s);
  if (s.panel) s.panel.es.close();
  window.clearInterval(s.statusTick);
  window.clearTimeout(s.askSyncTimer);
}

// A coarse pointer is a touch screen, and a touch screen refuses a focus the
// reader did not ask for: taking one when a session opens, or when a question
// arrives, is the focus that sometimes does not land and the one that can zoom
// the page. Programmatic focuses stay for a fine pointer, and every focus a tap
// reaches is left alone.
const coarsePointer = () => window.matchMedia('(pointer: coarse)').matches;

// The view half of opening: the session's header, transcript and event stream,
// with the previous session's teardown first.
function showSession(id) {
  closeSession();
  const s = newSessionState(id);
  opened = s;
  showScreen(view);
  const session = sessions.find((listed) => listed.id === id);
  if (session) {
    updateHeader(session);
    openCrew(s, session.owner_id || session.id);
  }
  // The reader has the tree on screen, so its notification has done its job.
  closeNotifications(session ? session.owner_id || session.id : id);
  updateProjectLink();
  fetchSession(s);
  // EventSource reconnects automatically; durable frames carry the event seq
  // as their SSE id, so the browser resumes with Last-Event-ID.
  // The stream replays only the newest messages; older ones are read back as
  // the reader scrolls up. A long session would otherwise render its whole
  // history before the reader sees anything.
  s.es = new EventSource(
    '/sessions/' + encodeURIComponent(id) + '/events?tail=' + TAIL_MESSAGES
  );
  s.es.onmessage = (event) => {
    if (opened !== s) return;
    try {
      handleFrame(s, JSON.parse(event.data));
    } catch (error) {
      showStatus('events: ' + error.message);
    }
  };
  s.es.onerror = () => {
    if (opened === s) showStatus('events: stream lost, reconnecting');
  };
  s.statusTick = window.setInterval(refreshStatusLabel, 1000);
  // The chat draft survives closing the session; restore it here so the
  // resumed session starts where the user left off.
  input.value = loadDraft();
  // Opening a session asks for the composer whatever the pointer: this is the
  // path a phone had before, and the keyboard comes up with the session.
  if (!inputRow.hidden) input.focus();
}

function closeSession() {
  if (opened) stopSession(opened);
  opened = null;
  activityLog.hidden = true;
  activityLog.textContent = '';
  transcript.textContent = '';
  // The panel belongs to the session the pane is leaving: its lines go with it.
  closeChildPanel();
  // The next session opens on its newest line, so the control that returns
  // there goes away with this one.
  syncBtnBottom();
  saveDraft();
  input.value = '';
  askSheet.hidden = true;
  askChips.textContent = '';
  askFree.hidden = true;
  chatRow.hidden = false;
  // The ⋯ sheet is a sibling of #session-view, so hiding the view does not
  // hide it.
  viewSheet.hidden = true;
  clearHeader();
  leaveScreen(view);
}

// The header and the ⋯ sheet carry the open session's identity, and the footer
// and the sheet rows carry the shape a watch-only child gives the pane.
// Clearing them returns the pane's no-session layout, so a session the pane
// cannot read yet shows nothing of the session before it.
function clearHeader() {
  viewStateDot.className = 'dot';
  viewSummary.textContent = '';
  chipPersona.textContent = '';
  chipPermissionText.textContent = '';
  btnInterrupt.hidden = true;
  viewNode.textContent = '';
  viewDir.textContent = '';
  viewIdCopy.textContent = '';
  viewSheetMeta.textContent = '';
  viewWaiting.textContent = '';
  viewWaiting.hidden = true;
  viewPermission.textContent = '';
  btnPermission.textContent = 'Switch to read-only';
  personaName.value = '';
  inputRow.hidden = false;
  watchBanner.hidden = true;
  rowPermission.hidden = false;
  rowPersona.hidden = false;
  rowFork.hidden = false;
  rowClear.hidden = false;
  rowStop.hidden = false;
  // The sheet's controls come back live with their rows: a request the reader
  // left behind is nobody's to answer, so it must not hold a control off, or
  // leave its note, on the session the pane shows next.
  btnFork.disabled = false;
  btnClear.disabled = false;
  viewFork.textContent = '';
  viewClear.textContent = '';
}

async function fetchSession(s) {
  try {
    const response = await fetch('/sessions/' + encodeURIComponent(s.id));
    // The pane may have left this session while the fetch was in flight: a
    // late reply must not write the header, or close the view, of whichever
    // session the pane shows now.
    if (opened !== s) return;
    // The control plane has no such session: the entry the pane is on stops
    // naming it, so the session screen does not outlive the session and a
    // reload does not open it again.
    if (response.status === 404) {
      showStatus('session ' + s.id + ' ended');
      markListEntry();
      closeSession();
      return;
    }
    if (!response.ok) throw new Error('HTTP ' + response.status);
    const session = await response.json();
    if (opened !== s) return;
    updateHeader(session);
    if (!s.crew) openCrew(s, session.owner_id || session.id);
  } catch (error) {
    // A failure for a session the pane has left is not this screen's to
    // report.
    if (opened !== s) return;
    showStatus('session: ' + error.message);
  }
}

function updateHeader(session) {
  viewStateDot.className = 'dot ' + session.state;
  opened.state = session.state;
  // The session's own one-line description names it; until its model writes
  // one, the node does.
  viewSummary.textContent = session.summary || session.node;
  chipPersona.textContent = '';
  if (session.persona) chipPersona.appendChild(personaChipContent(session.persona));
  viewNode.textContent = session.node;
  viewDir.textContent = session.dir;
  viewIdCopy.textContent = session.id;
  updateStatusLabel(session);
  updatePermissionBadge(session.permission);
  // Model and persona are session facts, shown once in the ⋯ sheet; the
  // transcript never repeats them as separator lines.
  viewSheetMeta.textContent = [session.persona, session.model]
    .filter((part) => part)
    .join(' · ');
  // The switch dropdown preselects the session's current persona, so the
  // "switch persona" action reads as a dropdown rather than a blank field.
  if (session.persona && personas.some((persona) => persona.name === session.persona)) {
    personaName.value = session.persona;
  } else {
    personaName.value = '';
  }
  // A child session is watch-only: its transcript and state render live, but
  // the pane offers no input path and no user actions toward it. The input
  // row is replaced by an "Open to act" banner, and the ⋯ sheet's control
  // rows are disabled — the API would refuse them anyway, so they must not
  // look actionable from a child.
  const watchOnly = !!session.parent_id;
  inputRow.hidden = watchOnly;
  watchBanner.hidden = !watchOnly;
  rowPermission.hidden = watchOnly;
  rowPersona.hidden = watchOnly;
  rowFork.hidden = watchOnly;
  rowClear.hidden = watchOnly;
  rowStop.hidden = watchOnly;
  chipPersona.hidden = watchOnly || !session.persona;
  chipPermission.hidden = watchOnly;
  syncStopButton(session.state, watchOnly);
}

// Stop is in the header while the crew works, where the thumb finds it at
// once; with nothing running there is nothing to stop.
function syncStopButton(state, watchOnly) {
  const running = state === 'running' || state === 'creating';
  btnInterrupt.hidden = !!watchOnly || !running;
}

function updatePermissionBadge(permission) {
  viewPermission.textContent = permission;
  chipPermissionText.textContent = permission === 'read_only' ? 'read-only' : 'read-write';
  btnPermission.textContent =
    permission === 'read_only' ? 'Switch to read-write' : 'Switch to read-only';
}

// Forking copies the session's conversation into a new root session with its
// own working copy, and the pane opens the fork: it is a session like any other
// from here on. A refusal — no recorded repository, a child, a session that is
// not waiting for input — is shown in the sheet, which is where the control is.
btnFork.addEventListener('click', async () => {
  // The clone takes seconds, and a second click would start a second fork: the
  // control is disabled until this one answers, and the note says what it is
  // waiting for.
  viewFork.textContent = 'forking…';
  btnFork.disabled = true;
  // The clone takes seconds, and the reader may leave the session in them: the
  // fork is theirs either way, but the pane only jumps to it if they are still
  // here.
  const started = opened;
  try {
    const response = await post('/sessions/' + encodeURIComponent(started.id) + '/fork', {});
    const fork = await response.json();
    await refreshSessions();
    // The reader may have left while the clone ran: the fork is in the list
    // either way, and the pane only acts on a session it is still showing.
    if (opened !== started) return;
    viewSheet.hidden = true;
    openSession(fork.id);
  } catch (error) {
    if (opened === started) viewFork.textContent = 'fork: ' + error.message;
  } finally {
    // The control belongs to the screen this request was made for; a later
    // screen's is the teardown's to set.
    if (opened === started) btnFork.disabled = false;
  }
});

// Clearing cuts the session's thread at a durable marker and starts the model
// fresh from the next message. The transcript keeps everything, so the pane
// draws nothing itself: the session's own stream carries the divider.
btnClear.addEventListener('click', async () => {
  if (
    !window.confirm(
      "Clear this session's context? The transcript keeps everything; the model starts fresh."
    )
  ) {
    return;
  }
  viewClear.textContent = 'clearing…';
  // The clear is one store write, but a second click would post a second one:
  // the control is disabled until this one answers, and the note says what it
  // is waiting for.
  btnClear.disabled = true;
  // The reader may leave the session while the write is in flight: the sheet
  // and the toast belong only to a screen still showing the session it was for.
  const started = opened;
  try {
    await post('/sessions/' + encodeURIComponent(started.id) + '/clear', {});
    if (opened !== started) return;
    // The request answered, so there is nothing to wait for: the note goes
    // with the sheet, and the toast is what reports the clear.
    viewClear.textContent = '';
    viewSheet.hidden = true;
    toastOk('context cleared');
  } catch (error) {
    // A refusal — a running session, one still being created, a pending
    // question — is shown where the control is, in the sheet.
    if (opened === started) viewClear.textContent = 'clear: ' + error.message;
  } finally {
    // Only the screen this request was made for takes its control back: the
    // next screen's state is the teardown's, and an old attempt's answer must
    // not re-enable a control a new attempt has just turned off.
    if (opened === started) btnClear.disabled = false;
  }
});

btnInterrupt.addEventListener('click', async () => {
  try {
    await post('/sessions/' + encodeURIComponent(opened.id) + '/interrupt', {});
  } catch (error) {
    toastError('interrupt: ' + error.message);
  }
});

btnPermission.addEventListener('click', async () => {
  const next = viewPermission.textContent === 'read_only' ? 'read_write' : 'read_only';
  try {
    await post('/sessions/' + encodeURIComponent(opened.id) + '/permission', {
      permission: next,
    });
  } catch (error) {
    toastError('permission: ' + error.message);
  }
});

btnPersona.addEventListener('click', async () => {
  const name = personaName.value;
  if (!name) {
    toastError('persona: pick a persona first');
    return;
  }
  const s = opened;
  try {
    await post('/sessions/' + encodeURIComponent(s.id) + '/persona', { persona: name });
    personaName.value = '';
    await fetchSession(s);
  } catch (error) {
    toastError('persona: ' + error.message);
  }
});

btnStop.addEventListener('click', async () => {
  if (!window.confirm('Stop and delete this session?')) return;
  const s = opened;
  try {
    await post('/stop', { session_id: s.id });
    toastOk('session stopped');
    // The pane may have left this session while the stop was in flight, so the
    // close belongs only to a screen still showing the session that stopped.
    if (opened === s) {
      markListEntry();
      closeSession();
    }
    await refreshSessions();
  } catch (error) {
    toastError('stop: ' + error.message);
  }
});

// The thin header's back arrow takes the same step the browser's back button
// takes, so the list is the screen it returns to.
btnBack.addEventListener('click', () => history.back());

// The status/phase line toggles the debug console. Opening it replays the
// buffered activity rows, so a session watched after the fact shows history.
viewTitle.addEventListener('click', () => {
  activityLog.hidden = !activityLog.hidden;
  if (!activityLog.hidden) renderActivityConsole();
});

btnMore.addEventListener('click', () => {
  viewSheet.hidden = false;
});

// On a wide screen the header carries the persona and permission: the
// permission chip switches it at once, and the persona chip opens the sheet at
// its picker.
chipPermission.addEventListener('click', () => btnPermission.click());
chipPersona.addEventListener('click', () => {
  viewSheet.hidden = false;
  personaName.focus();
});

// On desktop the actions are a popup: clicking anywhere else, or Escape,
// dismisses it. Mobile keeps the full bottom sheet, which has its own ✕.
document.addEventListener('click', (event) => {
  if (viewSheet.hidden || isNarrow()) return;
  if (viewSheet.contains(event.target) || btnMore.contains(event.target)) return;
  if (chipPersona.contains(event.target)) return;
  viewSheet.hidden = true;
});

document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !viewSheet.hidden && !isNarrow()) {
    viewSheet.hidden = true;
  }
});

btnSheetClose.addEventListener('click', () => {
  viewSheet.hidden = true;
});

btnCopyId.addEventListener('click', async () => {
  if (!viewIdCopy.textContent) return;
  try {
    await navigator.clipboard.writeText(viewIdCopy.textContent);
    toastOk('session id copied');
  } catch (error) {
    toastError('copy failed: ' + error.message);
  }
});

// A watch-only child can only be watched; acting on it means opening the
// tree owner, which is the only session in the tree that accepts input.
btnWatchOpen.addEventListener('click', () => {
  const owner = sessions.find((session) => session.id === opened.id);
  const ownerId = owner && owner.owner_id ? owner.owner_id : null;
  openSession(ownerId || opened.id);
});

// One durable message of the open session. The session's own state moves with
// it: the last message, the live paragraph it replaces, the ask record and the
// ask composer.
function drawMessage(s, message, atMs) {
  const block = message.block;
  s.lastMsg = block;
  // Any message that is not itself the pending ask closes the previous ask's
  // record: it is history once the stream has moved past it.
  if (block.kind !== 'ask') s.askBox = null;
  if (message.role === 'assistant' && block.kind === 'text') {
    const reply = drawAssistant(block.text, atMs);
    if (s.liveEl) {
      s.liveEl.replaceWith(reply);
      s.liveEl = null;
    } else {
      transcript.appendChild(reply);
    }
  } else {
    // A durable block supersedes the streamed live paragraph. Text replaces it
    // above; any other kind held tool output or reasoning, not assistant
    // prose, so the paragraph is dropped rather than left in the transcript.
    if (s.liveEl) {
      s.liveEl.remove();
      s.liveEl = null;
    }
    const drawn = drawBlock(message, atMs, s);
    if (drawn) transcript.appendChild(drawn);
  }
  follow();
  scheduleAskSync();
}

// Live deltas stream into one assistant paragraph until the turn's durable
// text arrives or another message begins.
function drawDelta(s, text) {
  if (!s.liveEl) {
    s.liveEl = drawLiveParagraph();
    transcript.appendChild(s.liveEl);
  }
  s.liveEl.textContent += text;
  follow();
}

function handleEvent(s, event) {
  switch (event.kind) {
    case 'message':
      drawMessage(s, event.message, event.at_ms);
      break;
    case 'state':
      // The current state lives once, in the header dot; a transcript line
      // would repeat what is already on screen.
      viewStateDot.className = 'dot ' + event.state;
      s.state = event.state;
      updateStatusLabel({ id: s.id, state: event.state });
      syncStopButton(event.state, !watchBanner.hidden);
      renderCrew();
      break;
    case 'permission':
      updatePermissionBadge(event.permission);
      break;
    case 'persona':
      // The persona lives once, in the ⋯ sheet's metadata row; nothing is
      // added to the transcript.
      break;
    case 'model_call':
      // Model calls are metering history, not conversation: the line is
      // transcript bookkeeping, never a message or an activity-console row.
      transcript.appendChild(drawLine('mono', modelCallLine(event), event.at_ms));
      follow();
      break;
    case 'activity': {
      // Loop activity is machinery, not conversation: it feeds the debug
      // console and the phase indicator, and never the transcript. The local
      // receipt time is stamped for the elapsed counter.
      event.received = Date.now();
      const previous = s.activities[s.activities.length - 1];
      s.activities.push(event);
      if (s.activities.length > MAX_ACTIVITIES) {
        s.activities.shift();
        // The console mirrors the capped buffer, so drop the oldest row too.
        if (activityLog.firstChild) activityLog.removeChild(activityLog.firstChild);
      }
      if (!activityLog.hidden) {
        appendActivityRow(event, previous);
        activityLog.scrollTop = activityLog.scrollHeight;
      }
      if (s.state) updateStatusLabel({ id: s.id, state: s.state });
      break;
    }
    case 'warning':
      // A durable note the loop recorded, such as a selected MCP server being
      // unavailable.
      transcript.appendChild(drawLine('warning', event.text));
      follow();
      break;
  }
}

function handleFrame(s, frame) {
  if (Object.prototype.hasOwnProperty.call(frame, 'delta')) {
    drawDelta(s, frame.delta);
    return;
  }
  if (frame.history) {
    startEarlier(s, frame.history);
    return;
  }
  if (frame.event) handleEvent(s, frame.event);
}
