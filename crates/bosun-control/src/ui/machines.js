// The machines screen and the health strip, and the node poll that feeds
// them.

import {
  btnMachinesBack,
  healthDot,
  healthStrip,
  healthText,
  machinesList,
  machinesTab,
} from './dom.js';
import { ago, showStatus } from './common.js';
import { renderNodeOptions } from './new-session.js';
import { leaveScreen, showScreen } from './screens.js';

export { nodes, refreshNodes };

let nodes = [];

// One line of node health on the home screen. Tapping it opens the Machines
// tab; the strip stays on the home screen so only the tab, not home, repeats
// node state.
function renderHealth() {
  const up = nodes.filter((node) => node.up).length;
  const down = nodes.length - up;
  healthDot.className = 'health-dot' + (down > 0 ? ' down' : '');
  renderMachines();
  if (nodes.length === 0) {
    healthText.textContent = 'no machines connected';
    return;
  }
  healthText.textContent = down > 0
    ? down + ' of ' + nodes.length + (nodes.length === 1 ? ' machine' : ' machines') + ' down'
    : up + (up === 1 ? ' machine' : ' machines') + ' up';
}

function openMachines() {
  showScreen(machinesTab);
  renderMachines();
}

function closeMachines() {
  leaveScreen(machinesTab);
}

// Down nodes lead, red, with their age; an up node's dot already says "up",
// so its row shows nothing more — the detail is only where it matters.
function renderMachines() {
  machinesList.textContent = '';
  if (nodes.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = 'no nodes connected';
    machinesList.appendChild(empty);
    return;
  }
  const ordered = [...nodes].sort((a, b) => (a.up ? 1 : 0) - (b.up ? 1 : 0));
  for (const node of ordered) {
    const row = document.createElement('div');
    row.className = 'machine-row' + (node.up ? '' : ' down');
    const dot = document.createElement('span');
    dot.className = 'dot ' + (node.up ? 'up' : 'down');
    row.appendChild(dot);
    const name = document.createElement('span');
    name.className = 'machine-name';
    name.textContent = node.name;
    row.appendChild(name);
    if (!node.up) {
      const note = document.createElement('span');
      note.className = 'machine-down';
      note.textContent = 'down · ' + ago(node.last_seen_secs);
      row.appendChild(note);
    }
    machinesList.appendChild(row);
  }
}

healthStrip.addEventListener('click', openMachines);
healthStrip.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    openMachines();
  }
});

btnMachinesBack.addEventListener('click', closeMachines);

let nodesFetching = false;
async function refreshNodes() {
  if (nodesFetching) return;
  nodesFetching = true;
  try {
    const response = await fetch('/nodes');
    if (!response.ok) throw new Error('HTTP ' + response.status);
    nodes = await response.json();
    renderHealth();
    renderNodeOptions();
  } catch (error) {
    showStatus('nodes: ' + error.message);
  } finally {
    nodesFetching = false;
  }
}
