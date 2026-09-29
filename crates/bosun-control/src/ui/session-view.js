// The session view: opening and closing a session, its header and actions
// sheet, and the frames of its event stream.

import {
  activityLog,
  askChips,
  askFree,
  askSheet,
  btnBack,
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
  input,
  inputRow,
  isNarrow,
  personaName,
  rowFork,
  rowInterrupt,
  rowPermission,
  rowPersona,
  rowStop,
  transcript,
  view,
  viewDir,
  viewFork,
  viewIdCopy,
  viewNode,
  viewPermission,
  viewSheet,
  viewSheetMeta,
  viewStateDot,
  viewTitle,
  viewWaiting,
  watchBanner,
} from './dom.js';
import { post, showStatus, toastError, toastOk } from './common.js';
import { personas } from './new-session.js';
import {
  MAX_ACTIVITIES,
  activities,
  appendActivityRow,
  refreshStatusLabel,
  renderActivityConsole,
  setActivities,
  updateStatusLabel,
} from './activity.js';
import { refreshSessions, sessions } from './session-list.js';
import { markListEntry, openSession } from './history.js';
import { askSyncTimer, loadDraft, saveDraft, setAskSyncTimer } from './composer.js';
import { setStick, syncBtnBottom } from './scroll.js';
import { scheduleVisualViewportSync } from './viewport.js';
import {
  appendDelta,
  appendLine,
  callArgs,
  modelCallLine,
  renderMessage,
  setLastMsg,
  setLiveEl,
  setOpenAskBox,
} from './transcript.js';
import { closeChildPanel } from './subagents.js';
import { TAIL_MESSAGES, setEarlier, startEarlier } from './earlier.js';

export { closeSession, coarsePointer, current, setViewState, showSession, viewState };

let current = null;   // the open session id
let viewState = null; // the open session's last known state
let statusTick = null; // the 1s header-label interval for the open session
let es = null;        // the open EventSource

// Another module cannot assign an imported binding, so it writes `viewState`
// through this.
function setViewState(value) {
  viewState = value;
}

// The view half of opening: the session's header, transcript and event stream,
// with the previous session's teardown first.
// A coarse pointer is a touch screen, and a touch screen refuses a focus the
// reader did not ask for: taking one when a session opens, or when a question
// arrives, is the focus that sometimes does not land and the one that can zoom
// the page. Programmatic focuses stay for a fine pointer, and every focus a tap
// reaches is left alone.
const coarsePointer = () => window.matchMedia('(pointer: coarse)').matches;

function showSession(id) {
  closeSession();
  current = id;
  coverHome(true);
  view.hidden = false;
  const session = sessions.find((s) => s.id === id);
  if (session) updateHeader(session);
  fetchSession(id);
  // EventSource reconnects automatically; durable frames carry the event seq
  // as their SSE id, so the browser resumes with Last-Event-ID.
  // The stream replays only the newest messages; older ones are read back as
  // the reader scrolls up. A long session would otherwise render its whole
  // history before the reader sees anything.
  es = new EventSource(
    '/sessions/' + encodeURIComponent(id) + '/events?tail=' + TAIL_MESSAGES
  );
  es.onmessage = (event) => {
    try {
      handleFrame(JSON.parse(event.data));
    } catch (error) {
      showStatus('events: ' + error.message);
    }
  };
  es.onerror = () => {
    showStatus('events: stream lost, reconnecting');
  };
  statusTick = window.setInterval(refreshStatusLabel, 1000);
  // The chat draft survives closing the session; restore it here so the
  // resumed session starts where the user left off.
  input.value = loadDraft();
  // Opening a session asks for the composer whatever the pointer: this is the
  // path a phone had before, and the keyboard comes up with the session.
  if (!inputRow.hidden) input.focus();
  // The keyboard that focus raises moves the visual viewport, and the numbers
  // settle after this turn: ask for a sync here rather than wait for the next
  // resize, so the view is at its height as the keyboard arrives.
  scheduleVisualViewportSync();
}

function closeSession() {
  // The call arguments are the transcript's: the DOM clears here, and a call
  // drawn in it cannot be answered after the session it was drawn in has gone.
  // A collapse keeps them, because a `message_child` drawn in the main
  // transcript can still be answered while the panel is closed.
  callArgs.clear();
  if (es) {
    es.close();
    es = null;
  }
  current = null;
  viewState = null;
  setLiveEl(null);
  setLastMsg(null);
  setActivities([]);
  activityLog.hidden = true;
  activityLog.textContent = '';
  setOpenAskBox(null);
  if (statusTick) window.clearInterval(statusTick);
  statusTick = null;
  if (askSyncTimer) window.clearTimeout(askSyncTimer);
  setAskSyncTimer(null);
  transcript.textContent = '';
  setEarlier({ before: null, more: false, loading: false });
  // The panel belongs to the session the pane is leaving: its child, its stream
  // and the lines it drew go with it.
  closeChildPanel();
  // The next session opens on its newest line, so re-arm auto-follow and hide
  // the control at this teardown.
  setStick(true);
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
  view.hidden = true;
  coverHome(false);
}

// The session list's scroll position while a session covers it: the list is
// out of the page then, and a browser drops the scroll of a box it removes.
let homeScroll = 0;
const homeColumn = document.querySelector('body > main');

function coverHome(covered) {
  const wasCovered = document.body.classList.contains('in-session');
  if (covered === wasCovered) return;
  if (covered) homeScroll = homeColumn.scrollTop;
  document.body.classList.toggle('in-session', covered);
  if (!covered) homeColumn.scrollTop = homeScroll;
}

// The header and the ⋯ sheet carry the open session's identity, and the footer
// and the sheet rows carry the shape a watch-only child gives the pane.
// Clearing them returns the pane's no-session layout, so a session the pane
// cannot read yet shows nothing of the session before it.
function clearHeader() {
  viewStateDot.className = 'dot';
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
  rowInterrupt.hidden = false;
  rowStop.hidden = false;
  viewFork.textContent = '';
}

async function fetchSession(id) {
  try {
    const response = await fetch('/sessions/' + encodeURIComponent(id));
    // The pane may have left this session while the fetch was in flight: a
    // late reply must not write the header, or close the view, of whichever
    // session the pane shows now.
    if (current !== id) return;
    // The control plane has no such session: the entry the pane is on stops
    // naming it, so the session screen does not outlive the session and a
    // reload does not open it again.
    if (response.status === 404) {
      showStatus('session ' + id + ' ended');
      markListEntry();
      closeSession();
      return;
    }
    if (!response.ok) throw new Error('HTTP ' + response.status);
    const session = await response.json();
    if (current !== id) return;
    updateHeader(session);
  } catch (error) {
    // A failure for a session the pane has left is not this screen's to
    // report.
    if (current !== id) return;
    showStatus('session: ' + error.message);
  }
}

function updateHeader(session) {
  viewStateDot.className = 'dot ' + session.state;
  viewState = session.state;
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
  rowInterrupt.hidden = watchOnly;
  rowStop.hidden = watchOnly;
}

function updatePermissionBadge(permission) {
  viewPermission.textContent = permission;
  btnPermission.textContent =
    permission === 'read_only' ? 'Switch to read-write' : 'Switch to read-only';
}

// Forking copies the session's conversation into a new root session with its
// own working copy, and the pane opens the fork: it is a session like any other
// from here on. A refusal — no recorded repository, a child, a session that is
// not waiting for input — is shown in the sheet, which is where the control is.
btnFork.addEventListener('click', async () => {
  viewFork.textContent = '';
  // The clone takes seconds, and a second click would start a second fork: the
  // control is disabled until this one answers.
  btnFork.disabled = true;
  // The clone takes seconds, and the reader may leave the session in them: the
  // fork is theirs either way, but the pane only jumps to it if they are still
  // here.
  const started = current;
  try {
    const response = await post('/sessions/' + encodeURIComponent(started) + '/fork', {});
    const fork = await response.json();
    await refreshSessions();
    // The reader may have left while the clone ran: the fork is in the list
    // either way, and the pane only acts on a session it is still showing.
    if (current !== started) return;
    viewSheet.hidden = true;
    openSession(fork.id);
  } catch (error) {
    if (current === started) viewFork.textContent = 'fork: ' + error.message;
  } finally {
    btnFork.disabled = false;
  }
});

btnInterrupt.addEventListener('click', async () => {
  try {
    await post('/sessions/' + encodeURIComponent(current) + '/interrupt', {});
  } catch (error) {
    toastError('interrupt: ' + error.message);
  }
});

btnPermission.addEventListener('click', async () => {
  const next = viewPermission.textContent === 'read_only' ? 'read_write' : 'read_only';
  try {
    await post('/sessions/' + encodeURIComponent(current) + '/permission', {
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
  try {
    await post('/sessions/' + encodeURIComponent(current) + '/persona', { persona: name });
    personaName.value = '';
    await fetchSession(current);
  } catch (error) {
    toastError('persona: ' + error.message);
  }
});

btnStop.addEventListener('click', async () => {
  if (!window.confirm('Stop and delete this session?')) return;
  const id = current;
  try {
    await post('/stop', { session_id: id });
    toastOk('session stopped');
    // The pane may have left this session while the stop was in flight, so the
    // close belongs only to a screen still showing the session that stopped.
    if (current === id) {
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

// On desktop the actions are a popup: clicking anywhere else, or Escape,
// dismisses it. Mobile keeps the full bottom sheet, which has its own ✕.
document.addEventListener('click', (event) => {
  if (viewSheet.hidden || isNarrow()) return;
  if (viewSheet.contains(event.target) || event.target === btnMore) return;
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
  const owner = sessions.find((session) => session.id === current);
  const ownerId = owner && owner.owner_id ? owner.owner_id : null;
  openSession(ownerId || current);
});

function handleEvent(event) {
  switch (event.kind) {
    case 'message':
      renderMessage(event.message, event.at_ms);
      break;
    case 'state':
      // The current state lives once, in the header dot; a transcript line
      // would repeat what is already on screen.
      viewStateDot.className = 'dot ' + event.state;
      viewState = event.state;
      if (current) updateStatusLabel({ id: current, state: event.state });
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
      appendLine('mono', modelCallLine(event), event.at_ms);
      break;
    case 'activity': {
      // Loop activity is machinery, not conversation: it feeds the debug
      // console and the phase indicator, and never the transcript. The local
      // receipt time is stamped for the elapsed counter.
      event.received = Date.now();
      const previous = activities[activities.length - 1];
      activities.push(event);
      if (activities.length > MAX_ACTIVITIES) {
        activities.shift();
        // The console mirrors the capped buffer, so drop the oldest row too.
        if (activityLog.firstChild) activityLog.removeChild(activityLog.firstChild);
      }
      if (!activityLog.hidden) {
        appendActivityRow(event, previous);
        activityLog.scrollTop = activityLog.scrollHeight;
      }
      if (current && viewState) updateStatusLabel({ id: current, state: viewState });
      break;
    }
    case 'warning':
      // A durable note the loop recorded, such as a selected MCP server being
      // unavailable.
      appendLine('warning', event.text);
      break;
  }
}

function handleFrame(frame) {
  if (Object.prototype.hasOwnProperty.call(frame, 'delta')) {
    appendDelta(frame.delta);
    return;
  }
  if (frame.history) {
    startEarlier(frame.history);
    return;
  }
  if (frame.event) handleEvent(frame.event);
}
