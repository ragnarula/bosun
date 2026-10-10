// The open session's browser history entries, addressed by a `#s=` fragment,
// with `&m=` when a crew member's screen is open over the session.

import { closeSession, opened, showSession } from './session-view.js';
import { hideMember, showMember, shownMember } from './crew.js';

export { closeMember, followHistory, markListEntry, openMember, openSession, openTreeAt, startFromLink };

// ---- Session history ----
// The pane keeps the session list, the one open session above it, and an
// entry for each crew member's screen opened over that session. The browser's
// back button and the headers' ‹ move between them.

// The state a pane entry carries: null while it shows the list, and the
// session id while it shows that session. Another page's state carries no
// `pane` key, and the browser copies the state it is on onto a fragment pasted
// into the address bar, so the key alone does not say an entry shows the
// session on screen.
function paneEntry(id, member) {
  return { pane: id, member: member || null };
}

function isOwnEntry(state, id) {
  return !!state && Object.prototype.hasOwnProperty.call(state, 'pane') && state.pane === id;
}

// The address of an open session. The browser keeps the fragment, so the
// control plane needs no route for it and a reload or a shared link reopens
// the session.
function sessionLink(id, member) {
  return '#s=' + encodeURIComponent(id) + (member ? '&m=' + encodeURIComponent(member) : '');
}

// The session and the crew member the address bar names. The session is null
// when it names the list, and the member is null when no member's screen is
// open.
function targetFromLink() {
  const match = /^#s=([^&]+)(?:&m=(.+))?$/.exec(location.hash);
  if (!match) return { id: null, member: null };
  try {
    return {
      id: decodeURIComponent(match[1]),
      member: match[2] ? decodeURIComponent(match[2]) : null,
    };
  } catch (error) {
    // A hand-edited fragment carrying a bad escape names no session.
    return { id: null, member: null };
  }
}

// The current entry becomes the list entry, with the fragment off, so the
// address bar names the list the pane shows.
function markListEntry() {
  history.replaceState(paneEntry(null), '', location.pathname + location.search);
}

// Pushes an open session's entry above the list: the current entry becomes the
// list entry, and the session is written on top of it, so back reaches the
// list rather than the page before the pane.
function pushSession(id) {
  markListEntry();
  history.pushState(paneEntry(id), '', sessionLink(id));
}

// The pane's one way into a session: the entry and the fragment are written
// here, so the header's ‹ and the browser's back leave by the same path.
function openSession(id) {
  // The session already open owns the entry on screen, and the new session
  // takes it, so the list stays the entry behind a session. Anywhere else —
  // the list, or an entry the pane did not write — the list entry goes under
  // the session instead.
  if (opened && isOwnEntry(history.state, opened.id)) {
    history.replaceState(paneEntry(id), '', sessionLink(id));
  } else {
    pushSession(id);
  }
  showSession(id);
}

// The pane follows the address bar: the session its fragment names, or the
// list. Back, forward, and a fragment pasted into the address bar all arrive
// here.
function followHistory() {
  const { id, member } = targetFromLink();
  // The session on screen already: rebuilding the view would drop the
  // transcript and reopen the stream for nothing, so only the member's screen
  // follows the entry.
  if (id && id === (opened ? opened.id : null)) {
    if (member) showMember(member);
    else hideMember();
    return;
  }
  if (!id) {
    closeSession();
    return;
  }
  // An entry the pane did not write — a link loaded from another page, a
  // fragment pasted into the address bar — may sit above anything, so the pane
  // puts its own list entry under the session before opening it: back reaches
  // the list rather than the page, or the session, that came before.
  if (!isOwnEntry(history.state, id)) {
    pushSession(id);
    if (member) history.pushState(paneEntry(id, member), '', sessionLink(id, member));
  }
  showSession(id);
  if (member) showMember(member);
}

// Opens a crew member's screen over the open session, on an entry of its own,
// so back returns to the screen the reader opened it from.
function openMember(id) {
  if (!opened || !id || id === shownMember()) return;
  history.pushState(paneEntry(opened.id, id), '', sessionLink(opened.id, id));
  showMember(id);
}

// Leaves the member's screen by the entry it opened on, or, on an entry the
// pane did not write, by closing it in place.
function closeMember() {
  if (history.state && history.state.member && opened && isOwnEntry(history.state, opened.id)) {
    history.back();
  } else {
    if (opened) history.replaceState(paneEntry(opened.id), '', sessionLink(opened.id));
    hideMember();
  }
}

// A child's address opens its tree: the root's session, with the child's
// screen over it, on the entry that named the child.
function openTreeAt(rootId, memberId) {
  history.replaceState(paneEntry(rootId, memberId), '', sessionLink(rootId, memberId));
  showSession(rootId);
  showMember(memberId);
}

// The pane's load starts on the session its `#s=` fragment names, or on the
// list. An entry the pane wrote keeps its state, so a reload adds no entry;
// any other becomes the list entry, with the session pushed above it.
function startFromLink() {
  const { id, member } = targetFromLink();
  if (isOwnEntry(history.state, id)) {
    if (id) showSession(id);
    if (id && member) showMember(member);
    return;
  }
  if (!id) {
    markListEntry();
    return;
  }
  pushSession(id);
  if (member) history.pushState(paneEntry(id, member), '', sessionLink(id, member));
  showSession(id);
  if (member) showMember(member);
}
