// The pane's entry module. It starts the polls and opens the entry the
// address bar names.

import { $, newSessionBtn, originEl } from './dom.js';
import { refreshNodes } from './machines.js';
import { refreshSkillRepos } from './skills.js';
import { refreshMcpServers } from './mcp.js';
import { refreshPersonas } from './new-session.js';
import { refreshSessions } from './session-list.js';
import { refreshProjects } from './project-map.js';
import { followHistory, startFromLink } from './history.js';
import { icon } from './signal.js';
import './device.js';
import './views.js';
import './crew-screen.js';
// The report samples each viewport event before viewport.js answers it, so it
// registers its listeners first.
import './viewport-report.js';
import './viewport.js';

originEl.textContent = location.origin;

// Controls in the page name their icon; the pane draws each one in front of
// the control's label.
for (const control of document.querySelectorAll('[data-icon]')) {
  control.prepend(icon(control.dataset.icon));
}

// Each poll and how often it runs, in ms.
const POLLS = [
  [refreshNodes, 5000],
  [refreshSessions, 3000],
  [refreshPersonas, 30000],
  [refreshSkillRepos, 30000],
  [refreshMcpServers, 30000],
  [refreshProjects, 5000],
];

// The polls run while the pane is on screen. A pane that comes back reads
// everything at once, so the reader sees the current state, not the state at
// the next poll. Returns when the first reads are done.
let timers = [];
function startPolls() {
  timers = POLLS.map(([refresh, every]) => window.setInterval(refresh, every));
  return Promise.all(POLLS.map(([refresh]) => refresh()));
}

function stopPolls() {
  for (const timer of timers) window.clearInterval(timer);
  timers = [];
}

document.addEventListener('visibilitychange', () => {
  if (document.hidden) stopPolls();
  else if (!timers.length) startPolls();
});
const firstReads = document.hidden ? Promise.resolve() : startPolls();

// The pane starts on the entry the address bar names, and follows it from
// there.
window.addEventListener('popstate', followHistory);
startFromLink();

// The installed app's shortcuts open it with `?open=new` or `?open=projects`.
// The query comes off the address, so a reload starts on the list, and the
// screen opens once the first reads have filled its choices.
const params = new URLSearchParams(location.search);
const shortcut = params.get('open');
if (shortcut) {
  params.delete('open');
  const query = params.toString();
  history.replaceState(history.state, '', location.pathname + (query ? '?' + query : '') + location.hash);
  firstReads.then(() => {
    if (shortcut === 'new') newSessionBtn.click();
    else if (shortcut === 'projects') $('projects-strip').click();
  });
}
