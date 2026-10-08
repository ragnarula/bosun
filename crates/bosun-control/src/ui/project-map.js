// The Projects tab: every repository the sessions work in, and one project's
// map. The map shows each branch a session works on as a lane off the main
// branch: where it is checked out, who works there, what is not committed or
// not pushed, its pull request, the files two lanes both change, and a feed of
// what happened. The project's stream redraws it on every change.

import { $, isWide, projectsTab, view } from './dom.js';
import { showStatus, toastOk } from './common.js';
import { openSession } from './history.js';
import { nodes } from './machines.js';
import { leaveScreen, showScreen } from './screens.js';
import { sessions } from './session-list.js';
import { opened } from './session-view.js';
import { activityCaption, avatarOf, crewState, treeMembers } from './crew.js';
import { hullOf, icon, memberName, personaLabel } from './signal.js';

export { refreshProjects, updateProjectLink };

const title = $('project-title');
const sub = $('project-sub');
const liveDot = $('project-live');
const seg = $('project-seg');
const warn = $('project-warn');
const body = $('project-body');
const mainEl = $('project-main');
const sideEl = $('project-side');
const projectLink = $('btn-project');
const projectLinkText = $('btn-project-text');

// The desktop map's geometry: the SVG's width, one row per lane, main's row
// at the top, and the gap between main's commits.
const MAP_WIDTH = 560;
const ROW = 120;
const MAIN_Y = ROW / 2;
const MAIN_STEP = 36;
// The phone track's width; main takes its first part.
const TRACK_WIDTH = 280;
const TRACK_MAIN = 56;

const NS = 'http://www.w3.org/2000/svg';

let summaries = [];
// The open project: its stream, its newest view, the chip chosen, the lane
// open in detail, a session whose lane opens with the first view, the session
// the reader came from, and the newest feed entry already drawn.
let current = null;

// ---- Small builders ----

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function svg(name, attrs, parent) {
  const node = document.createElementNS(NS, name);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, String(value));
  if (parent) parent.appendChild(node);
  return node;
}

// The colour a branch's lane keeps on every screen.
function paint(node, branch) {
  node.style.setProperty('--lane', 'var(--hull-' + hullOf(branch || 'detached') + ')');
  return node;
}

function dot(branch) {
  return paint(el('span', 'pm-dot'), branch);
}

function basename(path) {
  const parts = String(path || '').split('/').filter(Boolean);
  return parts.length ? parts[parts.length - 1] : String(path || '');
}

// A long path keeps its end, where the name is.
function clip(path, length) {
  return path.length > length ? '…' + path.slice(path.length - length + 1) : path;
}

function counts(added, removed) {
  const span = el('span', 'bs-count');
  if (added) span.appendChild(el('span', 'add', '+' + added));
  if (added && removed) span.appendChild(document.createTextNode(' '));
  if (removed) span.appendChild(el('span', 'del', '−' + removed));
  return span;
}

function clock(secs) {
  const at = new Date(secs * 1000);
  return at.getHours() + ':' + String(at.getMinutes()).padStart(2, '0');
}

function plural(count, one, many) {
  return count + ' ' + (count === 1 ? one : many);
}

function branchName(lane) {
  return lane.branch || 'detached at ' + String(lane.head || '').slice(0, 7);
}

// ---- What a lane says ----

// Everyone in the trees that work in the lane's folder.
function laneMembers(lane) {
  return lane.sessions.flatMap((root) => treeMembers(root));
}

function dirtyTotals(lane) {
  let added = 0;
  let removed = 0;
  for (const file of lane.dirty) {
    added += file.added;
    removed += file.removed;
  }
  return { files: lane.dirty.length, added, removed };
}

function isMoving(lane) {
  return laneMembers(lane).some((member) => ['working', 'needs-you'].includes(crewState(member)));
}

// One line on what is happening in the lane now: a question for the reader,
// the newest work of a busy member, or what the lane's session is for.
function nowLine(lane) {
  const line = el('div', 'pm-now');
  const members = laneMembers(lane);
  const asking = members.find((member) => crewState(member) === 'needs-you');
  if (asking) {
    line.classList.add('is-ask');
    line.appendChild(avatarOf(asking, 'xs'));
    line.appendChild(el('span', null, (personaLabel(asking.persona) || 'The lead') + ' asks you'));
    return line;
  }
  let busy = null;
  for (const member of members) {
    if (crewState(member) !== 'working' || !member.activity) continue;
    if (!busy || member.activity.at_ms > busy.activity.at_ms) busy = member;
  }
  const caption = busy && activityCaption(busy.activity);
  if (caption) {
    line.appendChild(avatarOf(busy, 'xs'));
    const owner = busy.owner_id || busy.id;
    line.appendChild(el('span', null, memberName(busy, treeMembers(owner)) + ' is ' + caption));
    return line;
  }
  line.classList.add('is-quiet');
  const root = members.find((member) => !member.parent_id);
  line.appendChild(el('span', null, (root && root.summary) || 'Nobody is working on it'));
  return line;
}

function whereLine(lane) {
  const line = el('div', 'pm-where');
  const machine = el('span', 'pm-machine');
  machine.appendChild(icon('machines'));
  machine.appendChild(document.createTextNode(lane.node));
  line.appendChild(machine);
  line.appendChild(el('span', 'pm-kind', lane.kind));
  const path = el('span', 'pm-path', clip(lane.root, 34));
  path.title = lane.root;
  line.appendChild(path);
  return line;
}

function metaLine(lane, mainBranch) {
  const line = el('div', 'pm-meta');
  line.appendChild(el('span', null, lane.ahead + ' ahead'));
  line.appendChild(el('span', null, lane.behind ? lane.behind + ' behind' : 'up to date with ' + mainBranch));
  const local = lane.commits.filter((commit) => !commit.pushed).length;
  if (local) line.appendChild(el('span', 'pm-local', local + ' not pushed'));
  const dirty = dirtyTotals(lane);
  if (dirty.files) {
    const part = el('span');
    part.appendChild(el('span', 'pm-dirty', dirty.files + ' not committed'));
    part.appendChild(document.createTextNode(' '));
    part.appendChild(counts(dirty.added, dirty.removed));
    line.appendChild(part);
  }
  return line;
}

// A pull request's badge says the one fact that matters most now.
function prBadge(pr) {
  if (!pr) return null;
  const n = '#' + pr.number;
  let label = n + ' open';
  let state = 'open';
  if (pr.state === 'merged') [label, state] = [n + ' merged', 'merged'];
  else if (pr.state === 'closed') [label, state] = [n + ' closed', 'closed'];
  else if (pr.state === 'draft') [label, state] = [n + ' draft', 'draft'];
  else if (pr.review === 'changes_requested') [label, state] = [n + ' changes asked', 'failed'];
  else if (pr.checks === 'failed') [label, state] = [n + ' checks failed', 'failed'];
  else if (pr.review === 'approved') [label, state] = [n + ' approved', 'approved'];
  else if (pr.checks === 'passed') [label, state] = [n + ' checks passed', 'passed'];
  else if (pr.checks === 'running') [label, state] = [n + ' checks running', 'running'];
  const badge = el('span', 'pm-pr', label);
  badge.dataset.state = state;
  return badge;
}

// The overlaps a lane is in, each with the other lane's branch.
function laneOverlaps(lane, project) {
  const byKey = new Map(project.lanes.map((each) => [each.key, each]));
  return project.overlaps
    .filter((overlap) => overlap.lanes.includes(lane.key))
    .map((overlap) => {
      const other = byKey.get(overlap.lanes[0] === lane.key ? overlap.lanes[1] : overlap.lanes[0]);
      return { path: overlap.path, other: other ? branchName(other) : 'another branch' };
    });
}

function laneTop(lane, project) {
  const top = el('div', 'pm-card__top');
  top.appendChild(dot(lane.branch));
  top.appendChild(el('span', 'pm-branch', branchName(lane)));
  for (const overlap of laneOverlaps(lane, project).slice(0, 2)) {
    const chip = el('span', 'pm-ovchip', '! ' + basename(overlap.path));
    chip.title = 'Also changes on ' + overlap.other;
    top.appendChild(chip);
  }
  const badge = prBadge(lane.pr);
  if (badge) top.appendChild(badge);
  return top;
}

// ---- The desktop map ----

function mainX(index) {
  return 30 + index * MAIN_STEP;
}

// Draws the lanes off main: each leaves main at its fork point, carries its
// commits, a filled dot for one on the remote and a ring for one only on the
// machine, and a dashed mark with the count of files not committed.
function mapSvg(project) {
  const main = project.main.slice().reverse();
  const height = (project.lanes.length + 1) * ROW;
  const root = svg('svg', {
    width: MAP_WIDTH,
    height,
    viewBox: '0 0 ' + MAP_WIDTH + ' ' + height,
    role: 'img',
    'aria-label': 'Branches as lanes off ' + project.main_branch,
  });
  const lastMain = mainX(Math.max(main.length - 1, 0));
  svg('path', { class: 'm-main', d: 'M' + mainX(0) + ' ' + MAIN_Y + ' L' + lastMain + ' ' + MAIN_Y }, root);
  svg('line', {
    class: 'm-tail', style: '--lane:var(--line-strong);opacity:1',
    x1: lastMain + 8, y1: MAIN_Y, x2: MAP_WIDTH, y2: MAIN_Y,
  }, root);
  if (main.length) {
    svg('text', { class: 'm-label', x: mainX(0), y: MAIN_Y - 14 }, root).textContent =
      plural(main.length, 'newest commit', 'newest commits') + ' on ' + project.main_branch;
  }
  project.lanes.forEach((lane, index) => root.appendChild(mapLane(lane, index + 1, main.length)));
  main.forEach((commit, index) => {
    const mark = svg('circle', { class: 'm-maindot', cx: mainX(index), cy: MAIN_Y, r: 5.5 }, root);
    svg('title', {}, mark).textContent = commit.sha.slice(0, 7) + ' ' + commit.subject;
  });
  // One amber line per pair of lanes that change the same files.
  const rows = new Map(project.lanes.map((lane, index) => [lane.key, index + 1]));
  const pairs = [...new Set(project.overlaps.map((overlap) => overlap.lanes.join('\n')))];
  pairs.forEach((pair, index) => {
    const [a, b] = pair.split('\n').map((key) => MAIN_Y + rows.get(key) * ROW);
    const x = MAP_WIDTH - 18 - index * 12;
    svg('line', { class: 'm-overlap', x1: x, y1: a, x2: x, y2: b }, root);
    svg('circle', { class: 'm-overlap-end', cx: x, cy: a, r: 4 }, root);
    svg('circle', { class: 'm-overlap-end', cx: x, cy: b, r: 4 }, root);
  });
  return root;
}

function mapLane(lane, row, mainCount) {
  const group = paint(svg('g', {}), lane.branch);
  const y = MAIN_Y + row * ROW;
  // A fork older than the commits listed leaves from main's left end.
  const forkX = lane.fork_index == null ? 10 : mainX(mainCount - 1 - lane.fork_index);
  const commits = lane.commits.slice().reverse();
  const start = forkX + 26;
  const room = MAP_WIDTH - 90 - start;
  const step = Math.max(4, Math.min(24, commits.length > 1 ? room / (commits.length - 1) : 24));
  const xs = commits.map((_, index) => start + index * step);
  const last = xs.length ? xs[xs.length - 1] : forkX + 14;
  svg('path', {
    class: 'm-lane',
    d: 'M' + forkX + ' ' + MAIN_Y + ' L' + forkX + ' ' + (y - 14) + ' Q' + forkX + ' ' + y + ' ' + (forkX + 14) + ' ' + y + ' L' + last + ' ' + y,
  }, group);
  const dirty = dirtyTotals(lane);
  let tailFrom = last + 8;
  if (dirty.files) {
    const x = last + 26;
    svg('line', { class: 'm-lane', x1: last, y1: y, x2: x - 8, y2: y, style: 'stroke-dasharray:3 3' }, group);
    const ring = svg('g', { class: isMoving(lane) ? 'm-breathe' : '' }, group);
    svg('circle', { class: 'm-dirty', cx: x, cy: y, r: 9 }, ring);
    svg('text', { class: 'm-dirty-n', x, y: y + 0.5 }, group).textContent = dirty.files;
    tailFrom = x + 12;
  }
  svg('line', { class: 'm-tail', x1: tailFrom, y1: y, x2: MAP_WIDTH, y2: y }, group);
  commits.forEach((commit, index) => {
    const mark = svg('circle', { class: commit.pushed ? 'm-pushed' : 'm-local', cx: xs[index], cy: y, r: 5.5 }, group);
    svg('title', {}, mark).textContent = commit.sha.slice(0, 7) + ' ' + commit.subject;
  });
  return group;
}

function mainCard(project) {
  const card = el('div', 'pm-card is-main');
  const top = el('div', 'pm-card__top');
  const mark = el('span', 'pm-dot');
  mark.style.setProperty('--lane', 'var(--line-strong)');
  top.appendChild(mark);
  top.appendChild(el('span', 'pm-branch', project.main_branch));
  card.appendChild(top);
  const head = project.main[0];
  if (head) {
    const now = el('div', 'pm-now is-quiet');
    const text = el('span');
    text.appendChild(el('span', 'pm-mono', head.sha.slice(0, 7)));
    text.appendChild(document.createTextNode(' ' + head.subject));
    now.appendChild(text);
    card.appendChild(now);
  }
  const meta = el('div', 'pm-meta');
  meta.appendChild(el('span', null, plural(project.lanes.length, 'branch', 'branches') + ' off it'));
  card.appendChild(meta);
  return card;
}

function laneCard(lane, project, className) {
  const card = paint(el('button', className), lane.branch);
  card.type = 'button';
  card.dataset.lane = lane.key;
  if (current && current.lane === lane.key) card.classList.add('is-selected');
  card.appendChild(laneTop(lane, project));
  if (className === 'pm-lane') card.appendChild(track(lane, project.main.length));
  card.appendChild(whereLine(lane));
  card.appendChild(nowLine(lane));
  card.appendChild(metaLine(lane, project.main_branch));
  card.addEventListener('click', () => openLane(lane.key));
  return card;
}

function renderMap(project) {
  const map = el('div', 'pm-map');
  map.appendChild(mapSvg(project));
  const cards = el('div', 'pm-cards');
  cards.appendChild(mainCard(project));
  for (const lane of project.lanes) cards.appendChild(laneCard(lane, project, 'pm-card'));
  map.appendChild(cards);
  mainEl.appendChild(map);
  renderTail(project);
}

// ---- The phone's lane cards ----

// A short track for one lane: main, the fork, the commits and the mark for
// files not committed.
function track(lane, mainCount) {
  const root = svg('svg', {
    class: 'pm-track', viewBox: '0 0 ' + TRACK_WIDTH + ' 22',
    preserveAspectRatio: 'xMinYMid meet', 'aria-hidden': 'true',
  });
  const y = 7;
  const mainY = 17;
  const fork = lane.fork_index == null ? mainCount : lane.fork_index;
  const forkX = 10 + Math.round((1 - fork / Math.max(mainCount, 1)) * (TRACK_MAIN - 16));
  const start = forkX + 18;
  const commits = lane.commits.slice().reverse();
  const step = Math.min(14, (TRACK_WIDTH - 40 - start) / Math.max(commits.length - 1, 1));
  const xs = commits.map((_, index) => start + index * step);
  const last = xs.length ? xs[xs.length - 1] : start;
  svg('path', { class: 'm-main', style: 'stroke-width:2', d: 'M0 ' + mainY + ' L' + TRACK_MAIN + ' ' + mainY }, root);
  svg('path', {
    class: 'm-lane', style: 'stroke-width:2',
    d: 'M' + forkX + ' ' + mainY + ' Q' + forkX + ' ' + y + ' ' + (forkX + 10) + ' ' + y + ' L' + last + ' ' + y,
  }, root);
  commits.forEach((commit, index) => {
    svg('circle', { class: commit.pushed ? 'm-pushed' : 'm-local', cx: xs[index], cy: y, r: 4 }, root);
  });
  if (lane.dirty.length) {
    svg('line', { class: 'm-lane', style: 'stroke-width:2;stroke-dasharray:2 3', x1: last + 5, y1: y, x2: last + 13, y2: y }, root);
    const ring = svg('g', { class: isMoving(lane) ? 'm-breathe' : '' }, root);
    svg('circle', { class: 'm-dirty', cx: last + 20, cy: y, r: 5.5 }, ring);
  }
  return root;
}

function renderLaneCards(project) {
  const moving = project.lanes.filter(isMoving);
  const quiet = project.lanes.filter((lane) => !isMoving(lane));
  for (const [label, lanes] of [['Moving', moving], ['Quiet', quiet]]) {
    if (!lanes.length) continue;
    mainEl.appendChild(el('div', 'bs-label', label + ' · ' + lanes.length));
    for (const lane of lanes) mainEl.appendChild(laneCard(lane, project, 'pm-lane'));
  }
  renderTail(project);
}

// Below the lanes: the branches only on the remote, why pull requests are
// missing, and the branches merged recently.
function renderTail(project) {
  if (project.remote_only) {
    const line = el('div', 'pm-lane is-quiet');
    line.appendChild(el('div', 'pm-now is-quiet', plural(project.remote_only, 'more branch is', 'more branches are') +
      ' only on ' + (project.web_url ? 'GitHub' : 'the remote')));
    mainEl.appendChild(line);
  }
  const note = {
    no_token: 'Pull requests are off: the control plane has no github_token.',
    failing: 'GitHub did not answer the last read of pull requests.',
  }[project.pull_requests];
  if (note) mainEl.appendChild(el('div', 'pm-note', note));
  if (!project.merged.length) return;
  mainEl.appendChild(el('div', 'bs-label pm-merged-head', 'Merged · ' + project.merged.length));
  for (const merged of project.merged) {
    const card = paint(el('div', 'pm-lane is-merged'), merged.branch);
    const top = el('div', 'pm-card__top');
    top.appendChild(dot(merged.branch));
    top.appendChild(el('span', 'pm-branch', merged.branch));
    if (merged.pr) {
      const badge = el('span', 'pm-pr', '#' + merged.pr + ' merged');
      badge.dataset.state = 'merged';
      top.appendChild(badge);
    }
    card.appendChild(top);
    card.appendChild(el('div', 'pm-now is-quiet', (merged.title ? merged.title + ' · ' : '') +
      'into ' + project.main_branch + ' at ' + clock(merged.merged_at_secs)));
    mainEl.appendChild(card);
  }
}

// ---- Where ----

// The machines and their copies of the repository, with each worktree under
// the folder it belongs to.
function renderWhere(project) {
  const byNode = new Map();
  for (const lane of project.lanes) {
    if (!byNode.has(lane.node)) byNode.set(lane.node, []);
    byNode.get(lane.node).push(lane);
  }
  const grid = el('div', 'pm-where-grid');
  for (const [name, lanes] of [...byNode].sort((a, b) => a[0].localeCompare(b[0]))) {
    const section = el('section', 'pm-section');
    const head = el('div', 'pm-machinehead');
    head.appendChild(icon('machines'));
    head.appendChild(document.createTextNode(name));
    const node = nodes.find((each) => each.name === name);
    if (node) head.appendChild(el('small', node.up ? 'is-up' : 'is-down', node.up ? '● up' : '✕ down'));
    section.appendChild(head);
    const copies = el('div', 'pm-copies');
    for (const lane of orderCopies(lanes)) {
      const copy = paint(el('button', 'pm-copy' + (lane.kind === 'worktree' ? ' is-worktree' : '')), lane.branch);
      copy.type = 'button';
      copy.appendChild(el('div', 'pm-copy__path', lane.root));
      const kind = el('div', 'pm-where');
      kind.appendChild(el('span', 'pm-kind', lane.kind));
      if (lane.worktree_of) kind.appendChild(el('span', null, 'of ' + basename(lane.worktree_of)));
      copy.appendChild(kind);
      const branch = el('div', 'pm-card__top');
      branch.appendChild(dot(lane.branch));
      branch.appendChild(el('span', 'pm-branch', branchName(lane)));
      copy.appendChild(branch);
      copy.appendChild(nowLine(lane));
      const dirty = dirtyTotals(lane);
      if (dirty.files) {
        const meta = el('div', 'pm-meta');
        meta.appendChild(el('span', 'pm-dirty', plural(dirty.files, 'file', 'files') + ' not committed'));
        meta.appendChild(counts(dirty.added, dirty.removed));
        copy.appendChild(meta);
      }
      copy.addEventListener('click', () => openLane(lane.key));
      copies.appendChild(copy);
    }
    section.appendChild(copies);
    grid.appendChild(section);
  }
  mainEl.appendChild(grid);
}

// Folders first, each followed by its worktrees.
function orderCopies(lanes) {
  const roots = lanes.filter((lane) => lane.kind !== 'worktree');
  const ordered = [];
  for (const root of roots) {
    ordered.push(root);
    ordered.push(...lanes.filter((lane) => lane.worktree_of === root.root));
  }
  return ordered.concat(lanes.filter((lane) => !ordered.includes(lane)));
}

// ---- One lane ----

function renderLane(project, lane) {
  const wrap = paint(el('div', 'pm-detail'), lane.branch);
  const head = el('div', 'pm-detail__head');
  head.appendChild(dot(lane.branch));
  head.appendChild(el('span', 'pm-branch', branchName(lane)));
  const close = el('button', 'bs-btn bs-btn--ghost bs-btn--icon pm-detail__close');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close the branch');
  close.appendChild(icon('close'));
  close.addEventListener('click', closeLane);
  head.appendChild(close);
  wrap.appendChild(head);
  wrap.appendChild(whereLine(lane));

  if (lane.pr) {
    const card = el('div', 'pm-prcard');
    const top = el('div', 'pm-card__top');
    top.appendChild(el('span', 'pm-mono bs-muted', '#' + lane.pr.number));
    top.appendChild(prBadge(lane.pr));
    card.appendChild(top);
    // The pull request opens on GitHub. The pane builds no anchor here: the
    // one place that does checks its target, and this checks its own.
    const open = el('button', 'pm-prcard__title', lane.pr.title);
    open.type = 'button';
    const url = lane.pr.url;
    open.disabled = !/^https:\/\//i.test(url);
    open.addEventListener('click', () => window.open(url, '_blank', 'noopener'));
    card.appendChild(open);
    const checks = el('div', 'pm-checks');
    checks.appendChild(el('span', null, {
      passed: 'checks passed', running: 'checks running', failed: 'checks failed',
    }[lane.pr.checks] || 'no checks'));
    if (lane.pr.review) {
      checks.appendChild(el('span', null, lane.pr.review === 'approved' ? 'approved' : 'changes asked for'));
    }
    card.appendChild(checks);
    wrap.appendChild(card);
  }

  const crew = el('div', 'pm-section');
  crew.appendChild(el('div', 'bs-label', 'Crew'));
  for (const rootId of lane.sessions) {
    const members = treeMembers(rootId);
    const row = el('button', 'pm-crewrow');
    row.type = 'button';
    const faces = el('div', 'bs-avatars');
    for (const member of members.slice(0, 4)) faces.appendChild(avatarOf(member, 'sm'));
    row.appendChild(faces);
    const root = members.find((member) => member.id === rootId);
    row.appendChild(el('span', 'pm-crewrow__text', (root && root.summary) ||
      members.map((member) => memberName(member, members)).join(', ') || rootId));
    row.appendChild(el('span', 'bs-muted', 'Open ›'));
    row.addEventListener('click', () => {
      closeProject();
      openSession(rootId);
    });
    crew.appendChild(row);
  }
  // A quiet lane's line repeats what the crew row says.
  const now = nowLine(lane);
  if (!now.classList.contains('is-quiet')) crew.appendChild(now);
  wrap.appendChild(crew);

  const overlaps = laneOverlaps(lane, project);
  if (overlaps.length) {
    const box = el('div', 'pm-needs');
    for (const overlap of overlaps) {
      const line = el('div');
      line.appendChild(el('b', null, '! '));
      line.appendChild(el('span', 'pm-mono', overlap.path));
      line.appendChild(document.createTextNode(' also changes on ' + overlap.other));
      box.appendChild(line);
    }
    wrap.appendChild(box);
  }

  const files = el('div', 'pm-section');
  files.appendChild(el('div', 'bs-label', 'Not committed · ' + lane.dirty.length));
  if (!lane.dirty.length) files.appendChild(el('span', 'bs-muted pm-small', 'Everything is committed.'));
  for (const file of lane.dirty) {
    const row = el('div', 'bs-file pm-file');
    const op = el('span', 'bs-op', { created: 'C', edited: 'E', deleted: 'D' }[file.op]);
    op.dataset.op = file.op;
    row.appendChild(op);
    const path = el('div', 'bs-file__path');
    const slash = file.path.lastIndexOf('/');
    if (slash >= 0) path.appendChild(el('span', 'bs-file__dir', file.path.slice(0, slash + 1)));
    path.appendChild(document.createTextNode(file.path.slice(slash + 1)));
    row.appendChild(path);
    row.appendChild(counts(file.added, file.removed));
    files.appendChild(row);
  }
  wrap.appendChild(files);

  const commits = el('div', 'pm-section');
  commits.appendChild(el('div', 'bs-label', 'Commits · ' + lane.ahead));
  for (const commit of lane.commits) {
    const row = el('div', 'pm-commit');
    const mark = svg('svg', { viewBox: '0 0 12 12', 'aria-hidden': 'true' });
    svg('circle', { cx: 6, cy: 6, r: 4.5, class: commit.pushed ? 'm-pushed' : 'm-local' }, mark);
    row.appendChild(mark);
    row.appendChild(el('span', null, commit.subject));
    row.appendChild(el('span', 'pm-commit__sha', commit.sha.slice(0, 7)));
    row.title = commit.pushed ? 'On the remote' : 'Only on ' + lane.node;
    commits.appendChild(row);
  }
  wrap.appendChild(commits);
  sideEl.appendChild(wrap);
}

// ---- Live ----

const FEED_ICONS = {
  pr_opened: 'pr', checks: 'check', review: 'review', merged: 'merge', overlap: 'ask',
  committed: 'commit', pushed: 'push', switched: 'branch',
};

// Who did it: the session whose call made the change, or the reader for a
// change made outside Bosun. News from GitHub has nobody.
function feedWho(entry) {
  if (entry.session) {
    const session = sessions.find((each) => each.id === entry.session);
    if (session) {
      return { name: memberName(session, treeMembers(session.owner_id || session.id)), face: avatarOf(session, 'sm') };
    }
    return { name: personaLabel(entry.persona) || 'A crew member', face: null };
  }
  if (['pr_opened', 'checks', 'review', 'merged', 'overlap'].includes(entry.kind)) return { name: null, face: null };
  const you = el('span', 'pm-you');
  you.appendChild(icon('persona'));
  you.setAttribute('role', 'img');
  you.setAttribute('aria-label', 'You');
  return { name: 'You', face: you };
}

function feedText(entry, who) {
  const name = who.name || 'Someone';
  const pr = entry.pr ? '#' + entry.pr : 'the pull request';
  switch (entry.kind) {
    case 'created': return name + ' created ' + entry.text;
    case 'edited': return name + ' edited ' + entry.text;
    case 'deleted': return name + ' deleted ' + entry.text;
    case 'committed':
      return entry.text ? name + ' committed “' + entry.text + '”' : 'and ' + plural(entry.count, 'more commit', 'more commits');
    case 'pushed': return name + ' pushed ' + plural(entry.count, 'commit', 'commits');
    case 'switched': return name + ' switched to ' + (entry.branch || 'a detached head');
    case 'pr_opened': return pr + ' opened: ' + (entry.text || '');
    case 'checks': return 'Checks ' + (entry.checks === 'failed' ? 'failed' : 'passed') + ' on ' + pr;
    case 'review': return entry.review === 'approved' ? pr + ' was approved' : 'Changes were asked for on ' + pr;
    case 'merged': return pr + ' merged into main';
    case 'overlap': return entry.text + ' is changing on two branches';
    default: return entry.kind;
  }
}

function renderFeed(project, into, fresh) {
  const list = el('div', 'pm-feed__list');
  list.setAttribute('aria-label', 'What happened');
  if (!project.feed.length) list.appendChild(el('div', 'bs-muted pm-small', 'Nothing has happened since the control plane started.'));
  project.feed.forEach((entry, index) => {
    const row = el('div', 'pm-event' + (index < fresh ? ' is-fresh' : ''));
    const who = feedWho(entry);
    let face = who.face;
    if (!face) {
      face = el('span', 'pm-event__icon' + (entry.kind === 'overlap' ? ' is-warn' : ''));
      face.appendChild(icon(FEED_ICONS[entry.kind] || 'edit'));
    }
    row.appendChild(face);
    const text = el('div', 'pm-event__text');
    text.appendChild(document.createTextNode(feedText(entry, who) + ' '));
    if (entry.added || entry.removed) text.appendChild(counts(entry.added, entry.removed));
    const meta = el('div', 'pm-event__meta');
    meta.appendChild(el('span', null, clock(entry.at_secs)));
    if (entry.branch) {
      meta.appendChild(dot(entry.branch));
      meta.appendChild(el('span', 'pm-mono', entry.branch));
    }
    text.appendChild(meta);
    row.appendChild(text);
    list.appendChild(row);
  });
  into.appendChild(list);
}

// ---- The screen ----

function renderList() {
  title.textContent = 'Projects';
  sub.textContent = summaries.length ? plural(summaries.length, 'repository', 'repositories') : '';
  liveDot.hidden = true;
  seg.hidden = true;
  warn.hidden = true;
  body.dataset.mode = 'list';
  delete body.dataset.lane;
  mainEl.textContent = '';
  sideEl.textContent = '';
  if (!summaries.length) {
    mainEl.appendChild(el('div', 'bs-empty', 'No session works in a git repository yet.'));
    return;
  }
  for (const summary of summaries) {
    const row = el('button', 'bs-session pm-project');
    row.type = 'button';
    row.appendChild(el('div', 'bs-session__title', summary.name));
    const meta = el('div', 'bs-session__meta');
    meta.appendChild(el('span', null, summary.url));
    meta.appendChild(el('span', null, plural(summary.lanes, 'branch', 'branches')));
    if (summary.overlaps) meta.appendChild(el('span', 'pm-overlap', plural(summary.overlaps, 'file', 'files') + ' on two branches'));
    row.appendChild(meta);
    row.addEventListener('click', () => openProject(summary.id, null));
    mainEl.appendChild(row);
  }
}

function render() {
  const project = current && current.view;
  if (!project) return;
  title.textContent = project.name;
  sub.textContent = project.url + ' · ' + plural(project.lanes.length, 'branch', 'branches');
  seg.hidden = false;
  const wide = isWide();
  const mode = wide && current.mode === 'live' ? 'branches' : current.mode;
  for (const chip of seg.querySelectorAll('[data-pm]')) {
    chip.setAttribute('aria-pressed', String(chip.dataset.pm === mode));
  }
  renderWarning(project);
  const lane = current.lane && project.lanes.find((each) => each.key === current.lane);
  if (!lane) current.lane = null;
  body.dataset.mode = mode;
  if (lane) body.dataset.lane = lane.key;
  else delete body.dataset.lane;

  const fresh = freshCount(project);
  const scroll = mainEl.scrollTop;
  mainEl.textContent = '';
  sideEl.textContent = '';
  if (mode === 'where') renderWhere(project);
  else if (mode === 'live') renderFeed(project, mainEl, fresh);
  else if (wide) renderMap(project);
  else renderLaneCards(project);
  if (lane) renderLane(project, lane);
  else if (wide) renderFeed(project, sideEl, fresh);
  mainEl.scrollTop = scroll;
}

// The entries above the newest one already drawn are new.
function freshCount(project) {
  const top = project.feed.length ? JSON.stringify(project.feed[0]) : null;
  let fresh = 0;
  if (current.feedTop !== undefined) {
    const index = project.feed.findIndex((entry) => JSON.stringify(entry) === current.feedTop);
    fresh = index < 0 ? project.feed.length : index;
  }
  current.feedTop = top;
  return fresh;
}

function renderWarning(project) {
  if (!project.overlaps.length) {
    warn.hidden = true;
    return;
  }
  const byKey = new Map(project.lanes.map((lane) => [lane.key, branchName(lane)]));
  const paths = new Map();
  for (const overlap of project.overlaps) {
    if (!paths.has(overlap.path)) paths.set(overlap.path, new Set());
    for (const key of overlap.lanes) paths.get(overlap.path).add(byKey.get(key) || key);
  }
  warn.textContent = '';
  warn.appendChild(icon('ask'));
  const text = el('span');
  [...paths].forEach(([path, branches], index) => {
    if (index) text.appendChild(document.createTextNode('; '));
    text.appendChild(el('span', 'pm-mono', path));
    text.appendChild(document.createTextNode(' is changing on ' + [...branches].join(' and ')));
  });
  text.appendChild(document.createTextNode('. Merge one first, or agree who owns the file.'));
  warn.appendChild(text);
  warn.hidden = false;
}

function openLane(key) {
  if (!current) return;
  current.lane = key;
  render();
}

function closeLane() {
  if (!current) return;
  current.lane = null;
  render();
}

// Opens a project's map and follows its stream. `focus` is a root session
// whose lane opens with the first view.
function openProject(id, focus) {
  closeProject();
  const summary = summaries.find((each) => each.id === id);
  current = {
    id,
    es: null,
    view: null,
    mode: 'branches',
    lane: null,
    focus,
    returnTo: !projectsTab.hidden ? null : opened && !view.hidden ? opened.id : null,
  };
  showScreen(projectsTab);
  title.textContent = summary ? summary.name : 'Project';
  sub.textContent = summary ? summary.url : '';
  seg.hidden = true;
  warn.hidden = true;
  mainEl.textContent = '';
  sideEl.textContent = '';
  const s = current;
  s.es = new EventSource('/projects/' + encodeURIComponent(id) + '/events');
  s.es.addEventListener('project', (event) => {
    if (current !== s) return;
    try {
      s.view = JSON.parse(event.data);
    } catch (error) {
      showStatus('project: ' + error.message);
      return;
    }
    if (s.focus) {
      const lane = s.view.lanes.find((each) => each.sessions.includes(s.focus));
      if (lane) s.lane = lane.key;
      s.focus = null;
    }
    render();
  });
  s.es.addEventListener('gone', () => {
    if (current !== s) return;
    const name = s.view ? s.view.name : 'the project';
    closeProject();
    toastOk('No session works in ' + name + ' now');
    refreshProjects().then(renderList);
  });
  s.es.onopen = () => {
    if (current === s) setLive(true);
  };
  s.es.onerror = () => {
    if (current === s) setLive(false);
  };
}

function setLive(live) {
  liveDot.hidden = false;
  liveDot.classList.toggle('is-paused', !live);
  liveDot.textContent = live ? 'Live' : 'Reconnecting';
}

function closeProject() {
  if (current && current.es) current.es.close();
  current = null;
}

// Projects opens on the map when there is one project, and on the list when
// there are more.
async function openProjectsTab() {
  showScreen(projectsTab);
  renderList();
  await refreshProjects();
  if (projectsTab.hidden || current) return;
  if (summaries.length === 1) openProject(summaries[0].id, null);
  else renderList();
}

// Back closes an open lane on a phone, then the project, and returns to the
// session the reader came from, the list, or Home.
function back() {
  if (current && current.lane && !isWide()) {
    closeLane();
    return;
  }
  const returnTo = current && current.returnTo;
  const fromList = current && summaries.length > 1 && !returnTo;
  closeProject();
  if (returnTo && opened && opened.id === returnTo) {
    showScreen(view);
  } else if (fromList) {
    renderList();
  } else {
    leaveScreen(projectsTab);
  }
}

let fetching = false;
async function refreshProjects() {
  if (fetching) return;
  fetching = true;
  try {
    const response = await fetch('/projects');
    if (!response.ok) throw new Error('HTTP ' + response.status);
    summaries = await response.json();
    updateProjectLink();
    if (!projectsTab.hidden && !current) renderList();
  } catch (error) {
    showStatus('projects: ' + error.message);
  } finally {
    fetching = false;
  }
}

// The open session's header links to its project's map, on its lane.
function updateProjectLink() {
  const session = opened && sessions.find((each) => each.id === opened.id);
  const root = session ? session.owner_id || session.id : opened && opened.id;
  const summary = root && summaries.find((each) => each.sessions.includes(root));
  projectLink.hidden = !summary;
  if (!summary) return;
  projectLinkText.textContent = summary.name;
  projectLink.dataset.project = summary.id;
  projectLink.dataset.root = root;
}

projectLink.addEventListener('click', () => {
  openProject(projectLink.dataset.project, projectLink.dataset.root);
});
for (const chip of seg.querySelectorAll('[data-pm]')) {
  chip.addEventListener('click', () => {
    if (!current) return;
    current.mode = chip.dataset.pm;
    current.lane = null;
    render();
  });
}
$('projects-strip').addEventListener('click', openProjectsTab);
$('btn-projects-back').addEventListener('click', back);
// The map and the cards swap at the wide breakpoint.
let wasWide = isWide();
window.addEventListener('resize', () => {
  if (isWide() === wasWide) return;
  wasWide = isWide();
  if (current && !projectsTab.hidden) render();
});
