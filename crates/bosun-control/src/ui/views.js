// The open session's views: Chat, Crew, Tasks and Files on the tab bar, and
// Log, which the lead's node in Crew opens. From 900px Crew, Tasks and Files
// share their own column, switched by chips, and the switch at the end of the
// crew row moves between Chat and Log. The pane remembers the views the reader
// last chose.

import { view } from './dom.js';
import { follow } from './scroll.js';
import { closeMember } from './history.js';

export { VIEWS, currentView, setView };

const VIEWS = ['chat', 'crew', 'tasks', 'files', 'log'];
const VIEW_KEY = 'bosun.view';
const SIDES = ['crew', 'tasks', 'files'];
const SIDE_KEY = 'bosun.side';

function saved(key, choices, fallback) {
  try {
    const value = localStorage.getItem(key);
    return choices.includes(value) ? value : fallback;
  } catch (error) {
    return fallback;
  }
}

function save(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch (error) {
    // Private mode: the choice lasts for this page load.
  }
}

function currentView() {
  return view.dataset.view || 'chat';
}

function setView(name) {
  if (!VIEWS.includes(name)) return;
  markView(name);
  if (name === 'log') follow();
}

// The view's controls and the saved choice, without scrolling: the pane's
// first view is marked while the other modules are still loading.
function markView(name) {
  view.dataset.view = name;
  for (const control of view.querySelectorAll('[data-view]')) {
    if (control === view) continue;
    const on = control.dataset.view === name;
    if (control.getAttribute('role') === 'tab') control.setAttribute('aria-selected', String(on));
    else control.setAttribute('aria-pressed', String(on));
  }
  save(VIEW_KEY, name);
  // Crew, Tasks or Files picked on a phone is the column a wide screen shows.
  if (SIDES.includes(name)) setSide(name);
}

function setSide(name) {
  view.dataset.side = name;
  for (const control of view.querySelectorAll('[data-side]')) {
    if (control === view) continue;
    control.setAttribute('aria-pressed', String(control.dataset.side === name));
  }
  save(SIDE_KEY, name);
}

// A view picked while a member's screen covers the session leaves the screen.
for (const control of view.querySelectorAll('button[data-view]')) {
  control.addEventListener('click', () => {
    if (view.dataset.member) closeMember();
    setView(control.dataset.view);
  });
}

for (const control of view.querySelectorAll('button[data-side]')) {
  control.addEventListener('click', () => setSide(control.dataset.side));
}

setSide(saved(SIDE_KEY, SIDES, 'crew'));
markView(saved(VIEW_KEY, VIEWS, 'chat'));

// Whether a keyboard holds part of the screen: a text field has the focus and
// the visual viewport is clearly shorter than the layout viewport.
function syncKeyboard() {
  const viewport = window.visualViewport;
  const field = document.activeElement && document.activeElement.matches('textarea, input');
  const up = !!viewport && !!field && viewport.height < window.innerHeight - 80;
  document.body.classList.toggle('keyboard-up', up);
}

if (window.visualViewport) window.visualViewport.addEventListener('resize', syncKeyboard);
window.addEventListener('focusin', syncKeyboard);
window.addEventListener('focusout', syncKeyboard);
