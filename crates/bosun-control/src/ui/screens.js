// The pane's screens: the home column with the session list, an open session,
// and the projects, machines, skills, MCP and crew tabs. On a phone exactly
// one is in the page at a time, in the body's normal flow; the others are
// hidden. From 900px
// an open session keeps the home column beside it as a rail, so the reader
// moves between sessions without leaving the one on screen.

import { $, crewTab, isWide, machinesTab, mcpTab, projectsTab, skillsTab, view } from './dom.js';

export { home, leaveScreen, showScreen };

const home = $('home');
const homeList = home.querySelector('main');
const SCREENS = [home, view, machinesTab, skillsTab, mcpTab, crewTab, projectsTab];

let current = home;
// The session list's scroll while another screen shows: a hidden box leaves
// the layout, and WebKit drops the scroll of a box it removes.
let listScroll = 0;

function showScreen(screen) {
  if (screen === current) return;
  if (!home.hidden) listScroll = homeList.scrollTop;
  current = screen;
  layout();
  if (!home.hidden) homeList.scrollTop = listScroll;
}

// Shows the current screen, and the home column beside an open session on a
// wide screen.
function layout() {
  const rail = current === view && isWide();
  for (const each of SCREENS) each.hidden = each !== current && !(rail && each === home);
  document.body.classList.toggle('with-rail', rail);
}

// Returns to the home column, if `screen` is the one showing.
function leaveScreen(screen) {
  if (current === screen) showScreen(home);
}

window.addEventListener('resize', layout);
