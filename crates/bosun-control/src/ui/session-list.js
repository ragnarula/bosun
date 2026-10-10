// Home's session list, one card per session tree, and the poll that
// refreshes it.

import { $, sessionListEl, view, viewStateDot } from './dom.js';
import { ago, showStatus } from './common.js';
import { updateStatusLabel } from './activity.js';
import { markListEntry, openSession } from './history.js';
import { closeSession, opened } from './session-view.js';
import { activityCaption, avatarOf, renderCrew, treeMembers } from './crew.js';
import { memberName, personaLabel } from './signal.js';
import { setBadge } from './device.js';

export { refreshSessions, sessions };

let sessions = [];

// The groups Home shows, in this order: trees with a question for the reader,
// trees with a member at work, and the rest.
const GROUPS = [
  ['needs-you', 'Needs you'],
  ['working', 'Working'],
  ['idle', 'Idle'],
];

// Which group a tree belongs in. The root carries the reader's questions: a
// child's question reaches the reader through it.
function treeGroup(root, members) {
  if (root.asking) return 'needs-you';
  if (members.some((member) => member.state === 'running' || member.state === 'creating')) {
    return 'working';
  }
  return 'idle';
}

// The tree's newest activity, by whichever member did it.
function newestActivity(members) {
  let newest = null;
  for (const member of members) {
    if (member.activity && (!newest || member.activity.at_ms > newest.activity.at_ms)) newest = member;
  }
  return newest;
}

function formatCost(cost) {
  return '$' + cost.toFixed(2);
}

// One card per tree: the crew's faces, what the tree is for, one line on what
// is happening now, its task progress, and where it runs and what it has cost.
// The now line takes the place of the state word: the card never shows both.
function sessionCard(root, members, group) {
  const card = document.createElement('button');
  card.type = 'button';
  card.className = 'bs-session session-row' + (group === 'needs-you' ? ' is-needs-you' : '');
  card.dataset.session = root.id;
  if (opened && (opened.id === root.id || members.some((member) => member.id === opened.id))) {
    card.classList.add('is-selected');
  }
  const top = document.createElement('div');
  top.className = 'bs-session__top';
  const faces = document.createElement('div');
  faces.className = 'bs-avatars';
  for (const member of members.slice(0, 4)) faces.appendChild(avatarOf(member, 'sm'));
  top.appendChild(faces);
  const title = document.createElement('div');
  title.className = 'bs-session__title';
  title.textContent = root.summary || root.node + ' / ' + root.dir;
  top.appendChild(title);
  card.appendChild(top);

  if (group === 'needs-you') {
    const now = document.createElement('div');
    now.className = 'bs-session__now is-ask';
    const text = document.createElement('span');
    text.textContent = '! ' + (personaLabel(root.persona) || 'The lead') + ' asks you';
    now.appendChild(text);
    card.appendChild(now);
  } else if (group === 'working') {
    const busy = newestActivity(members.filter((member) =>
      member.state === 'running' || member.state === 'creating'));
    const caption = busy ? activityCaption(busy.activity) : null;
    if (busy && caption) {
      const now = document.createElement('div');
      now.className = 'bs-session__now';
      now.appendChild(avatarOf(busy, 'xs'));
      const text = document.createElement('span');
      text.textContent = memberName(busy, members) + ' is ' + caption;
      now.appendChild(text);
      card.appendChild(now);
    }
  }
  const tasks = root.tasks || { total: 0, done: 0, in_progress: 0 };
  if (tasks.total > 0) {
    const bar = document.createElement('div');
    bar.className = 'bs-progress';
    bar.setAttribute('aria-label', tasks.done + ' of ' + tasks.total + ' tasks done');
    for (const [className, count] of [['is-done', tasks.done], ['is-doing', tasks.in_progress]]) {
      if (!count) continue;
      const part = document.createElement('i');
      part.className = className;
      part.style.width = (100 * count / tasks.total) + '%';
      bar.appendChild(part);
    }
    card.appendChild(bar);
  }
  const meta = document.createElement('div');
  meta.className = 'bs-session__meta';
  const parts = [];
  if (tasks.total > 0) parts.push(tasks.done + ' of ' + tasks.total + ' tasks');
  if (group === 'idle') parts.push(ago(root.created_at_secs));
  parts.push(root.node);
  if (root.summary) parts.push(root.dir.split('/').filter(Boolean).pop() || root.dir);
  const cost = members.reduce((sum, member) => sum + (member.cost || 0), 0);
  if (cost > 0) parts.push(formatCost(cost));
  for (const part of parts) {
    const span = document.createElement('span');
    span.textContent = part;
    meta.appendChild(span);
  }
  card.appendChild(meta);
  card.addEventListener('click', () => openSession(root.id));
  return card;
}

function renderSessions() {
  sessionListEl.textContent = '';
  sessionListEl.hidden = false;
  const roots = sessions.filter((session) => !session.parent_id);
  if (roots.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty bs-empty';
    empty.textContent = 'No sessions yet. Start one with New.';
    sessionListEl.appendChild(empty);
    return;
  }
  const byGroup = new Map(GROUPS.map(([key]) => [key, []]));
  for (const root of roots) {
    const members = treeMembers(root.id);
    byGroup.get(treeGroup(root, members)).push({ root, members });
  }
  for (const [key, label] of GROUPS) {
    const trees = byGroup.get(key);
    if (!trees.length) continue;
    trees.sort((a, b) => (b.root.created_at_secs || 0) - (a.root.created_at_secs || 0));
    const heading = document.createElement('div');
    heading.className = 'bs-label session-group';
    heading.textContent = label + ' · ' + trees.length;
    sessionListEl.appendChild(heading);
    for (const { root, members } of trees) sessionListEl.appendChild(sessionCard(root, members, key));
  }
}

// The newest list the control plane served, kept on the device so a pane
// that opens while the control plane cannot be reached still shows it.
const KEPT_LIST = 'bosun.sessions';
const offlineEl = $('offline');

function keepList(text) {
  try {
    localStorage.setItem(KEPT_LIST, JSON.stringify({ at_ms: Date.now(), text }));
  } catch (error) {
    // A browser that keeps nothing shows no list until it can reach the
    // control plane.
  }
}

// Shows the kept list, when the pane has drawn no list yet, and says that
// the control plane cannot be reached.
function showUnreachable() {
  let kept = null;
  let keptSessions = null;
  try {
    kept = JSON.parse(localStorage.getItem(KEPT_LIST) || 'null');
    keptSessions = kept && JSON.parse(kept.text);
  } catch (error) {
    kept = null;
  }
  let note = 'Cannot reach the control plane.';
  if (keptSessions && sessionListEl.hidden) {
    sessions = keptSessions;
    renderSessions();
  }
  if (kept && !sessionListEl.hidden) {
    const at = new Date(kept.at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    note += ' The list is from ' + at + '.';
  }
  offlineEl.textContent = note;
  offlineEl.hidden = false;
}

let sessionsFetching = false;
async function refreshSessions() {
  if (sessionsFetching) return;
  sessionsFetching = true;
  let response;
  try {
    response = await fetch('/sessions');
  } catch (error) {
    // fetch rejects only when no answer came: the control plane or the
    // network is down.
    showUnreachable();
    sessionsFetching = false;
    return;
  }
  try {
    if (!response.ok) throw new Error('HTTP ' + response.status);
    const text = await response.text();
    sessions = JSON.parse(text);
    keepList(text);
    offlineEl.hidden = true;
    renderSessions();
    setBadge(sessions.filter((session) => !session.parent_id && session.asking).length);
    renderCrew();
    // The waiting label follows the freshest list too: a child's state
    // changes without a state event for the open session, so the event
    // stream alone would leave it stale between fetches.
    const viewed = opened && sessions.find((session) => session.id === opened.id);
    if (opened && !viewed) {
      showStatus('session ' + opened.id + ' ended');
      markListEntry();
      closeSession();
    } else if (viewed && !view.hidden) {
      viewStateDot.className = 'dot ' + viewed.state;
      opened.state = viewed.state;
      updateStatusLabel(viewed);
    }
  } catch (error) {
    showStatus('sessions: ' + error.message);
  } finally {
    sessionsFetching = false;
  }
}
