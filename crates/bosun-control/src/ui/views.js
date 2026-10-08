// The open session's four views: Chat, Tasks, Files and Log. The tab bar at the
// bottom switches them on a phone; from 900px Tasks and Files stand in their
// own column, and the switch at the end of the crew row moves between Chat and
// Log. The pane remembers the view the reader last chose.

import { view } from './dom.js';
import { follow } from './scroll.js';

export { VIEWS, currentView, setView };

const VIEWS = ['chat', 'tasks', 'files', 'log'];
const VIEW_KEY = 'bosun.view';

function savedView() {
  try {
    const saved = localStorage.getItem(VIEW_KEY);
    return VIEWS.includes(saved) ? saved : 'chat';
  } catch (error) {
    return 'chat';
  }
}

function currentView() {
  return view.dataset.view || 'chat';
}

function setView(name) {
  if (!VIEWS.includes(name)) return;
  view.dataset.view = name;
  for (const control of view.querySelectorAll('[data-view]')) {
    if (control === view) continue;
    const on = control.dataset.view === name;
    if (control.getAttribute('role') === 'tab') control.setAttribute('aria-selected', String(on));
    else control.setAttribute('aria-pressed', String(on));
  }
  try {
    localStorage.setItem(VIEW_KEY, name);
  } catch (error) {
    // Private mode: the choice lasts for this page load.
  }
  if (name === 'log') follow();
}

for (const control of view.querySelectorAll('button[data-view]')) {
  control.addEventListener('click', () => setView(control.dataset.view));
}

setView(savedView());

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
