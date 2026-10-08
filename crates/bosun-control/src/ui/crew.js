// The crew: every session in the open session's tree, followed on one stream.
// It draws the crew strip, and the Chat, Tasks and Files views. The Log view
// is the open session's own transcript, which session-view.js draws.

import { $ } from './dom.js';
import { ago, showStatus } from './common.js';
import { sessions } from './session-list.js';
import { personas } from './new-session.js';
import { opened } from './session-view.js';
import { renderMarkdown } from './markdown.js';
import { avatar, flag, icon, memberName, personaLabel } from './signal.js';
import { USER_REJECTED_TEXT } from './composer.js';
import { closeChildPanel, followChild } from './subagents.js';
import { follow } from './scroll.js';

export {
  activityCaption,
  avatarOf,
  closeCrew,
  crewState,
  openCrew,
  renderCrew,
  treeMembers,
};

const crewStrip = $('crew-strip');
const chatList = $('chat-list');
const chatView = $('chat-view');
const tasksList = $('tasks-list');
const tasksProgress = $('tasks-progress');
const tasksCount = $('tasks-count');
const filesNow = $('files-now');
const filesList = $('files-list');
const filesCount = $('files-count');

// How many chat items the view keeps. The store holds everything, and the Log
// view reads it back; the chat is the crew's recent conversation.
const MAX_CHAT_ITEMS = 600;
// How many diff lines a chat card shows before the reader taps for the rest.
const CARD_LINES = 6;
// How many messages the tree stream replays when the crew opens.
const TREE_TAIL = 120;

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

// What a member's ring shows. A question waiting on the user is the root's;
// a working loop, a parent waiting on its children, a crash and a member with
// nothing to do each have their own.
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

// One line for a member under its face.
function memberCaption(session, state, crew) {
  switch (state) {
    case 'stopped': return 'stopped';
    case 'needs-you': return 'asks you';
    case 'waiting': return 'waiting on ' + liveChildren(session);
    case 'working': {
      const activity = crew.activity.get(session.id) || session.activity;
      return activityCaption(activity) || 'working';
    }
    default:
      return session.parent_id ? 'done' : 'ready';
  }
}

// ---- Opening and closing ----

function newCrew(rootId) {
  return {
    rootId,
    es: null,
    // Each member's newest activity from the stream, by session id.
    activity: new Map(),
    // The chat's state: the last item a post can join, the open fold of tool
    // calls, and each call's card or line by call id.
    chat: { last: null, fold: null, calls: new Map(), live: null, stick: true },
    // The task list, as the root's newest `todos` event wrote it.
    tasks: [],
    // Each changed file by path: what happened to it, its lines, who, when,
    // and its newest diff lines.
    files: new Map(),
    // The newest file change, for the Files view's "now" block.
    latest: null,
    // The member whose posts the chat shows, or null for everyone.
    filter: null,
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
  renderCrew();
}

function closeCrew(s) {
  if (s && s.crew) {
    if (s.crew.es) s.crew.es.close();
    s.crew = null;
  }
  crewStrip.textContent = '';
  chatList.textContent = '';
  tasksList.textContent = '';
  filesList.textContent = '';
  filesNow.textContent = '';
}

// ---- The crew strip ----

// Redraws the strip and the views that name members, from the newest session
// list. The session poll calls this every few seconds, and the stream calls
// it when a member's activity changes.
function renderCrew() {
  const s = opened;
  if (!s || !s.crew) return;
  const crew = s.crew;
  const members = treeMembers(crew.rootId);
  crewStrip.textContent = '';
  crewStrip.hidden = members.length === 0;
  for (const session of members) {
    const state = crewState(session);
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'bs-member' + (crew.filter === session.id ? ' is-filtered' : '');
    button.dataset.member = session.id;
    button.appendChild(avatarOf(session, 'md', state));
    const name = document.createElement('span');
    name.className = 'bs-member__name';
    name.textContent = memberName(session, members);
    button.appendChild(name);
    const now = document.createElement('span');
    now.className = 'bs-member__now';
    now.textContent = memberCaption(session, state, crew);
    button.appendChild(now);
    button.addEventListener('click', () => focusMember(crew, session));
    crewStrip.appendChild(button);
  }
  renderTasks(crew);
  renderFiles(crew);
  // The strip can change height as members join, which moves the bottom of
  // the transcript and the chat; a reader at the newest line stays there.
  follow();
  followChat(crew);
}

// A tap on a member shows only that member's part of the chat, and in the Log
// view follows a child's own transcript in the panel. A second tap shows the
// whole crew again.
function focusMember(crew, session) {
  crew.filter = crew.filter === session.id ? null : session.id;
  applyFilter(crew);
  if (document.getElementById('session-view').dataset.view === 'log') {
    if (session.parent_id && crew.filter) followChild(session.id);
    else closeChildPanel();
  }
  renderCrew();
}

function applyFilter(crew) {
  for (const item of chatList.children) {
    item.hidden = !!crew.filter
      && item.dataset.member !== crew.filter
      && item.dataset.to !== crew.filter;
  }
}

// ---- The tree stream ----

function handleTreeFrame(crew, frame) {
  if (Object.prototype.hasOwnProperty.call(frame, 'delta')) {
    drawDelta(crew, frame.session_id, frame.delta);
    return;
  }
  if (!frame.event) return;
  const event = frame.event;
  const member = frame.session_id;
  switch (event.kind) {
    case 'message':
      drawChatMessage(crew, member, event.message, event.at_ms);
      break;
    case 'todos':
      if (member === crew.rootId) {
        crew.tasks = Array.isArray(event.items) ? event.items : [];
        renderTasks(crew);
      }
      break;
    case 'activity': {
      crew.activity.set(member, event);
      if (event.phase === 'tool_started' || event.phase === 'tool_finished') renderCrew();
      break;
    }
    default:
      break;
  }
}

// ---- The chat ----

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

// Adds an item to the chat, dropping the oldest past the cap, and keeps the
// newest in view for a reader who is at the bottom.
function appendItem(crew, item) {
  chatList.appendChild(item);
  while (chatList.children.length > MAX_CHAT_ITEMS) chatList.removeChild(chatList.firstChild);
  if (crew.filter) {
    item.hidden = item.dataset.member !== crew.filter && item.dataset.to !== crew.filter;
  }
  followChat(crew);
}

function followChat(crew) {
  if (crew.chat.stick) chatView.scrollTop = chatView.scrollHeight;
}

chatView.addEventListener('scroll', () => {
  const crew = opened && opened.crew;
  if (!crew) return;
  crew.chat.stick = chatView.scrollTop + chatView.clientHeight >= chatView.scrollHeight - 40;
});

// A post by a member: avatar, a head naming the member and whom the post is
// to, and the body. A post by the member who wrote the item above joins that
// item instead of repeating the head.
function post(crew, memberId, atMs, body, to) {
  const last = crew.chat.last;
  if (last && last.memberId === memberId && last.to === to && !crew.chat.fold) {
    last.body.appendChild(body);
    followChat(crew);
    return last;
  }
  const item = document.createElement('div');
  item.className = 'bs-msg';
  item.dataset.member = memberId;
  if (to) item.dataset.to = to;
  item.appendChild(avatarOf(sessionOf(memberId), 'sm'));
  const column = document.createElement('div');
  column.className = 'bs-msg__column';
  const head = document.createElement('div');
  head.className = 'bs-msg__head';
  head.appendChild(document.createTextNode(nameOf(crew, memberId)));
  if (to) {
    const toEl = document.createElement('span');
    toEl.className = 'bs-msg__to';
    toEl.textContent = '→ ' + to;
    head.appendChild(toEl);
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
  appendItem(crew, item);
  crew.chat.last = { memberId, to, body: bodyEl };
  crew.chat.fold = null;
  return crew.chat.last;
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
function yourMessage(crew, text, atMs) {
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
  appendItem(crew, item);
  crew.chat.last = null;
  crew.chat.fold = null;
}

// A note across the chat: a cleared context, a summary, a failure.
function note(crew, text, kind, memberId) {
  const item = document.createElement('div');
  item.className = 'chat-note' + (kind ? ' ' + kind : '');
  if (memberId) item.dataset.member = memberId;
  item.textContent = text;
  appendItem(crew, item);
  crew.chat.last = null;
  crew.chat.fold = null;
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

// A tool call that is neither a message nor a file change joins the member's
// folded line: one quiet line per run of calls. A tap lists them.
function foldCall(crew, memberId, name, args) {
  let fold = crew.chat.fold;
  if (!fold || fold.memberId !== memberId) {
    const item = document.createElement('div');
    item.className = 'bs-activity-wrap';
    item.dataset.member = memberId;
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
    appendItem(crew, item);
    fold = { memberId, counts: {}, label, list };
    crew.chat.fold = fold;
    crew.chat.last = null;
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

// A message from the tree, by the member that wrote it. The chat shows the
// conversation and the file changes; the Log view keeps everything else.
function drawChatMessage(crew, memberId, message, atMs) {
  const block = message.block;
  const isRoot = memberId === crew.rootId;
  if (message.role === 'user') {
    switch (block.kind) {
      case 'text':
        // A child's user-role text is always its parent's instructions or an
        // answer routed down to it, and the parent's call or the answered
        // question already shows it.
        if (!isRoot) return;
        if (block.text === USER_REJECTED_TEXT) note(crew, 'You dismissed the question', 'muted');
        else yourMessage(crew, block.text, atMs);
        return;
      case 'tool_result':
        drawResult(crew, memberId, block, atMs);
        return;
      case 'child_event':
        if (block.event_kind === 'failure') {
          note(crew, nameOf(crew, block.child_id) + ' failed: ' + block.text, 'error', block.child_id);
        }
        return;
      default:
        return;
    }
  }
  switch (block.kind) {
    case 'text': {
      if (!block.text) return;
      const live = crew.chat.live;
      if (live && live.memberId === memberId) {
        live.body.replaceWith(markdown(block.text));
        crew.chat.live = null;
        return;
      }
      post(crew, memberId, atMs, markdown(block.text));
      return;
    }
    case 'tool_call':
      drawCall(crew, memberId, block, atMs);
      return;
    case 'ask':
      drawAsk(crew, memberId, block, atMs);
      return;
    case 'summary':
      note(crew, nameOf(crew, memberId) + ' compacted its notes', 'muted', memberId);
      return;
    case 'context_cleared':
      note(crew, nameOf(crew, memberId) + ' started fresh: ' + (block.reason || ''), 'muted', memberId);
      return;
    default:
      return;
  }
}

function drawCall(crew, memberId, block, atMs) {
  const args = block.args || {};
  switch (block.name) {
    case 'spawn': {
      const to = personaLabel(args.persona) || 'Agent';
      post(crew, memberId, atMs, markdown(String(args.instructions || '')), to);
      return;
    }
    case 'message_child': {
      const to = nameOf(crew, args.id);
      const entry = post(crew, memberId, atMs, markdown(String(args.text || '')), to);
      if (entry) entry.toId = args.id;
      return;
    }
    case 'edit':
    case 'file_write': {
      const path = String(args.path || '');
      const { card, counts } = diffCard(path, diffLines(block.name, args), false);
      const last = crew.chat.last;
      if (last && last.memberId === memberId && !last.to && !crew.chat.fold) {
        last.body.appendChild(card);
        followChat(crew);
      } else {
        post(crew, memberId, atMs, card);
      }
      crew.chat.calls.set(block.id, { name: block.name, path, args, counts, memberId, atMs });
      noteFileChange(crew, memberId, path, block.name === 'file_write' ? 'written' : 'edited', null, null, atMs, diffLines(block.name, args));
      return;
    }
    case 'ask':
      // The question itself arrives as its own message.
      return;
    default:
      foldCall(crew, memberId, block.name, args);
  }
}

function drawResult(crew, memberId, block, atMs) {
  const call = crew.chat.calls.get(block.id);
  if (call) {
    crew.chat.calls.delete(block.id);
    const content = block.content || {};
    if (block.is_error) {
      call.counts.className = 'bs-count error';
      call.counts.textContent = 'failed';
      return;
    }
    call.counts.replaceWith(countEl(content.added, content.removed));
    const op = call.name === 'file_write' && content.created ? 'created' : 'edited';
    noteFileChange(crew, call.memberId, call.path, op, content.added, content.removed, call.atMs, null);
    return;
  }
  // A shell command names the files its run changed.
  const files = block.content && Array.isArray(block.content.files) ? block.content.files : [];
  for (const file of files) {
    noteFileChange(crew, memberId, file.path, file.op, null, null, atMs, null, true);
  }
}

function drawAsk(crew, memberId, block, atMs) {
  const isRoot = memberId === crew.rootId;
  if (!isRoot) {
    // A child asks its parent: the question is a message to the parent.
    const parent = sessionOf(memberId).parent_id;
    post(crew, memberId, atMs, plain(block.message), parent ? nameOf(crew, parent) : null);
    return;
  }
  const item = document.createElement('div');
  item.className = 'bs-ask';
  item.dataset.member = block.child_id || memberId;
  const who = document.createElement('div');
  who.className = 'bs-ask__who';
  const asker = block.child_id || memberId;
  who.appendChild(avatarOf(sessionOf(asker), 'xs'));
  who.appendChild(document.createTextNode(nameOf(crew, asker) + ' asks you'));
  item.appendChild(who);
  const question = document.createElement('div');
  question.className = 'bs-ask__q';
  question.textContent = block.message;
  item.appendChild(question);
  if (block.answer) {
    const answer = document.createElement('div');
    answer.className = 'bs-ask__answer';
    answer.textContent = 'You answered: ' + block.answer;
    item.appendChild(answer);
  }
  appendItem(crew, item);
  crew.chat.last = null;
  crew.chat.fold = null;
}

// The root's reply as it streams: a post that the durable text replaces.
function drawDelta(crew, memberId, text) {
  let live = crew.chat.live;
  if (!live || live.memberId !== memberId) {
    const body = plain('');
    body.classList.add('is-live');
    post(crew, memberId, Date.now(), body);
    live = { memberId, body };
    crew.chat.live = live;
  }
  live.body.textContent += text;
  followChat(crew);
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
