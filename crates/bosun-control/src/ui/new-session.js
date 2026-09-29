// The new-session sheet: the node, directory, persona and MCP choices, and
// the recent settings.

import {
  btnBrowse,
  btnDirClose,
  btnDirUp,
  btnDirUse,
  cancelNewSessionBtn,
  dirBrowser,
  dirEntries,
  dirPathEl,
  formDir,
  formNode,
  formPersona,
  isNarrow,
  mcpChips,
  newSessionBtn,
  newSessionForm,
  nodeChips,
  personaChips,
  personaName,
  recentList,
} from './dom.js';
import { ago, post, showStatus, showToast, toastError, toastOk } from './common.js';
import { nodes } from './machines.js';
import { mcpServers } from './mcp.js';
import { refreshSessions } from './session-list.js';
import { openSession } from './history.js';

export { personas, refreshPersonas, renderMcpOptions, renderNodeOptions };

let nodeOptionsSignature = null;
let personas = [];

let dirNode = null;   // the node the directory browser is looking at
let dirPath = null;   // null = the node's browse roots
let dirListing = null;

// The composer's chosen directory is a plain span, not an input: `value`
// keeps the submit-facing property, textContent is what the user sees, and
// an empty setter restores the `:empty::before` placeholder.
function setDir(value) {
  formDir.value = value;
  formDir.textContent = value;
}

let personaSelected = ''; // the chip persona picked in the composer
let personaInitialized = false; // whether the last-used persona has been applied
// The composer's node pick lives here, not in the hidden #form-node select:
// the select is cleared of options below the many-nodes threshold, so its
// value cannot hold the pick. Mirrors personaSelected.
let selectedNode = '';

// The MCP servers picked in the composer. The submit handler reads this set
// directly; the chips are the only control, mirroring the persona picker.
let mcpSelected = new Set();

function renderMcpOptions() {
  const enabled = mcpServers.filter((server) => server.enabled);
  const names = new Set(enabled.map((server) => server.name));
  for (const name of mcpSelected) {
    if (!names.has(name)) mcpSelected.delete(name);
  }
  mcpChips.textContent = '';
  if (enabled.length === 0) {
    mcpChips.hidden = true;
    return;
  }
  mcpChips.hidden = false;
  for (const server of enabled) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'chip';
    chip.textContent = server.name;
    if (mcpSelected.has(server.name)) chip.classList.add('selected');
    chip.addEventListener('click', () => {
      if (mcpSelected.has(server.name)) {
        mcpSelected.delete(server.name);
      } else {
        mcpSelected.add(server.name);
      }
      renderMcpOptions();
    });
    mcpChips.appendChild(chip);
  }
}

function renderNodeOptions() {
  const signature = nodes.map((node) => node.name + (node.up ? 'up' : 'down')).join(',');
  if (signature === nodeOptionsSignature) return;
  nodeOptionsSignature = signature;
  const up = nodes.filter((node) => node.up);
  const tooMany = up.length > NODE_CHIP_LIMIT;
  if (tooMany) {
    formNode.hidden = false;
    nodeChips.hidden = true;
    nodeChips.textContent = '';
    formNode.textContent = '';
    const placeholder = document.createElement('option');
    placeholder.value = '';
    placeholder.textContent = 'choose a node';
    formNode.appendChild(placeholder);
    for (const node of nodes) {
      const option = document.createElement('option');
      option.value = node.name;
      option.textContent = node.name + (node.up ? '' : ' (down)');
      formNode.appendChild(option);
    }
    if (selectedNode && nodes.some((node) => node.name === selectedNode)) {
      formNode.value = selectedNode;
    }
    return;
  }
  formNode.hidden = true;
  formNode.textContent = '';
  if (up.length === 1) {
    // One live node is pre-selected; a pick from a node that just dropped is
    // dropped with its directory, exactly like a manual switch.
    if (selectedNode !== up[0].name) onNodeChange();
    selectedNode = up[0].name;
  } else if (selectedNode && !nodes.some((node) => node.name === selectedNode)) {
    // The picked node vanished: reuse the change listener to drop the stale
    // dir pick.
    onNodeChange();
    selectedNode = '';
  }
  renderNodeChips();
}

function renderNodeChips() {
  nodeChips.hidden = false;
  nodeChips.textContent = '';
  for (const node of nodes) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'chip';
    chip.textContent = node.name;
    if (!node.up) {
      chip.className = 'chip down';
      chip.disabled = true;
    }
    if (node.up && node.name === selectedNode) chip.classList.add('selected');
    chip.addEventListener('click', () => {
      // A picked directory belongs to one node; switching nodes drops it via
      // the same change listener the select keeps firing.
      if (selectedNode !== node.name) onNodeChange();
      selectedNode = node.name;
      if (formNode.value !== node.name) {
        // Keep the hidden select (shown only in the many-nodes case) in
        // sync so a submit/browse after a re-render reads the same node.
        formNode.value = node.name;
      }
      renderNodeChips();
    });
    nodeChips.appendChild(chip);
  }
}

function renderPersonaOptions() {
  formPersona.textContent = '';
  const placeholder = document.createElement('option');
  placeholder.value = '';
  placeholder.textContent = 'default persona';
  formPersona.appendChild(placeholder);
  const names = [];
  for (const persona of personas) {
    const option = document.createElement('option');
    option.value = persona.name;
    option.textContent = persona.name + (persona.default ? ' (default)' : '');
    formPersona.appendChild(option);
    names.push(persona.name);
  }
  if (personaSelected && !names.includes(personaSelected)) {
    // The picked persona vanished from the configured catalog: fall back to
    // default rather than leaving a dead selection on the wire.
    personaSelected = '';
  }
  formPersona.value = personaSelected;
  renderPersonaChips();

  const selected = personaName.value;
  personaName.textContent = '';
  const switchPlaceholder = document.createElement('option');
  switchPlaceholder.value = '';
  switchPlaceholder.textContent = 'switch persona…';
  personaName.appendChild(switchPlaceholder);
  for (const persona of personas) {
    const option = document.createElement('option');
    option.value = persona.name;
    option.textContent = persona.name;
    personaName.appendChild(option);
  }
  if (selected) personaName.value = selected;
}

function renderPersonaChips() {
  personaChips.textContent = '';
  personaChips.appendChild(personaChip('', 'default'));
  for (const persona of personas) {
    personaChips.appendChild(personaChip(persona.name, persona.name));
  }
}

function personaChip(name, label) {
  const chip = document.createElement('button');
  chip.type = 'button';
  chip.className = 'chip';
  chip.textContent = label;
  if (name === formPersona.value) chip.classList.add('selected');
  chip.addEventListener('click', () => {
    personaSelected = name;
    renderPersonaOptions();
  });
  return chip;
}

let personasFetching = false;
async function refreshPersonas() {
  if (personasFetching) return;
  personasFetching = true;
  try {
    const response = await fetch('/personas');
    if (!response.ok) throw new Error('HTTP ' + response.status);
    personas = await response.json();
    if (!personaInitialized) {
      // Apply the last-used persona now that the catalog is known: the load
      // happens after the fetch resolves, so a stale or vanished name falls
      // back to default inside renderPersonaOptions.
      const last = loadLastPersona();
      personaSelected = personas.some((persona) => persona.name === last) ? last : '';
      personaInitialized = true;
    }
    renderPersonaOptions();
  } catch (error) {
    showStatus('personas: ' + error.message);
  } finally {
    personasFetching = false;
  }
}

// Recently-created session settings, kept in localStorage so the composer
// can offer them back. Only the choices the sheet collects are stored:
// node, dir, persona and the time the session was started. A new session's
// settings go to the front; duplicates drop, so a reuse moves the entry
// rather than repeating it.
const RECENT_KEY = 'bosun.recent-sessions';
const RECENT_LIMIT = 5;
// Node picker: chips while few nodes are up, a select when the list grows
// past this. The constant is `~4 up nodes` from the story.
const NODE_CHIP_LIMIT = 4;
// The last persona a session started with, persisted so the sheet can
// pre-select it. "" means default; cleared when the user starts with the
// default persona.
const LAST_PERSONA_KEY = 'bosun.last-persona';

function loadLastPersona() {
  try {
    return localStorage.getItem(LAST_PERSONA_KEY) || '';
  } catch (error) {
    return '';
  }
}

function rememberLastPersona(persona) {
  try {
    localStorage.setItem(LAST_PERSONA_KEY, persona);
  } catch (error) {
    // Private mode or a full quota: the last persona is forgotten for this
    // page load.
  }
}

function clearLastPersona() {
  try {
    localStorage.removeItem(LAST_PERSONA_KEY);
  } catch (error) {
  }
}

// Re-reads the node roster and rebuilds the picker. Force-disables the
// signature shortcut so a node that came up or went down while the sheet was
// closed is reflected in the chips the moment it opens.
function refreshNodeOptions() {
  nodeOptionsSignature = null;
  renderNodeOptions();
}

function loadRecents() {
  try {
    const parsed = JSON.parse(localStorage.getItem(RECENT_KEY) || '[]');
    return Array.isArray(parsed) ? parsed : [];
  } catch (error) {
    return [];
  }
}

function saveRecents(recents) {
  try {
    localStorage.setItem(RECENT_KEY, JSON.stringify(recents.slice(0, RECENT_LIMIT)));
  } catch (error) {
    // Private mode or a full quota: the recent list degrades to in-memory
    // for this page load.
  }
}

function rememberCurrentSettings() {
  const persona = formPersona.value.trim();
  const recents = loadRecents().filter((entry) =>
    !(entry.node === selectedNode && entry.dir === formDir.value.trim()
      && entry.persona === persona)
  );
  recents.unshift({
    node: selectedNode,
    dir: formDir.value.trim(),
    persona,
    time: Math.floor(Date.now() / 1000),
  });
  saveRecents(recents);
  if (newSessionForm.hidden) return;
  // A dead node is never offered: a recent whose node is gone is dropped
  // rather than revived as a broken choice.
  saveRecents(loadRecents().filter((entry) =>
    nodes.some((node) => node.name === entry.node)
  ));
  renderRecents();
}

function useRecent(entry) {
  // A dead node is never offered: a recent whose node is gone is re-offered
  // against the first live node with the directory dropped, so the reuse
  // never lands on a broken choice. The persona survives the fallback.
  const live = nodes.filter((node) => node.up);
  const node = nodes.some((node) => node.name === entry.node && node.up)
    ? entry.node
    : live.length > 0 ? live[0].name : '';
  if (node !== entry.node) {
    setDir('');
    toastOk('node ' + entry.node + ' is gone — pick a directory');
  } else {
    setDir(entry.dir);
  }
  selectedNode = node;
  // The composer's persona chips are driven by personaSelected; renderPersonaOptions
  // overwrites the select with it, so the recent's persona must be applied to the
  // chip state, not just the select value.
  personaSelected = personas.some((persona) => persona.name === entry.persona)
    ? entry.persona
    : '';
  formPersona.value = personaSelected;
  // When the node roster has not changed since the last render, the signature
  // shortcut would skip rebuilds; force both pickers to re-render so the chips
  // reflect the recent's value instead of the previous selection.
  renderNodeOptions();
  renderNodeChips();
  renderPersonaOptions();
  dirBrowser.hidden = true;
  if (isNarrow()) {
    newSessionForm.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }
  renderRecents();
}

function renderRecents() {
  const recents = loadRecents()
    .filter((entry) => entry.node && entry.dir)
    .map((entry) => {
      // Legacy entries carry the removed prompt field; drop it so no recent
      // row shows text the composer no longer has.
      const copy = Object.assign({}, entry);
      if (Object.prototype.hasOwnProperty.call(copy, 'prompt')) delete copy.prompt;
      return copy;
    });
  if (recents.length === 0 || newSessionForm.hidden) {
    recentList.hidden = true;
    return;
  }
  recentList.hidden = false;
  recentList.textContent = '';
  recentList.appendChild(recentTitle());
  for (const entry of recents) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'recent-entry';
    button.title = 'Use ' + entry.node + ': ' + entry.dir;
    const node = document.createElement('span');
    node.className = 'recent-node';
    node.textContent = entry.node;
    button.appendChild(node);
    const dir = document.createElement('span');
    dir.className = 'recent-dir';
    dir.textContent = entry.dir;
    button.appendChild(dir);
    const persona = document.createElement('span');
    persona.className = 'recent-persona';
    persona.textContent = entry.persona ? entry.persona : '';
    button.appendChild(persona);
    const time = document.createElement('span');
    time.className = 'recent-time';
    time.textContent = ago(entry.time);
    button.appendChild(time);
    button.addEventListener('click', () => useRecent(entry));
    recentList.appendChild(button);
  }
}

function recentTitle() {
  const title = document.createElement('div');
  title.className = 'recent-title';
  title.textContent = 'Recently used';
  return title;
}

newSessionBtn.addEventListener('click', () => {
  newSessionForm.hidden = !newSessionForm.hidden;
  if (!newSessionForm.hidden) {
    // The composer rebuilds its choices in the sheet each time the sheet
    // opens, so a node or persona list that changed while it was closed is
    // picked up; a dead node's recent is never offered.
    refreshNodeOptions();
    renderPersonaOptions();
    renderMcpOptions();
    renderRecents();
    if (isNarrow()) {
      newSessionForm.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }
  }
});

// A picked directory belongs to one node; switch nodes, drop the pick. Fired
// by renderNodeOptions and the chip clicks; the hidden select's own change
// event (many-nodes case) does the same work.
function onNodeChange() {
  setDir('');
  dirBrowser.hidden = true;
}
formNode.addEventListener('change', () => {
  // The visible select's choice becomes the composer's pick.
  if (formNode.value !== selectedNode) {
    onNodeChange();
    selectedNode = formNode.value;
    renderNodeChips();
  }
});

btnBrowse.addEventListener('click', () => {
  const node = selectedNode;
  if (!node) {
    showToast('choose a node first', 'error');
    return;
  }
  dirNode = node;
  dirPath = null;
  dirListing = null;
  dirBrowser.hidden = false;
  loadDirs();
});

btnDirUp.addEventListener('click', () => {
  if (dirPath === null) return;
  dirPath = dirListing && dirListing.parent ? dirListing.parent : null;
  loadDirs();
});

btnDirUse.addEventListener('click', () => {
  if (dirPath === null) return;
  setDir(dirPath);
  dirBrowser.hidden = true;
});

btnDirClose.addEventListener('click', () => {
  dirBrowser.hidden = true;
});

async function loadDirs() {
  let url = '/nodes/' + encodeURIComponent(dirNode) + '/dirs';
  if (dirPath) url += '?path=' + encodeURIComponent(dirPath);
  try {
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error('HTTP ' + response.status + ': ' + (await response.text()));
    }
    dirListing = await response.json();
    renderDirs();
  } catch (error) {
    showStatus('browse: ' + error.message);
  }
}

function renderDirs() {
  dirPathEl.textContent = dirPath === null ? 'browse roots' : dirPath;
  btnDirUp.disabled = dirPath === null;
  btnDirUse.disabled = dirPath === null;
  dirEntries.textContent = '';
  const entries = (dirListing && dirListing.entries) || [];
  if (entries.length === 0) {
    dirEntries.appendChild(document.createTextNode('(no directories)'));
    return;
  }
  for (const entry of entries) {
    const row = document.createElement('div');
    row.className = 'dir-entry';
    row.setAttribute('role', 'button');
    row.tabIndex = 0;
    const name = document.createElement('span');
    name.className = 'dir-name';
    name.textContent = entry.name;
    row.appendChild(name);
    if (entry.is_repo) {
      const repo = document.createElement('span');
      repo.className = 'dir-repo';
      repo.textContent = 'repo';
      row.appendChild(repo);
    }
    // The whole row descends into the entry in one tap; "Use this
    // directory" is the single pinned confirm, so a row never needs a
    // second button.
    const descend = () => {
      dirPath = entry.path;
      loadDirs();
    };
    row.addEventListener('click', descend);
    row.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        descend();
      }
    });
    dirEntries.appendChild(row);
  }
}

cancelNewSessionBtn.addEventListener('click', () => {
  // Cancel hides the sheet without resetting the composer: the picks and the
  // directory browsing state survive, ready for the next open.
  newSessionForm.hidden = true;
  renderRecents();
});

newSessionForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!selectedNode) {
    toastError('new session: choose a node first');
    return;
  }
  if (!formDir.value.trim()) {
    toastError('new session: choose a directory first');
    return;
  }
  const body = {
    node: selectedNode,
    dir: formDir.value.trim(),
  };
  const persona = formPersona.value.trim();
  if (persona) body.persona = persona;
  body.mcp_servers = [...mcpSelected];
  try {
    // A directory-based session on the node: the executor starts there and
    // the loop can run tools immediately.
    const response = await post('/dev', body);
    const session = await response.json();
    if (persona) {
      rememberLastPersona(persona);
    } else {
      clearLastPersona();
    }
    rememberCurrentSettings();
    newSessionForm.hidden = true;
    // The composer is cleared only by actually starting a session: cancel
    // and close keep the picks, a successful Start retires them.
    selectedNode = '';
    setDir('');
    personaSelected = '';
    formPersona.value = '';
    mcpSelected = new Set();
    renderNodeOptions();
    renderNodeChips();
    renderPersonaOptions();
    renderMcpOptions();
    renderRecents();
    await refreshSessions();
    openSession(session.id);
  } catch (error) {
    toastError('new session: ' + error.message);
  }
});
