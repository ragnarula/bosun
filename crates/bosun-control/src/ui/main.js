// The pane's entry module. It starts the polls and opens the entry the
// address bar names.

import { originEl } from './dom.js';
import { refreshNodes } from './machines.js';
import { refreshSkillRepos } from './skills.js';
import { refreshMcpServers } from './mcp.js';
import { refreshPersonas } from './new-session.js';
import { refreshSessions } from './session-list.js';
import { followHistory, startFromLink } from './history.js';
import { icon } from './signal.js';
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

refreshNodes();
refreshSessions();
refreshPersonas();
refreshSkillRepos();
refreshMcpServers();
setInterval(refreshNodes, 5000);
setInterval(refreshSessions, 3000);
setInterval(refreshPersonas, 30000);
setInterval(refreshSkillRepos, 30000);
setInterval(refreshMcpServers, 30000);
// The pane starts on the entry the address bar names, and follows it from
// there.
window.addEventListener('popstate', followHistory);
startFromLink();
