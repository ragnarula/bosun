// The pane's screens: the home column with the session list, an open session,
// and the machines, skills and MCP tabs. Exactly one is in the page at a time,
// in the body's normal flow; the others are hidden.

import { $, machinesTab, mcpTab, skillsTab, view } from './dom.js';

export { home, leaveScreen, showScreen };

const home = $('home');
const homeList = home.querySelector('main');
const SCREENS = [home, view, machinesTab, skillsTab, mcpTab];

let current = home;
// The session list's scroll while another screen shows: a hidden box leaves
// the layout, and WebKit drops the scroll of a box it removes.
let listScroll = 0;

function showScreen(screen) {
  if (screen === current) return;
  if (current === home) listScroll = homeList.scrollTop;
  for (const each of SCREENS) each.hidden = each !== screen;
  current = screen;
  if (screen === home) homeList.scrollTop = listScroll;
}

// Returns to the home column, if `screen` is the one showing.
function leaveScreen(screen) {
  if (current === screen) showScreen(home);
}
