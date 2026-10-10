// The crew: every session in the open session's tree, followed on one stream.
// The reader talks to the lead, the tree's root, and the lead runs the crew.
// Chat draws the lead's conversation, Crew draws the chain of command and the
// flow of orders, reports and questions between agents, a member's screen
// draws one agent's own thread, and Tasks and Files draw what the whole crew
// does. The Log view is the open session's own transcript, which
// session-view.js draws.

import { $, input, view } from './dom.js';
import { ago, showStatus } from './common.js';
import { sessions } from './session-list.js';
import { personas } from './new-session.js';
import { opened } from './session-view.js';
import { closeMember, openMember } from './history.js';
import { renderMarkdown } from './markdown.js';
import { avatar, flag, icon, memberName, personaLabel } from './signal.js';
import { USER_REJECTED_TEXT } from './composer.js';
import { closeMemberLog, followMemberLog } from './member-log.js';
import { setView } from './views.js';
import { follow } from './scroll.js';

export {
  activityCaption,
  avatarOf,
  closeCrew,
  crewState,
  hideMember,
  openCrew,
  renderCrew,
  showMember,
  shownMember,
  treeMembers,
};

const crewBar = $('crew-bar');
const crewBarFaces = $('crew-bar-faces');
const crewBarText = $('crew-bar-text');
const chatDot = $('chat-dot');
const chatList = $('chat-list');
const chatView = $('chat-view');
const crewTree = $('crew-tree');
const crewCount = $('crew-count');
const flowChips = $('flow-chips');
const flowList = $('flow-list');
const memberView = $('member-view');
const memberFace = $('member-face');
const memberTitle = $('member-title');
const memberSub = $('member-sub');
const memberScroll = $('member-scroll');
const memberThread = $('member-thread');
const memberCrew = $('member-crew');
const memberFootText = $('member-foot-text');
const btnAskLead = $('btn-ask-lead');
const btnMemberBack = $('btn-member-back');
const tasksList = $('tasks-list');
const tasksProgress = $('tasks-progress');
const tasksCount = $('tasks-count');
const filesNow = $('files-now');
const filesList = $('files-list');
const filesCount = $('files-count');

// How many items a thread keeps on screen. The store holds everything, and the
// Log reads it back; a thread is the recent conversation.
const MAX_THREAD_ITEMS = 600;
// How many diff lines a chat card shows before the reader taps for the rest.
const CARD_LINES = 6;
// How many messages the tree stream replays when the crew opens. The tail
// counts every member's messages, and Chat draws only the lead's.
const TREE_TAIL = 300;
// How many messages the pane keeps per member, to draw a member's thread when
// its screen opens.
const MAX_MEMBER_MESSAGES = 400;
// How many orders, reports and questions Flow keeps, and how many it draws.
const MAX_SIGNALS = 300;
const FLOW_ROWS = 80;

// The Flow filter the reader chose, kept across sessions for this page load.
let flowFilter = 'all';

// ---- The crew's members ----

// The sessions of the tree `rootId` owns, root first, then in the order they
// were created.
function treeMembers(rootId) {
  return sessions
    .filter((session) => session.owner_id === rootId || session.id === rootId)
    .sort((a, b) =>
      a.id === rootId ? -1 : b.id === rootId ? 1 : (a.created_at_secs || 0) - (b.created_at_secs || 0)
    );
}

// The face a session shows: the persona's uploaded picture, or the robot its
// seed draws. A root draws from its persona's stored seed, a child from its
// own id, so two builders differ but fly the same flag.
function avatarOf(session, size, state) {
  const persona = personas.find((each) => each.name === session.persona);
  const src = persona && persona.picture_at_secs
    ? '/personas/' + encodeURIComponent(persona.name) + '/avatar?v=' + persona.picture_at_secs
    : null;
  const seed = session.parent_id ? session.id : persona ? persona.avatar_seed : session.persona;
  return avatar({ seed, persona: session.persona, state, size, src });
}

// What a member's ring shows from the session list alone. A question waiting
// on the user is the root's; a working loop, a parent waiting on its
// children, a crash and a member with nothing to do each have their own.
function crewState(session) {
  if (session.state === 'interrupted' && session.interrupt_cause === 'crash') return 'stopped';
  if (session.asking && !session.parent_id) return 'needs-you';
  if (session.state === 'running' || session.state === 'creating') return 'working';
  if (session.state === 'waiting_for_input' && liveChildren(session) > 0) return 'waiting';
  return 'idle';
}

function liveChildren(session) {
  return sessions.filter((each) => each.parent_id === session.id && each.state !== 'stopped').length;
}

// Whether `ancestorId` is above `id` in the tree.
function isAbove(ancestorId, id) {
  let session = sessionOf(id);
  while (session.parent_id) {
    if (session.parent_id === ancestorId) return true;
    session = sessionOf(session.parent_id);
  }
  return false;
}

// What a member's ring shows in the open crew. While the root waits on a
// question it surfaced, the member that asked it waits on the reader, and
// each member between them waits on the question.
function memberState(crew, session) {
  const question = crew.question;
  if (question && question.leaf !== crew.rootId && sessionOf(crew.rootId).asking) {
    if (session.id === question.leaf) return 'needs-you';
    if (session.id !== crew.rootId && isAbove(session.id, question.leaf)) return 'waiting';
  }
  return crewState(session);
}

// The word a state pill prints for a member.
function stateWord(session, state) {
  switch (state) {
    case 'needs-you': return session.parent_id ? 'asks you' : 'needs you';
    case 'working': return 'working';
    case 'waiting': return 'waiting';
    case 'stopped': return 'stopped';
    default: return session.parent_id ? 'done' : 'ready';
  }
}

function statePill(session, state) {
  const pill = document.createElement('span');
  pill.className = 'bs-status';
  pill.dataset.state = state === 'idle' ? (session.parent_id ? 'done' : 'idle') : state;
  pill.textContent = stateWord(session, state);
  return pill;
}

// The sessions under `id`, in the order they were created. A member whose
// parent the list no longer holds stays in the tree under the root.
function childrenOf(members, id, rootId) {
  const known = new Set(members.map((member) => member.id));
  return members.filter((member) =>
    member.id !== rootId
    && (member.parent_id === id || (id === rootId && !known.has(member.parent_id)))
  );
}

// The names from `id` up to the root, root first: "Lead › Builder".
function pathNames(crew, id) {
  const names = [];
  let session = sessionOf(id);
  for (;;) {
    names.unshift(nameOf(crew, session.id));
    if (!session.parent_id || session.id === crew.rootId) break;
    session = sessionOf(session.parent_id);
  }
  return names;
}

// The verb a tool's activity reads as, in a crew caption.
const VERBS = {
  edit: 'editing',
  file_write: 'writing',
  file_read: 'reading',
  grep: 'searching for',
  glob: 'looking for',
  shell: 'running',
  webfetch: 'reading',
  skill: 'loading skill',
  spawn: 'starting a',
  message_child: 'messaging',
  history_read: 'reading history',
  ask: 'asking',
  todowrite: 'updating tasks',
};

// The last part of a path: a caption has room for a file's name, not its
// directory.
function basename(path) {
  const parts = String(path).split('/').filter(Boolean);
  return parts.length ? parts[parts.length - 1] : String(path);
}

// What an activity says a member is doing, in a few words, from the event
// alone: "editing winsw.ts", "running npm test", "thinking".
function activityCaption(activity) {
  if (!activity) return null;
  switch (activity.phase) {
    case 'tool_started': {
      const verb = VERBS[activity.name] || 'using ' + activity.name;
      if (!activity.target || activity.name === 'todowrite' || activity.name === 'ask') return verb;
      const target = ['edit', 'file_write', 'file_read'].includes(activity.name)
        ? basename(activity.target)
        : activity.target;
      return verb + ' ' + target;
    }
    case 'wake_started':
    case 'request_sent':
    case 'first_token':
    case 'response_complete':
    case 'tool_finished':
    case 'empty_retry':
      return 'thinking';
    case 'compaction_started':
    case 'compaction_finished':
      return 'tidying its notes';
    default:
      return null;
  }
}

// One line saying what a member is doing now.
function memberCaption(crew, session, state) {
  switch (state) {
    case 'stopped': return 'stopped';
    case 'needs-you':
      return session.id === crew.rootId && crew.question && crew.question.leaf !== crew.rootId
        ? 'passed a question from ' + nameOf(crew, crew.question.leaf) + ' to you'
        : 'asks you';
    case 'waiting': {
      const live = liveChildren(session);
      return live ? 'waiting on ' + live : 'waiting for your answer';
    }
    case 'working': {
      const activity = crew.activity.get(session.id) || session.activity;
      return activityCaption(activity) || 'working';
    }
    default:
      return session.parent_id ? 'done' : 'ready';
  }
}

// ---- Opening and closing ----

// A thread draws one member's conversation into `list`: Chat is the root's
// thread, and a member's screen is that member's.
function newThread(list, scroller, memberId) {
  return {
    list,
    scroller,
    memberId,
    // The last post a post by the same author joins, the open fold of tool
    // calls, the Orders card the next order joins, and each call's card or
    // order row by call id.
    last: null,
    fold: null,
    orders: null,
    calls: new Map(),
    // The root's reply as it streams, which the durable text replaces.
    live: null,
    // Whether the thread follows its newest item.
    stick: true,
    // The order rows still waiting on a report, by the child they name.
    openRows: new Map(),
    // The newest question card, which its answered copy fills in place.
    askCard: null,
    // A child's own question that its next instruction answers.
    pendingAsk: null,
    // Each child's question to this member, by child: a question the member
    // passes on to the reader replaces it.
    askLines: new Map(),
  };
}

function newCrew(rootId) {
  return {
    rootId,
    es: null,
    // Each member's newest activity from the stream, by session id.
    activity: new Map(),
    // Each member's recent messages, by session id, to draw its thread when
    // its screen opens.
    messages: new Map(),
    chat: newThread(chatList, chatView, rootId),
    // The open member screen's thread, or null.
    member: null,
    // Every order, report and question between agents, oldest first.
    signals: [],
    // The spawn calls whose result has not named their child yet.
    spawns: new Map(),
    // The origin of each child's newest question to the root, by child.
    askOrigins: new Map(),
    // The question the root put to the reader and the member that asked it.
    question: null,
    // The edit and write calls whose result has not arrived, by call id.
    fileCalls: new Map(),
    // The task list, as the root's newest `todos` event wrote it.
    tasks: [],
    // Each changed file by path: what happened to it, its lines, who, when,
    // and its newest diff lines.
    files: new Map(),
    // The newest file change, for the Files view's "now" block.
    latest: null,
    renderTimer: null,
  };
}

// Follows the tree the open session belongs to on one stream. The crew's
// view state lives on the open session, so closing the session drops it.
function openCrew(s, rootId) {
  const crew = newCrew(rootId);
  s.crew = crew;
  chatList.textContent = '';
  tasksList.textContent = '';
  filesList.textContent = '';
  filesNow.textContent = '';
  renderTasks(crew);
  renderFiles(crew);
  crew.es = new EventSource(
    '/sessions/' + encodeURIComponent(rootId) + '/tree-events?tail=' + TREE_TAIL
  );
  crew.es.onmessage = (event) => {
    if (opened !== s || s.crew !== crew) return;
    try {
      handleTreeFrame(crew, JSON.parse(event.data));
    } catch (error) {
      showStatus('crew: ' + error.message);
    }
  };
  crew.es.onerror = () => {
    if (opened === s && s.crew === crew) showStatus('crew: stream lost, reconnecting');
  };
  if (s.memberId) buildMember(crew, s.memberId);
  renderCrew();
}

function closeCrew(s) {
  if (s && s.crew) {
    if (s.crew.es) s.crew.es.close();
    window.clearTimeout(s.crew.renderTimer);
    s.crew = null;
  }
  hideMember();
  crewBar.hidden = true;
  crewBarFaces.textContent = '';
  chatList.textContent = '';
  crewTree.textContent = '';
  flowList.textContent = '';
  tasksList.textContent = '';
  filesList.textContent = '';
  filesNow.textContent = '';
}

// ---- Drawing what names members ----

// Redraws everything that names members, from the newest session list. The
// session poll calls this every few seconds.
function renderCrew() {
  const s = opened;
  if (!s || !s.crew) return;
  const crew = s.crew;
  renderMembers(crew);
  renderTasks(crew);
  renderFiles(crew);
  // The crew bar can appear as members join, which moves the bottom of the
  // transcript and the chat; a reader at the newest line stays there.
  follow();
  followThread(crew.chat);
}

// The stream changes members' states and adds signals many times a second
// while it replays, so their views redraw once per burst.
function scheduleRender(crew) {
  if (crew.renderTimer) return;
  crew.renderTimer = window.setTimeout(() => {
    crew.renderTimer = null;
    if (opened && opened.crew === crew) renderMembers(crew);
  }, 0);
}

function renderMembers(crew) {
  const members = treeMembers(crew.rootId);
  renderBar(crew, members);
  renderTree(crew, members);
  renderFlow(crew);
  refreshRows(crew, crew.chat);
  if (crew.member) {
    renderMemberHead(crew, crew.member.memberId, members);
    refreshRows(crew, crew.member);
  }
  chatDot.hidden = !sessionOf(crew.rootId).asking;
}

// The crew bar under the header of Chat: the children's faces and who works
// or asks the reader. A tap opens Crew.
function renderBar(crew, members) {
  const children = members.filter((member) => member.id !== crew.rootId);
  crewBar.hidden = children.length === 0;
  crewBarFaces.textContent = '';
  crewBarText.textContent = '';
  if (!children.length) return;
  for (const child of children.slice(0, 5)) crewBarFaces.appendChild(avatarOf(child, 'xs'));
  const states = children.map((child) => memberState(crew, child));
  const working = states.filter((state) => state === 'working').length;
  const asking = states.filter((state) => state === 'needs-you').length;
  const parts = [];
  if (working) parts.push(document.createTextNode(working + ' working'));
  if (asking) {
    const ask = document.createElement('span');
    ask.className = 'crew-bar__ask';
    ask.textContent = asking + ' asks you';
    parts.push(ask);
  }
  if (!parts.length) parts.push(document.createTextNode(children.length + (children.length === 1 ? ' agent' : ' agents')));
  parts.forEach((part, index) => {
    if (index) crewBarText.appendChild(document.createTextNode(' · '));
    crewBarText.appendChild(part);
  });
}

crewBar.addEventListener('click', () => setView('crew'));

// ---- Crew: the chain of command ----

function renderTree(crew, members) {
  crewTree.textContent = '';
  crewCount.textContent = members.length > 1 ? 'crew of ' + members.length : '';
  if (!members.length) return;
  const you = document.createElement('div');
  you.className = 'tree-node';
  const youRow = document.createElement('div');
  youRow.className = 'tree-you';
  const badge = document.createElement('span');
  badge.className = 'you-badge';
  badge.textContent = 'You';
  youRow.appendChild(badge);
  const youText = document.createElement('span');
  youText.className = 'tree-you__text';
  youText.textContent = 'You give orders to ' + nameOf(crew, crew.rootId) + ' only.';
  youRow.appendChild(youText);
  you.appendChild(youRow);
  const kids = document.createElement('div');
  kids.className = 'tree-kids';
  kids.appendChild(treeNode(crew, members, sessionOf(crew.rootId)));
  you.appendChild(kids);
  crewTree.appendChild(you);
}

function treeNode(crew, members, session) {
  const node = document.createElement('div');
  node.className = 'tree-node';
  const state = memberState(crew, session);
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'tree-row';
  row.dataset.member = session.id;
  row.appendChild(avatarOf(session, 'md', state));
  const body = document.createElement('span');
  body.className = 'tree-row__body';
  const head = document.createElement('span');
  head.className = 'tree-row__head';
  const name = document.createElement('span');
  name.className = 'tree-row__name';
  name.textContent = nameOf(crew, session.id);
  head.appendChild(name);
  head.appendChild(statePill(session, state));
  body.appendChild(head);
  const now = document.createElement('span');
  now.className = 'tree-row__line';
  now.textContent = memberCaption(crew, session, state);
  body.appendChild(now);
  const signal = newestSignal(crew, session.id);
  if (signal) {
    const line = document.createElement('span');
    line.className = 'tree-row__line';
    const kind = document.createElement('b');
    kind.textContent = SIGNAL_WORDS[signal.kind];
    line.appendChild(kind);
    line.appendChild(document.createTextNode(' · ' + firstLine(signal.text)));
    body.appendChild(line);
  }
  row.appendChild(body);
  // The root's thread is Chat, so its node opens its Log.
  row.addEventListener('click', () => {
    if (session.id === crew.rootId) setView('log');
    else openMember(session.id);
  });
  node.appendChild(row);
  const children = childrenOf(members, session.id, crew.rootId);
  if (children.length) {
    const kids = document.createElement('div');
    kids.className = 'tree-kids';
    for (const child of children) kids.appendChild(treeNode(crew, members, child));
    node.appendChild(kids);
  }
  return node;
}

// The newest order a member received or report it sent.
function newestSignal(crew, id) {
  for (let i = crew.signals.length - 1; i >= 0; i--) {
    const signal = crew.signals[i];
    if (signal.kind === 'order' && signal.to === id) return signal;
    if (['report', 'ask', 'failure'].includes(signal.kind) && signal.from === id) return signal;
  }
  return null;
}

// ---- Crew: the flow ----

const SIGNAL_WORDS = {
  order: 'Order',
  report: 'Report',
  ask: 'Asked',
  failure: 'Failed',
  question: 'Question',
  answer: 'Answer',
};

const FLOW_KINDS = {
  all: null,
  order: ['order'],
  report: ['report', 'failure'],
  question: ['ask', 'question', 'answer'],
};

function addSignal(crew, signal) {
  crew.signals.push(signal);
  if (crew.signals.length > MAX_SIGNALS) crew.signals.shift();
}

function renderFlow(crew) {
  for (const chip of flowChips.querySelectorAll('[data-flow]')) {
    chip.setAttribute('aria-pressed', String(chip.dataset.flow === flowFilter));
  }
  flowList.textContent = '';
  const kinds = FLOW_KINDS[flowFilter];
  const shown = [];
  for (let i = crew.signals.length - 1; i >= 0 && shown.length < FLOW_ROWS; i--) {
    const signal = crew.signals[i];
    if (!kinds || kinds.includes(signal.kind)) shown.push(signal);
  }
  if (!shown.length) {
    flowList.appendChild(emptyNote('No orders or reports yet.'));
    return;
  }
  for (const signal of shown) flowList.appendChild(flowRow(crew, signal));
}

function flowRow(crew, signal) {
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'flow-row';
  row.dataset.kind = signal.kind;
  const time = document.createElement('span');
  time.className = 'flow-row__time';
  time.textContent = clockTime(signal.atMs);
  row.appendChild(time);
  const faces = document.createElement('span');
  faces.className = 'flow-row__faces';
  faces.appendChild(faceOf(signal.from));
  const arrow = document.createElement('span');
  arrow.className = 'flow-row__to';
  arrow.textContent = '›';
  arrow.setAttribute('aria-label', 'to');
  faces.appendChild(arrow);
  faces.appendChild(faceOf(signal.to));
  row.appendChild(faces);
  const body = document.createElement('span');
  body.className = 'flow-row__body';
  const head = document.createElement('span');
  head.className = 'flow-row__head';
  const kind = document.createElement('b');
  kind.textContent = SIGNAL_WORDS[signal.kind];
  head.appendChild(kind);
  head.appendChild(document.createTextNode(' ' + whoName(crew, signal.from) + ' › ' + whoName(crew, signal.to)));
  body.appendChild(head);
  const text = document.createElement('span');
  text.className = 'flow-row__text';
  text.textContent = firstLine(signal.text);
  body.appendChild(text);
  row.appendChild(body);
  // A tap opens the agent the signal is about: the child, not the reader or
  // the root.
  const target = [signal.from, signal.to].find((id) => id !== 'you' && id !== crew.rootId);
  row.addEventListener('click', () => {
    if (target) openMember(target);
  });
  return row;
}

function faceOf(id) {
  if (id === 'you') {
    const badge = document.createElement('span');
    badge.className = 'you-badge you-badge--xs';
    badge.textContent = 'You';
    return badge;
  }
  return avatarOf(sessionOf(id), 'xs');
}

function whoName(crew, id) {
  return id === 'you' ? 'You' : nameOf(crew, id);
}

for (const chip of flowChips.querySelectorAll('[data-flow]')) {
  chip.addEventListener('click', () => {
    flowFilter = chip.dataset.flow;
    if (opened && opened.crew) renderFlow(opened.crew);
  });
}

function firstLine(text) {
  return String(text || '').split('\n').map((line) => line.trim()).find((line) => line) || '';
}

// ---- A crew member's screen ----

function shownMember() {
  return opened ? opened.memberId || null : null;
}

// Shows `id`'s screen over the open session. The root's thread is Chat, so a
// screen for the root opens the root's Log instead.
function showMember(id) {
  const s = opened;
  if (!s) return;
  const rootId = s.crew ? s.crew.rootId : s.id;
  if (id === rootId) {
    hideMember();
    setView('log');
    return;
  }
  if (s.memberId === id && s.crew && s.crew.member) return;
  s.memberId = id;
  if (s.crew) buildMember(s.crew, id);
}

function buildMember(crew, id) {
  closeMemberLog();
  memberThread.textContent = '';
  memberCrew.textContent = '';
  crew.member = newThread(memberThread, memberScroll, id);
  for (const { message, atMs } of crew.messages.get(id) || []) {
    drawThreadMessage(crew, crew.member, message, atMs);
  }
  setMemberView('thread');
  view.dataset.member = id;
  memberView.hidden = false;
  renderMemberHead(crew, id, treeMembers(crew.rootId));
  crew.member.stick = true;
  followThread(crew.member);
}

function hideMember() {
  if (opened) {
    opened.memberId = null;
    if (opened.crew) opened.crew.member = null;
  }
  closeMemberLog();
  delete view.dataset.member;
  memberView.hidden = true;
  memberThread.textContent = '';
  memberCrew.textContent = '';
}

function renderMemberHead(crew, id, members) {
  const session = sessionOf(id);
  const state = memberState(crew, session);
  memberFace.textContent = '';
  memberFace.appendChild(avatarOf(session, 'md', state));
  const name = nameOf(crew, id);
  memberTitle.textContent = name;
  memberSub.textContent = [pathNames(crew, id).join(' › '), stateWord(session, state), session.node]
    .filter(Boolean)
    .join(' · ');
  const parent = session.parent_id ? nameOf(crew, session.parent_id) : null;
  const lead = nameOf(crew, crew.rootId);
  memberFootText.textContent = parent
    ? name + ' takes orders from ' + parent + '. To change its work, tell ' + lead + '.'
    : '';
  btnAskLead.textContent = 'Ask ' + lead;
  memberCrew.textContent = '';
  const children = childrenOf(members, id, crew.rootId);
  if (!children.length) return;
  const label = document.createElement('div');
  label.className = 'bs-label';
  label.textContent = name + '’s crew';
  memberCrew.appendChild(label);
  for (const child of children) {
    const childState = memberState(crew, child);
    const signal = newestSignal(crew, child.id);
    memberCrew.appendChild(memberRow(crew, child, childState, signal ? firstLine(signal.text) : ''));
  }
}

// A row naming a member: its face, its name, a line of text and its state.
// A tap opens its screen.
function memberRow(crew, session, state, text) {
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'bs-order';
  row.dataset.member = session.id;
  row.appendChild(avatarOf(session, 'sm'));
  const body = document.createElement('span');
  body.className = 'bs-order__body';
  const name = document.createElement('span');
  name.className = 'bs-order__name';
  name.textContent = nameOf(crew, session.id);
  body.appendChild(name);
  const line = document.createElement('span');
  line.className = 'bs-order__text';
  line.textContent = text;
  body.appendChild(line);
  row.appendChild(body);
  row.appendChild(statePill(session, state));
  row.addEventListener('click', () => openMember(session.id));
  return row;
}

function setMemberView(name) {
  memberView.dataset.memberView = name;
  for (const chip of memberView.querySelectorAll('.member-switch [data-member-view]')) {
    chip.setAttribute('aria-pressed', String(chip.dataset.memberView === name));
  }
  if (name === 'log' && opened && opened.memberId) followMemberLog(opened.memberId);
}

for (const chip of memberView.querySelectorAll('.member-switch [data-member-view]')) {
  chip.addEventListener('click', () => setMemberView(chip.dataset.memberView));
}

btnMemberBack.addEventListener('click', () => closeMember());

// The reader steers a child only through the lead: the button opens Chat with
// the child named in the message box.
btnAskLead.addEventListener('click', () => {
  const crew = opened && opened.crew;
  const id = shownMember();
  if (!crew || !id) return;
  const name = nameOf(crew, id);
  closeMember();
  setView('chat');
  if (!input.value.trim()) input.value = 'About ' + name + ': ';
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
});

// ---- The tree stream ----

function handleTreeFrame(crew, frame) {
  if (Object.prototype.hasOwnProperty.call(frame, 'delta')) {
    if (frame.session_id === crew.rootId) drawDelta(crew.chat, frame.delta);
    return;
  }
  if (!frame.event) return;
  const event = frame.event;
  const member = frame.session_id;
  switch (event.kind) {
    case 'message': {
      const message = event.message;
      keepMessage(crew, member, message, event.at_ms);
      track(crew, member, message, event.at_ms);
      if (member === crew.rootId) drawThreadMessage(crew, crew.chat, message, event.at_ms);
      if (crew.member && crew.member.memberId === member) {
        drawThreadMessage(crew, crew.member, message, event.at_ms);
      }
      scheduleRender(crew);
      break;
    }
    case 'todos':
      if (member === crew.rootId) {
        crew.tasks = Array.isArray(event.items) ? event.items : [];
        renderTasks(crew);
      }
      break;
    case 'activity': {
      crew.activity.set(member, event);
      if (event.phase === 'tool_started' || event.phase === 'tool_finished') scheduleRender(crew);
      break;
    }
    default:
      break;
  }
}

function keepMessage(crew, member, message, atMs) {
  let kept = crew.messages.get(member);
  if (!kept) {
    kept = [];
    crew.messages.set(member, kept);
  }
  kept.push({ message, atMs });
  if (kept.length > MAX_MEMBER_MESSAGES) kept.shift();
}

// What one message tells the whole crew, whoever's thread is on screen: the
// orders, reports and questions between agents, the question the reader
// holds, and the files the crew changed.
function track(crew, member, message, atMs) {
  const block = message.block;
  const isRoot = member === crew.rootId;
  if (message.role === 'assistant') {
    if (block.kind === 'tool_call') {
      const args = block.args || {};
      if (block.name === 'spawn') {
        crew.spawns.set(block.id, { from: member, text: String(args.instructions || ''), atMs });
      } else if (block.name === 'message_child') {
        addSignal(crew, { kind: 'order', from: member, to: args.id, text: String(args.text || ''), atMs });
      } else if (block.name === 'edit' || block.name === 'file_write') {
        const path = String(args.path || '');
        crew.fileCalls.set(block.id, { name: block.name, path, memberId: member, atMs });
        noteFileChange(crew, member, path, block.name === 'file_write' ? 'written' : 'edited', null, null, atMs, diffLines(block.name, args));
      }
    } else if (block.kind === 'ask' && isRoot) {
      const leaf = block.child_id ? crew.askOrigins.get(block.child_id) || block.child_id : crew.rootId;
      if (block.answer) {
        if (crew.question && crew.question.message === block.message) {
          addSignal(crew, { kind: 'answer', from: 'you', to: leaf, text: block.answer, atMs });
          crew.question = null;
        }
      } else {
        addSignal(crew, { kind: 'question', from: leaf, to: 'you', text: block.message, atMs });
        crew.question = { leaf, message: block.message };
      }
    }
    return;
  }
  switch (block.kind) {
    case 'text':
      if (!isRoot || block.text === USER_REJECTED_TEXT) {
        if (isRoot) crew.question = null;
        return;
      }
      // The reader's message answers the root's own question, or is an order.
      if (crew.question && crew.question.leaf === crew.rootId) {
        addSignal(crew, { kind: 'answer', from: 'you', to: crew.rootId, text: block.text, atMs });
      } else {
        addSignal(crew, { kind: 'order', from: 'you', to: crew.rootId, text: block.text, atMs });
      }
      crew.question = null;
      return;
    case 'child_event': {
      const kind = block.event_kind === 'failure' ? 'failure' : block.event_kind === 'ask' ? 'ask' : 'report';
      addSignal(crew, { kind, from: block.child_id, to: member, text: block.text, atMs });
      if (kind === 'ask' && isRoot) crew.askOrigins.set(block.child_id, block.origin || block.child_id);
      return;
    }
    case 'tool_result': {
      const content = block.content || {};
      const spawn = crew.spawns.get(block.id);
      if (spawn) {
        crew.spawns.delete(block.id);
        if (!block.is_error && content.child_id) {
          addSignal(crew, { kind: 'order', from: spawn.from, to: content.child_id, text: spawn.text, atMs: spawn.atMs });
        }
        return;
      }
      const call = crew.fileCalls.get(block.id);
      if (call) {
        crew.fileCalls.delete(block.id);
        if (block.is_error) return;
        const op = call.name === 'file_write' && content.created ? 'created' : 'edited';
        noteFileChange(crew, call.memberId, call.path, op, content.added, content.removed, call.atMs, null);
        return;
      }
      // A shell command names the files its run changed.
      const files = Array.isArray(content.files) ? content.files : [];
      for (const file of files) {
        noteFileChange(crew, member, file.path, file.op, null, null, atMs, null, true);
      }
      return;
    }
    default:
      return;
  }
}

// ---- Threads ----

function sessionOf(id) {
  return sessions.find((session) => session.id === id) || { id, persona: null };
}

function nameOf(crew, id) {
  const members = treeMembers(crew.rootId);
  const session = members.find((each) => each.id === id) || sessionOf(id);
  return memberName(session, members.length ? members : [session]);
}

function clockTime(atMs) {
  if (atMs == null) return '';
  return new Date(atMs).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
}

// Adds an item to a thread, dropping the oldest past the cap, and keeps the
// newest in view for a reader who is at the bottom.
function appendItem(thread, item) {
  thread.list.appendChild(item);
  thread.orders = null;
  while (thread.list.children.length > MAX_THREAD_ITEMS) thread.list.removeChild(thread.list.firstChild);
  followThread(thread);
}

function followThread(thread) {
  if (thread.stick) thread.scroller.scrollTop = thread.scroller.scrollHeight;
}

for (const [scroller, threadOf] of [
  [chatView, (crew) => crew.chat],
  [memberScroll, (crew) => crew.member],
]) {
  scroller.addEventListener('scroll', () => {
    const thread = opened && opened.crew && threadOf(opened.crew);
    if (!thread) return;
    thread.stick = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 40;
  });
}

// A post by a member: avatar, a head naming the member, a word saying what
// the post is, and the body. A post by the author of the item above, of the
// same kind, joins that item instead of repeating the head.
function post(crew, thread, authorId, atMs, body, label) {
  const last = thread.last;
  thread.orders = null;
  if (last && last.authorId === authorId && last.label === label && !thread.fold) {
    last.body.appendChild(body);
    followThread(thread);
    return last;
  }
  const item = document.createElement('div');
  item.className = 'bs-msg';
  item.dataset.member = authorId;
  item.appendChild(avatarOf(sessionOf(authorId), 'sm'));
  const column = document.createElement('div');
  column.className = 'bs-msg__column';
  const head = document.createElement('div');
  head.className = 'bs-msg__head';
  head.appendChild(document.createTextNode(nameOf(crew, authorId)));
  if (label) {
    const labelEl = document.createElement('span');
    labelEl.className = 'bs-msg__to';
    labelEl.textContent = label;
    head.appendChild(labelEl);
  }
  const time = document.createElement('span');
  time.className = 'bs-msg__time';
  time.textContent = clockTime(atMs);
  head.appendChild(time);
  column.appendChild(head);
  const bodyEl = document.createElement('div');
  bodyEl.className = 'bs-msg__body';
  bodyEl.appendChild(body);
  column.appendChild(bodyEl);
  item.appendChild(column);
  appendItem(thread, item);
  thread.last = { authorId, label, body: bodyEl };
  thread.fold = null;
  return thread.last;
}

function markdown(text) {
  const holder = document.createElement('div');
  holder.className = 'bs-md';
  holder.appendChild(renderMarkdown(text));
  return holder;
}

function plain(text) {
  const p = document.createElement('p');
  p.className = 'bs-plain';
  p.textContent = text;
  return p;
}

// Your own message: the only bubble in the chat.
function yourMessage(thread, text, atMs) {
  const item = document.createElement('div');
  item.className = 'bs-msg bs-msg--you';
  item.dataset.member = 'you';
  const column = document.createElement('div');
  const body = document.createElement('div');
  body.className = 'bs-msg__body';
  body.textContent = text;
  body.title = clockTime(atMs);
  column.appendChild(body);
  item.appendChild(column);
  appendItem(thread, item);
  thread.last = null;
  thread.fold = null;
}

// A note across a thread: a cleared context, a summary.
function note(thread, text, kind) {
  const item = document.createElement('div');
  item.className = 'chat-note' + (kind ? ' ' + kind : '');
  item.textContent = text;
  appendItem(thread, item);
  thread.last = null;
  thread.fold = null;
}

// The words a folded line uses for each kind of call.
const FOLD_WORDS = {
  read: ['read', 'file', 'files'],
  search: ['searched', 'time', 'times'],
  shell: ['ran', 'command', 'commands'],
  fetch: ['fetched', 'page', 'pages'],
  skill: ['loaded', 'skill', 'skills'],
  tasks: ['updated the task list', '', ''],
  other: ['used', 'tool', 'tools'],
};

function foldKind(name) {
  switch (name) {
    case 'file_read':
    case 'history_read':
      return 'read';
    case 'grep':
    case 'glob':
      return 'search';
    case 'shell':
      return 'shell';
    case 'webfetch':
      return 'fetch';
    case 'skill':
      return 'skill';
    case 'todowrite':
      return 'tasks';
    default:
      return 'other';
  }
}

function foldText(counts) {
  const parts = [];
  for (const [kind, count] of Object.entries(counts)) {
    const [verb, one, many] = FOLD_WORDS[kind];
    if (kind === 'tasks') parts.push(verb);
    else parts.push(verb + ' ' + count + ' ' + (count === 1 ? one : many));
  }
  return parts.join(' · ');
}

// A tool call that is neither an order nor a file change joins the thread's
// folded line: one quiet line per run of calls. A tap lists them.
function foldCall(thread, name, args) {
  let fold = thread.fold;
  if (!fold) {
    const item = document.createElement('div');
    item.className = 'bs-activity-wrap';
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'bs-activity';
    button.appendChild(icon('read'));
    const label = document.createElement('span');
    button.appendChild(label);
    const list = document.createElement('div');
    list.className = 'bs-activity__calls';
    list.hidden = true;
    button.addEventListener('click', () => {
      list.hidden = !list.hidden;
    });
    item.appendChild(button);
    item.appendChild(list);
    appendItem(thread, item);
    fold = { counts: {}, label, list };
    thread.fold = fold;
    thread.last = null;
  }
  const kind = foldKind(name);
  fold.counts[kind] = (fold.counts[kind] || 0) + 1;
  fold.label.textContent = foldText(fold.counts);
  const line = document.createElement('div');
  line.className = 'bs-activity__call';
  line.textContent = name + (callTarget(name, args) ? ' ' + callTarget(name, args) : '');
  fold.list.appendChild(line);
}

function callTarget(name, args) {
  if (!args || typeof args !== 'object') return '';
  const key = {
    file_read: 'path', grep: 'pattern', glob: 'pattern', shell: 'command',
    webfetch: 'url', skill: 'name', history_read: 'op',
  }[name];
  const value = key ? args[key] : null;
  return typeof value === 'string' ? value.split('\n').find((line) => line.trim()) || '' : '';
}

// Line counts as the record prints them: a sign before each number, so the
// change reads without its colour.
function countEl(added, removed) {
  const count = document.createElement('span');
  count.className = 'bs-count';
  if (added != null && added > 0) {
    const add = document.createElement('span');
    add.className = 'add';
    add.textContent = '+' + added;
    count.appendChild(add);
  }
  if (removed != null && removed > 0) {
    if (count.childNodes.length) count.appendChild(document.createTextNode(' '));
    const del = document.createElement('span');
    del.className = 'del';
    del.textContent = '−' + removed;
    count.appendChild(del);
  }
  return count;
}

// The diff lines a call's arguments carry: an edit's old and new text, or
// the start of a new file.
function diffLines(name, args) {
  const lines = [];
  if (name === 'edit') {
    for (const line of String(args.old || '').split('\n')) if (line !== '') lines.push(['del', line]);
    for (const line of String(args.new || '').split('\n')) if (line !== '') lines.push(['add', line]);
  } else if (name === 'file_write') {
    for (const line of String(args.content || '').split('\n')) if (line !== '') lines.push(['add', line]);
  }
  return lines;
}

function diffCard(path, lines, open) {
  const card = document.createElement('div');
  card.className = 'bs-diff';
  const head = document.createElement('div');
  head.className = 'bs-diff__head';
  head.appendChild(icon('edit'));
  const pathEl = document.createElement('span');
  pathEl.className = 'bs-diff__path';
  pathEl.textContent = path;
  head.appendChild(pathEl);
  const counts = document.createElement('span');
  head.appendChild(counts);
  card.appendChild(head);
  const body = document.createElement('div');
  const shown = open ? lines : lines.slice(0, CARD_LINES);
  for (const [kind, text] of shown) {
    const line = document.createElement('div');
    line.className = 'bs-diff__line is-' + kind;
    line.textContent = text;
    body.appendChild(line);
  }
  if (!open && lines.length > CARD_LINES) {
    const more = document.createElement('button');
    more.type = 'button';
    more.className = 'bs-diff__more';
    more.textContent = (lines.length - CARD_LINES) + ' more lines';
    more.addEventListener('click', () => card.replaceWith(diffCard(path, lines, true)));
    body.appendChild(more);
  }
  card.appendChild(body);
  return { card, counts };
}

// One message of the member a thread draws. Chat is the root's thread: the
// reader's messages, the root's text, file changes and questions, its orders
// as cards and its children's reports as lines. A child's thread draws its
// parent's orders as posts by the parent. Everything else is in the Log.
function drawThreadMessage(crew, thread, message, atMs) {
  const block = message.block;
  const self = thread.memberId;
  const isRoot = self === crew.rootId;
  if (message.role === 'user') {
    switch (block.kind) {
      case 'text':
        if (isRoot) {
          if (block.text === USER_REJECTED_TEXT) note(thread, 'You dismissed the question', 'muted');
          else yourMessage(thread, block.text, atMs);
        } else if (thread.pendingAsk) {
          // The text that follows a child's own question answers it.
          answerAsk(thread.pendingAsk, 'Answer: ' + block.text);
          thread.pendingAsk = null;
        } else {
          const parent = sessionOf(self).parent_id || self;
          post(crew, thread, parent, atMs, markdown(block.text), 'order');
        }
        return;
      case 'tool_result':
        drawResult(crew, thread, block);
        return;
      case 'child_event':
        drawChildEvent(crew, thread, block, atMs);
        return;
      default:
        return;
    }
  }
  switch (block.kind) {
    case 'text': {
      if (!block.text) return;
      const live = thread.live;
      if (live) {
        live.body.replaceWith(markdown(block.text));
        thread.live = null;
        return;
      }
      post(crew, thread, self, atMs, markdown(block.text));
      return;
    }
    case 'tool_call':
      drawCall(crew, thread, block, atMs);
      return;
    case 'ask':
      drawAsk(crew, thread, block, atMs);
      return;
    case 'summary':
      note(thread, nameOf(crew, self) + ' compacted its notes', 'muted');
      return;
    case 'context_cleared':
      note(thread, nameOf(crew, self) + ' started fresh: ' + (block.reason || ''), 'muted');
      return;
    default:
      return;
  }
}

function drawCall(crew, thread, block, atMs) {
  const args = block.args || {};
  const self = thread.memberId;
  switch (block.name) {
    case 'spawn': {
      const row = orderRow(crew, thread, atMs, null, args.persona, String(args.instructions || ''));
      thread.calls.set(block.id, { name: 'spawn', row });
      return;
    }
    case 'message_child':
      orderRow(crew, thread, atMs, args.id, null, String(args.text || ''));
      return;
    case 'edit':
    case 'file_write': {
      const path = String(args.path || '');
      const { card, counts } = diffCard(path, diffLines(block.name, args), false);
      post(crew, thread, self, atMs, card);
      thread.calls.set(block.id, { name: block.name, counts });
      return;
    }
    case 'ask':
      // The question itself arrives as its own message.
      return;
    default:
      foldCall(thread, block.name, args);
  }
}

function drawResult(crew, thread, block) {
  const call = thread.calls.get(block.id);
  if (!call) return;
  thread.calls.delete(block.id);
  const content = block.content || {};
  if (call.name === 'spawn') {
    if (block.is_error || !content.child_id) setOutcome(call.row, 'failed');
    else bindRow(crew, thread, call.row, content.child_id);
    return;
  }
  if (block.is_error) {
    call.counts.className = 'bs-count error';
    call.counts.textContent = 'failed';
    return;
  }
  call.counts.replaceWith(countEl(content.added, content.removed));
}

// ---- Orders ----

const OUTCOMES = {
  done: ['done', 'done'],
  asked: ['waiting', 'asked'],
  failed: ['stopped', 'failed'],
};

// One order in the thread's Orders card: the orders a member gives in a row
// are one card under its text, a row per child. The row shows the child's
// live state until the child reports, asks or fails, then that outcome.
function orderRow(crew, thread, atMs, childId, persona, text) {
  if (!thread.orders) {
    const card = document.createElement('div');
    card.className = 'bs-orders';
    const label = document.createElement('div');
    label.className = 'bs-label';
    card.appendChild(label);
    post(crew, thread, thread.memberId, atMs, card);
    thread.orders = { card, label, count: 0 };
  }
  const orders = thread.orders;
  orders.count += 1;
  orders.label.textContent = orders.count === 1 ? 'Order' : 'Orders · ' + orders.count;
  const el = document.createElement('button');
  el.type = 'button';
  el.className = 'bs-order';
  const row = { el, childId: null, persona, face: null, name: null, pill: null, outcome: null };
  row.face = document.createElement('span');
  row.face.className = 'bs-order__face';
  el.appendChild(row.face);
  const body = document.createElement('span');
  body.className = 'bs-order__body';
  row.name = document.createElement('span');
  row.name.className = 'bs-order__name';
  body.appendChild(row.name);
  const line = document.createElement('span');
  line.className = 'bs-order__text';
  line.textContent = firstLine(text);
  body.appendChild(line);
  el.appendChild(body);
  row.pill = document.createElement('span');
  el.appendChild(row.pill);
  el.addEventListener('click', () => {
    if (row.childId) openMember(row.childId);
  });
  orders.card.appendChild(el);
  if (childId) bindRow(crew, thread, row, childId);
  else drawRow(crew, row);
  followThread(thread);
  return row;
}

function bindRow(crew, thread, row, childId) {
  row.childId = childId;
  row.el.dataset.member = childId;
  const open = thread.openRows.get(childId) || [];
  open.push(row);
  thread.openRows.set(childId, open);
  drawRow(crew, row);
}

function drawRow(crew, row) {
  const session = row.childId ? sessionOf(row.childId) : { id: '', persona: row.persona, parent_id: 'x' };
  row.face.textContent = '';
  row.face.appendChild(row.childId ? avatarOf(session, 'sm') : avatar({ seed: row.persona, persona: row.persona, size: 'sm' }));
  row.name.textContent = row.childId ? nameOf(crew, row.childId) : personaLabel(row.persona) || 'Agent';
  if (row.outcome) {
    const [state, word] = OUTCOMES[row.outcome];
    row.pill.className = 'bs-status';
    row.pill.dataset.state = state;
    row.pill.textContent = word;
    return;
  }
  const state = row.childId ? memberState(crew, session) : 'working';
  row.pill.replaceWith(statePill(session, state));
  row.pill = row.el.lastChild;
}

function setOutcome(row, outcome) {
  row.outcome = outcome;
  const [state, word] = OUTCOMES[outcome];
  row.pill.className = 'bs-status';
  row.pill.dataset.state = state;
  row.pill.textContent = word;
}

// A child's report, question or failure closes the rows of the orders it
// answers.
function closeRows(thread, childId, outcome) {
  for (const row of thread.openRows.get(childId) || []) setOutcome(row, outcome);
  thread.openRows.delete(childId);
}

// The open rows follow their child's live state as the session list moves.
function refreshRows(crew, thread) {
  for (const rows of thread.openRows.values()) {
    for (const row of rows) drawRow(crew, row);
  }
}

// ---- Reports and questions ----

const EVENT_VERBS = {
  report: 'reported to',
  ask: 'asked',
};

// A child's report, question or failure, as one line in its parent's thread.
// A long text shows its first lines until the reader taps it.
function drawChildEvent(crew, thread, block, atMs) {
  const kind = block.event_kind === 'failure' ? 'failure' : block.event_kind === 'ask' ? 'ask' : 'report';
  closeRows(thread, block.child_id, kind === 'report' ? 'done' : kind === 'ask' ? 'asked' : 'failed');
  const line = document.createElement('div');
  line.className = 'bs-signal' + (kind === 'failure' ? ' is-failure' : '');
  line.dataset.member = block.child_id;
  line.appendChild(avatarOf(sessionOf(block.child_id), 'xs'));
  const text = document.createElement('span');
  text.className = 'bs-signal__text is-clamped';
  text.title = clockTime(atMs);
  const who = document.createElement('b');
  who.textContent = nameOf(crew, block.child_id);
  text.appendChild(who);
  const verb = kind === 'failure'
    ? ' failed: '
    : ' ' + EVENT_VERBS[kind] + ' ' + nameOf(crew, thread.memberId) + ': ';
  text.appendChild(document.createTextNode(verb + block.text));
  text.addEventListener('click', () => text.classList.toggle('is-clamped'));
  line.appendChild(text);
  appendItem(thread, line);
  thread.last = null;
  thread.fold = null;
  if (kind === 'ask') thread.askLines.set(block.child_id, line);
}

function drawAsk(crew, thread, block, atMs) {
  const card = thread.askCard;
  // The answered copy of the question on screen fills it in place.
  if (block.answer && card && !card.answered && card.message === block.message) {
    answerAsk(card, 'You answered: ' + block.answer);
    card.answered = true;
    return;
  }
  if (thread.memberId !== crew.rootId) {
    // A child asks its parent; the next instruction it receives answers it.
    const parent = sessionOf(thread.memberId).parent_id;
    const asked = post(crew, thread, thread.memberId, atMs, plain(block.message), 'asked ' + (parent ? nameOf(crew, parent) : ''));
    thread.pendingAsk = { body: asked.body };
    thread.last = null;
    return;
  }
  // The root asks the reader: its own question, or one a child raised to it,
  // which names the member that first asked it and the way it came up.
  const leaf = block.child_id ? crew.askOrigins.get(block.child_id) || block.child_id : crew.rootId;
  const raised = block.child_id ? thread.askLines.get(block.child_id) : null;
  if (raised) {
    raised.remove();
    thread.askLines.delete(block.child_id);
  }
  const item = document.createElement('div');
  item.className = 'bs-ask';
  item.dataset.member = leaf;
  const who = document.createElement('div');
  who.className = 'bs-ask__who';
  who.appendChild(avatarOf(sessionOf(leaf), 'xs'));
  who.appendChild(document.createTextNode(nameOf(crew, leaf) + ' asks you'));
  item.appendChild(who);
  if (leaf !== crew.rootId) {
    const path = document.createElement('div');
    path.className = 'bs-ask__path';
    path.textContent = pathNames(crew, leaf).reverse().concat('You').join(' › ');
    item.appendChild(path);
  }
  const question = document.createElement('div');
  question.className = 'bs-ask__q';
  question.textContent = block.message;
  item.appendChild(question);
  const next = { body: item, message: block.message, answered: !!block.answer };
  if (block.answer) {
    answerAsk(next, 'You answered: ' + block.answer);
  } else if (leaf !== crew.rootId) {
    const route = document.createElement('div');
    route.className = 'bs-ask__route';
    route.textContent = 'Your answer goes to ' + nameOf(crew, leaf) + ' word for word.';
    item.appendChild(route);
  }
  appendItem(thread, item);
  thread.askCard = next;
  thread.last = null;
  thread.fold = null;
}

function answerAsk(card, text) {
  const route = card.body.querySelector('.bs-ask__route');
  if (route) route.remove();
  const answer = document.createElement('div');
  answer.className = 'bs-ask__answer';
  answer.textContent = text;
  card.body.appendChild(answer);
}

// The root's reply as it streams: a post that the durable text replaces.
function drawDelta(thread, text) {
  let live = thread.live;
  if (!live) {
    const body = plain('');
    body.classList.add('is-live');
    post(opened.crew, thread, thread.memberId, Date.now(), body);
    live = { body };
    thread.live = live;
  }
  live.body.textContent += text;
  followThread(thread);
}

// ---- Tasks ----

const KIND_ICONS = { build: 'build', test: 'test', review: 'review', design: 'design', research: 'research' };
const TASK_GROUPS = [
  ['in_progress', 'In progress'],
  ['todo', 'To do'],
  ['done', 'Done'],
];

function renderTasks(crew) {
  const items = crew.tasks;
  const done = items.filter((item) => item.status === 'done').length;
  const doing = items.filter((item) => item.status === 'in_progress').length;
  tasksCount.textContent = items.length ? done + ' of ' + items.length + ' done' : '';
  tasksProgress.textContent = '';
  tasksProgress.hidden = items.length === 0;
  if (items.length) {
    tasksProgress.setAttribute('aria-label', done + ' of ' + items.length + ' tasks done, ' + doing + ' in progress');
    for (const [className, count] of [['is-done', done], ['is-doing', doing]]) {
      if (!count) continue;
      const bar = document.createElement('i');
      bar.className = className;
      bar.style.width = (100 * count / items.length) + '%';
      tasksProgress.appendChild(bar);
    }
  }
  tasksList.textContent = '';
  if (!items.length) {
    tasksList.appendChild(emptyNote('No tasks yet. The lead writes its plan here as it works.'));
    return;
  }
  const members = treeMembers(crew.rootId);
  for (const [status, title] of TASK_GROUPS) {
    const group = items.filter((item) => (item.status || 'todo') === status);
    if (!group.length) continue;
    const label = document.createElement('div');
    label.className = 'bs-label';
    label.textContent = title + ' · ' + group.length;
    tasksList.appendChild(label);
    for (const item of group) {
      const row = document.createElement('div');
      row.className = 'bs-task' + (status === 'done' ? ' is-done' : '');
      const kind = document.createElement('span');
      kind.className = 'bs-kind';
      kind.appendChild(icon(KIND_ICONS[item.kind] || 'tasks'));
      row.appendChild(kind);
      const text = document.createElement('div');
      const title = document.createElement('div');
      title.className = 'bs-task__title';
      title.textContent = item.content || '';
      text.appendChild(title);
      const owner = item.owner ? members.find((member) => member.id === item.owner) : null;
      const meta = document.createElement('div');
      meta.className = 'bs-task__meta';
      meta.textContent = [item.kind, owner ? memberName(owner, members) : null].filter(Boolean).join(' · ');
      if (meta.textContent) text.appendChild(meta);
      row.appendChild(text);
      if (status === 'done') {
        const pill = document.createElement('span');
        pill.className = 'bs-status';
        pill.dataset.state = 'done';
        pill.textContent = 'Done';
        row.appendChild(pill);
      } else if (owner) {
        row.appendChild(avatarOf(owner, 'sm', crewState(owner)));
      } else {
        row.appendChild(document.createElement('span'));
      }
      tasksList.appendChild(row);
    }
  }
}

function emptyNote(text) {
  const empty = document.createElement('div');
  empty.className = 'bs-empty';
  empty.textContent = text;
  return empty;
}

// ---- Files ----

// Records a change to `path`. Counts add up across a member's edits to one
// file; a later creation or deletion says what the file is now. `pending`
// diff lines come with the call, before its result says how it went.
function noteFileChange(crew, memberId, path, op, added, removed, atMs, lines, fromShell) {
  if (!path) return;
  const entry = crew.files.get(path) || { path, op: null, added: 0, removed: 0 };
  if (op === 'created' || op === 'deleted' || !entry.op || entry.op === 'written') entry.op = op;
  if (op === 'edited' && entry.op === 'written') entry.op = 'edited';
  if (added != null) entry.added += added;
  if (removed != null) entry.removed += removed;
  entry.memberId = memberId;
  entry.atMs = atMs || Date.now();
  entry.fromShell = !!fromShell;
  if (lines) entry.lines = lines;
  crew.files.delete(path);
  crew.files.set(path, entry);
  if (lines) crew.latest = { path, memberId, lines, atMs: entry.atMs };
  renderFiles(crew);
}

const OP_LETTERS = { created: 'C', edited: 'E', written: 'E', deleted: 'D' };

function renderFiles(crew) {
  const entries = [...crew.files.values()].reverse();
  let added = 0;
  let removed = 0;
  for (const entry of entries) {
    added += entry.added;
    removed += entry.removed;
  }
  filesCount.textContent = '';
  if (entries.length) {
    filesCount.appendChild(document.createTextNode(entries.length + (entries.length === 1 ? ' file ' : ' files ')));
    filesCount.appendChild(countEl(added, removed));
  }
  renderNow(crew);
  filesList.textContent = '';
  if (!entries.length) {
    filesList.appendChild(emptyNote('No files changed yet.'));
    return;
  }
  const members = treeMembers(crew.rootId);
  for (const entry of entries) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'bs-file';
    const op = document.createElement('span');
    op.className = 'bs-op';
    op.dataset.op = entry.op === 'written' ? 'edited' : entry.op || 'edited';
    op.textContent = OP_LETTERS[entry.op] || 'E';
    op.title = entry.op || 'edited';
    row.appendChild(op);
    const text = document.createElement('div');
    text.style.minWidth = '0';
    const path = document.createElement('div');
    path.className = 'bs-file__path';
    const slash = entry.path.lastIndexOf('/');
    if (slash >= 0) {
      const dir = document.createElement('span');
      dir.className = 'bs-file__dir';
      dir.textContent = entry.path.slice(0, slash + 1);
      path.appendChild(dir);
    }
    path.appendChild(document.createTextNode(entry.path.slice(slash + 1)));
    text.appendChild(path);
    const who = members.find((member) => member.id === entry.memberId) || sessionOf(entry.memberId);
    const meta = document.createElement('div');
    meta.className = 'bs-file__meta';
    meta.textContent = memberName(who, members.length ? members : [who])
      + (entry.fromShell ? ' · by a command' : '')
      + ' · ' + ago(Math.floor(entry.atMs / 1000));
    text.appendChild(meta);
    row.appendChild(text);
    row.appendChild(countEl(entry.added, entry.removed));
    if (entry.lines && entry.lines.length) {
      row.addEventListener('click', () => {
        const next = row.nextElementSibling;
        if (next && next.classList.contains('bs-diff')) {
          next.remove();
          return;
        }
        row.after(diffCard(entry.path, entry.lines, true).card);
      });
    }
    filesList.appendChild(row);
  }
}

// The newest change, shown the way a person sees a colleague type: the member,
// the file, and the new text ending in a caret while the member still works on
// it. The text is the real change, never invented keystrokes.
function renderNow(crew) {
  filesNow.textContent = '';
  const latest = crew.latest;
  if (!latest) return;
  const member = sessionOf(latest.memberId);
  const state = crewState(member);
  const activity = crew.activity.get(latest.memberId);
  const typing = state === 'working'
    && activity && activity.phase === 'tool_started'
    && ['edit', 'file_write'].includes(activity.name)
    && activity.target === latest.path;
  const label = document.createElement('div');
  label.className = 'bs-label';
  label.textContent = typing ? 'Now' : 'Latest';
  filesNow.appendChild(label);
  const live = document.createElement('div');
  live.className = 'bs-live';
  const who = document.createElement('div');
  who.className = 'bs-live__who';
  who.appendChild(avatarOf(member, 'sm', typing ? 'working' : null));
  const words = document.createElement('span');
  const name = document.createElement('b');
  name.textContent = nameOf(crew, latest.memberId);
  words.appendChild(name);
  words.appendChild(document.createTextNode(typing ? ' is editing ' : ' edited '));
  const file = document.createElement('span');
  file.className = 'bs-mono';
  file.textContent = basename(latest.path);
  words.appendChild(file);
  who.appendChild(words);
  live.appendChild(who);
  const code = document.createElement('div');
  code.className = 'bs-live__code';
  const added = latest.lines.filter(([kind]) => kind === 'add').slice(-3);
  added.forEach(([, text], index) => {
    const line = document.createElement('div');
    line.className = 'is-add';
    line.textContent = text;
    if (typing && index === added.length - 1) {
      const caret = document.createElement('span');
      caret.className = 'bs-caret';
      line.appendChild(caret);
    }
    code.appendChild(line);
  });
  live.appendChild(code);
  filesNow.appendChild(live);
}

// The persona chip on a desktop header names the session's persona with its
// flag; the session view draws it.
export function personaChipContent(persona) {
  const frag = document.createDocumentFragment();
  if (persona) frag.appendChild(flag(persona));
  frag.appendChild(document.createTextNode(personaLabel(persona) || 'No persona'));
  return frag;
}
