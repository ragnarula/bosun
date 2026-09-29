// The session list and the poll that refreshes it.

import { sessionListEl, view, viewStateDot } from './dom.js';
import { ago, shortId, showStatus } from './common.js';
import { liveChildrenLabel, updateStatusLabel } from './activity.js';
import { markListEntry, openSession } from './history.js';
import { closeSession, opened } from './session-view.js';
import { renderChildList, updateChildPanelDot } from './subagents.js';

export { refreshSessions, sessions };

let sessions = [];
// The tree roots whose children the user has shown, by owner id: a root owns
// itself, so the render reads it back with root.id, the same id its children
// carry as owner_id. The list is rebuilt on every session poll and every open,
// so this state has to outlive the toggle element that shows it.
const openGroups = new Set();

// Active first, then past: newly creating, running and waiting sessions
// float to the top, each tied by created_at; a finished or interrupted
// session's row keeps its final state below them.
function sessionOrderRank(session) {
  return session.state === 'running' || session.state === 'creating' ? 0
    : session.state === 'waiting_for_input' ? 1
    : 2;
}

function byActiveThenNewest(a, b) {
  const rank = sessionOrderRank(a) - sessionOrderRank(b);
  if (rank !== 0) return rank;
  return (b.created_at_secs || 0) - (a.created_at_secs || 0);
}

// One row per root session: a whole-row tap target led by the model's summary
// of the session when it has one, then the state dot, the node/dir pair, the
// persona tag, and the short id plus age. The full id lives on the session
// screen, so no id is touch-invisible on home.
function appendSessionRow(session) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'session-row';
  const dot = document.createElement('span');
  dot.className = 'dot ' + session.state;
  button.appendChild(dot);
  const main = document.createElement('span');
  main.className = 'row-main';
  // A summarized session is named by that line: the node and directory it
  // works in drop to the meta line under it.
  if (session.summary) {
    const summary = document.createElement('span');
    summary.className = 'row-summary';
    summary.textContent = session.summary;
    main.appendChild(summary);
  }
  const nodeDir = document.createElement('span');
  nodeDir.className = session.summary ? 'row-meta' : 'row-node';
  nodeDir.textContent = session.node + ' / ' + session.dir;
  main.appendChild(nodeDir);
  if (session.persona) {
    const persona = document.createElement('span');
    persona.className = 'row-meta';
    persona.textContent = session.persona;
    main.appendChild(persona);
  }
  const time = document.createElement('span');
  time.className = 'row-time';
  time.textContent = shortId(session.id) + ' · ' + ago(session.created_at_secs);
  main.appendChild(time);
  // A waiting session names who it is waiting for; any other state is the
  // dot's job alone, so nothing repeats on the row.
  if (liveChildrenLabel(session) !== session.state) {
    const waiting = document.createElement('span');
    waiting.className = 'row-meta';
    waiting.textContent = liveChildrenLabel(session);
    main.appendChild(waiting);
  }
  button.appendChild(main);
  button.addEventListener('click', () => openSession(session.id));
  return button;
}

// One banner line per child, flattened under its parent row: rows never
// indent deeper than the one level the children tag opens. A child is
// watch-only, so tapping the line opens its own monitor rather than acting
// on it.
function childLine(child) {
  const line = document.createElement('button');
  line.type = 'button';
  line.className = 'child-row';
  const dot = document.createElement('span');
  dot.className = 'dot ' + child.state;
  line.appendChild(dot);
  const note = document.createElement('span');
  note.className = 'child-note';
  note.textContent = shortId(child.id) + ' · ' + child.state + ' · watch-only';
  line.appendChild(note);
  line.addEventListener('click', () => openSession(child.id));
  return line;
}

function renderSessions() {
  sessionListEl.textContent = '';
  if (sessions.length === 0) {
    sessionListEl.hidden = false;
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = 'no sessions';
    sessionListEl.appendChild(empty);
    return;
  }
  sessionListEl.hidden = false;
  // Split into roots and children. owner_id is the tree root every session
  // in one tree shares; a root owns itself, so a session with a parent never
  // gets its own row — it always appears under its tree's row.
  const roots = [];
  const groups = new Map();
  for (const session of sessions) {
    if (session.parent_id) {
      const owner = session.owner_id;
      if (!groups.has(owner)) groups.set(owner, []);
      groups.get(owner).push(session);
    } else {
      roots.push(session);
    }
  }
  roots.sort(byActiveThenNewest);
  for (const group of groups.values()) group.sort(byActiveThenNewest);
  // A stopped tree leaves no root, and its id would otherwise sit in the set
  // for the life of the tab.
  const liveRoots = new Set(roots.map((root) => root.id));
  for (const id of openGroups) if (!liveRoots.has(id)) openGroups.delete(id);
  for (const root of roots) {
    const item = document.createElement('div');
    item.className = 'session-item';
    item.appendChild(appendSessionRow(root));
    const children = groups.get(root.id) || [];
    const lines = [];
    const expanded = openGroups.has(root.id);
    if (children.length > 0) {
      const label = children.length === 1 ? '1 child' : children.length + ' children';
      const toggle = document.createElement('button');
      toggle.type = 'button';
      toggle.className = expanded ? 'children-toggle open' : 'children-toggle';
      toggle.textContent = expanded ? 'hide children' : label;
      toggle.addEventListener('click', () => {
        const open = toggle.classList.toggle('open');
        if (open) openGroups.add(root.id);
        else openGroups.delete(root.id);
        toggle.textContent = open ? 'hide children' : label;
        for (const line of lines) line.hidden = !open;
      });
      item.appendChild(toggle);
    }
    sessionListEl.appendChild(item);
    for (const child of children) {
      const line = childLine(child);
      line.hidden = !expanded;
      lines.push(line);
      sessionListEl.appendChild(line);
    }
  }
}

let sessionsFetching = false;
async function refreshSessions() {
  if (sessionsFetching) return;
  sessionsFetching = true;
  try {
    const response = await fetch('/sessions');
    if (!response.ok) throw new Error('HTTP ' + response.status);
    sessions = await response.json();
    renderSessions();
    updateChildPanelDot();
    renderChildList();
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
