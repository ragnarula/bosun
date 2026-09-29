// The open session's status label and its activity console.

import { activityLog, viewWaiting } from './dom.js';
import { sessions } from './session-list.js';
import { opened } from './session-view.js';

export {
  MAX_ACTIVITIES,
  appendActivityRow,
  liveChildrenLabel,
  refreshStatusLabel,
  renderActivityConsole,
  updateStatusLabel,
};

// The open session's loop-activity frames, its `activities`, feed the debug
// console and the status indicator. Activity never enters the transcript.
// The buffer is capped and drops the oldest; the store holds the full record.
const MAX_ACTIVITIES = 5000;

// The count of a session's direct children that can still act. A child is
// live for display when its state is not stopped.
function liveChildCount(session) {
  return sessions.filter(
    (s) => s.parent_id === session.id && s.state !== 'stopped'
  ).length;
}

// The visible label for a loop-activity phase. Activity detail fields flatten
// beside `phase` on the wire. `wake_dropped` has no phase to show, so the
// caller falls back to the state name. Mirrors the terminal client's labels.
function phaseLabel(activity) {
  switch (activity.phase) {
    case 'wake_started': return 'working';
    case 'request_sent': return 'awaiting model';
    case 'first_token': return 'receiving reply';
    case 'response_complete': return 'processing reply';
    case 'tool_started': return 'running tool ' + activity.name;
    case 'tool_finished': return 'ran tool ' + activity.name;
    case 'empty_retry':
      return 'retrying empty reply (' + activity.attempt + '/' + activity.limit + ')';
    case 'compaction_started': return 'compacting';
    case 'compaction_finished': return 'finishing compaction';
    default: return null;
  }
}

// The running status text: the newest phase label plus the elapsed seconds
// counted from when the frame arrived locally. The wire `at_ms` orders the
// history; clock skew must not distort the live counter.
function runningStatus() {
  if (!opened) return null;
  const newest = opened.activities[opened.activities.length - 1];
  if (!newest) return null;
  const label = phaseLabel(newest);
  if (!label) return null;
  const elapsed = Math.max(0, Math.floor((Date.now() - newest.received) / 1000));
  return label + ' · ' + elapsed + 's';
}

// The phase's detail, mirroring the terminal console's text.
function phaseDetail(activity) {
  switch (activity.phase) {
    case 'wake_started': return '';
    case 'wake_dropped': return activity.reason;
    case 'request_sent': return activity.model + ' via ' + activity.provider;
    case 'first_token': return activity.latency_ms + 'ms';
    case 'response_complete': return activity.stop_reason;
    case 'tool_started': return activity.name;
    case 'tool_finished':
      return activity.name + ' ' + (activity.ok ? 'ok' : 'failed') + ' ' + activity.elapsed_ms + 'ms';
    case 'empty_retry':
      return activity.attempt + '/' + activity.limit + ' ' + activity.reason;
    case 'compaction_started': return activity.input_tokens + ' in';
    case 'compaction_finished': return activity.retired_messages + ' retired';
    default: return '';
  }
}

// A duration for one console row: milliseconds under a second, whole seconds
// above it, so the row stays short.
function formatDuration(ms) {
  return ms < 1000 ? ms + 'ms' : Math.floor(ms / 1000) + 's';
}

// The console's phase name. The status hides `wake_dropped`, but the console
// lists every recorded phase, so a dropped wake still gets a name.
function activityLabel(activity) {
  return phaseLabel(activity) || 'wake dropped';
}

// One monospace row per activity: the phase label, its detail, and the time
// since the previous row derived from `at_ms`. The first row has no previous
// row, so it carries no duration.
function appendActivityRow(activity, previous) {
  const row = document.createElement('div');
  row.className = 'activity-row';
  const label = document.createElement('span');
  label.className = 'activity-label';
  label.textContent = activityLabel(activity);
  row.appendChild(label);
  const detail = phaseDetail(activity);
  if (detail) {
    const detailEl = document.createElement('span');
    detailEl.className = 'activity-detail';
    detailEl.textContent = detail;
    row.appendChild(detailEl);
  }
  if (previous) {
    const duration = document.createElement('span');
    duration.className = 'activity-duration';
    duration.textContent = '+' + formatDuration(Math.max(0, activity.at_ms - previous.at_ms));
    row.appendChild(duration);
  }
  activityLog.appendChild(row);
}

function renderActivityConsole() {
  activityLog.textContent = '';
  if (!opened) return;
  const activities = opened.activities;
  activities.forEach((activity, index) => {
    appendActivityRow(activity, index > 0 ? activities[index - 1] : null);
  });
  activityLog.scrollTop = activityLog.scrollHeight;
}

// A waiting session with live children announces that it is waiting for
// them; a running session names its newest loop phase when one is known; any
// other state is named by its dot alone. The freshest list wins, so a child's
// report is reflected here between polls. A caller with no live phase (the
// session-list row) passes none and gets the plain state.
function liveChildrenLabel(session, running = null) {
  if (session.state === 'running' && running) return running;
  const count = liveChildCount(session);
  return session.state === 'waiting_for_input' && count > 0
    ? 'waiting for children (' + count + ')'
    : session.state;
}

// Writes the header's state label. The state dot names the state; this span
// carries the waiting-for-children or running-phase text, and hides when the
// state alone says it.
function updateStatusLabel(session) {
  const running = runningStatus();
  const label = liveChildrenLabel(session, running);
  viewWaiting.hidden = label === session.state;
  viewWaiting.textContent = label;
  viewWaiting.classList.toggle('running', session.state === 'running' && !!running);
}

// The elapsed counter advances once a second while a session is open; the
// state events and the session poll alone would leave it stale.
function refreshStatusLabel() {
  if (opened && opened.state) updateStatusLabel({ id: opened.id, state: opened.state });
}
