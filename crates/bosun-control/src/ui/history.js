// The open session's browser history entry, addressed by a `#s=` fragment.

import { closeSession, opened, showSession } from './session-view.js';

export { followHistory, markListEntry, openSession, startFromLink };

// ---- Session history ----
// The pane keeps two history entries: the session list, and the one open
// session above it. The browser's back button and the header's ‹ move between
// them.

// The state a pane entry carries: null while it shows the list, and the
// session id while it shows that session. Another page's state carries no
// `pane` key, and the browser copies the state it is on onto a fragment pasted
// into the address bar, so the key alone does not say an entry shows the
// session on screen.
function paneEntry(id) {
  return { pane: id };
}

function isOwnEntry(state, id) {
  return !!state && Object.prototype.hasOwnProperty.call(state, 'pane') && state.pane === id;
}

// The address of an open session. The browser keeps the fragment, so the
// control plane needs no route for it and a reload or a shared link reopens
// the session.
function sessionLink(id) {
  return '#s=' + encodeURIComponent(id);
}

// The session the address bar names, or null when it names the list.
function sessionFromLink() {
  const match = /^#s=(.+)$/.exec(location.hash);
  if (!match) return null;
  try {
    return decodeURIComponent(match[1]);
  } catch (error) {
    // A hand-edited fragment carrying a bad escape names no session.
    return null;
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
  const id = sessionFromLink();
  // The entry on screen already: rebuilding the view would drop the transcript
  // and reopen the stream for nothing.
  if (id === (opened ? opened.id : null)) return;
  if (!id) {
    closeSession();
    return;
  }
  // An entry the pane did not write — a link loaded from another page, a
  // fragment pasted into the address bar — may sit above anything, so the pane
  // puts its own list entry under the session before opening it: back reaches
  // the list rather than the page, or the session, that came before.
  if (!isOwnEntry(history.state, id)) pushSession(id);
  showSession(id);
}

// The pane's load starts on the session its `#s=` fragment names, or on the
// list. An entry the pane wrote keeps its state, so a reload adds no entry;
// any other becomes the list entry, with the session pushed above it.
function startFromLink() {
  const id = sessionFromLink();
  if (isOwnEntry(history.state, id)) {
    if (id) showSession(id);
    return;
  }
  if (!id) {
    markListEntry();
    return;
  }
  pushSession(id);
  showSession(id);
}
