// The MCP servers screen and its strip.

import {
  btnMcpAdd,
  btnMcpBack,
  btnMcpCancel,
  btnMcpSubmit,
  formMcpAuth,
  formMcpBearer,
  formMcpName,
  formMcpOAuthClient,
  formMcpOAuthSecret,
  formMcpUrl,
  mcpAdd,
  mcpBearerLabel,
  mcpList,
  mcpOAuthClientLabel,
  mcpOAuthSecretLabel,
  mcpStrip,
  mcpStripText,
  mcpTab,
} from './dom.js';
import { ago, post, showStatus, showToast, toastOk } from './common.js';
import { renderMcpOptions } from './new-session.js';
import { leaveScreen, showScreen } from './screens.js';

export { mcpServers, refreshMcpServers };

let mcpServers = [];
let mcpServersFetching = false;
let mcpEditingName = null; // set when the add form is editing an existing server

// The MCP servers sheet mirrors the skill-repos one. The strip on home opens
// it; the add form both creates and edits a server, and each row's buttons
// hold the repeated actions: Edit, Enable/Disable, Retry, Remove.
async function refreshMcpServers() {
  if (mcpServersFetching) return;
  mcpServersFetching = true;
  try {
    const response = await fetch('/mcp/servers');
    if (!response.ok) throw new Error('HTTP ' + response.status);
    mcpServers = await response.json();
    renderMcpStrip();
    renderMcpServers();
    renderMcpOptions();
  } catch (error) {
    showStatus('mcp: ' + error.message);
  } finally {
    mcpServersFetching = false;
  }
}

function renderMcpStrip() {
  if (mcpServers.length === 0) {
    mcpStripText.textContent = 'MCP servers · none yet';
    return;
  }
  const enabled = mcpServers.filter((server) => server.enabled).length;
  mcpStripText.textContent = mcpServers.length + ' server'
    + (mcpServers.length === 1 ? '' : 's')
    + ' · ' + enabled + ' enabled';
}

function openMcp() {
  showScreen(mcpTab);
  refreshMcpServers();
}

function closeMcp() {
  leaveScreen(mcpTab);
  mcpAdd.hidden = true;
  resetMcpForm();
}

mcpStrip.addEventListener('click', openMcp);
mcpStrip.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    openMcp();
  }
});

btnMcpBack.addEventListener('click', closeMcp);

btnMcpAdd.addEventListener('click', () => {
  if (!mcpAdd.hidden && mcpEditingName === null) {
    mcpAdd.hidden = true;
    return;
  }
  resetMcpForm();
  mcpAdd.hidden = false;
  formMcpName.focus();
});

function mcpAuthLabel(auth) {
  if (auth === 'oauth') return 'OAuth';
  if (auth === 'bearer') return 'Bearer';
  return 'none';
}

function mcpOAuthExpiry(expires_at_secs) {
  if (!expires_at_secs) return 'authorized';
  const now = Math.floor(Date.now() / 1000);
  const diff = expires_at_secs - now;
  if (diff >= 0) {
    if (diff < 60) return 'expires in ' + diff + 's';
    if (diff < 3600) return 'expires in ' + Math.floor(diff / 60) + 'm';
    if (diff < 86400) return 'expires in ' + Math.floor(diff / 3600) + 'h';
    return 'expires in ' + Math.floor(diff / 86400) + 'd';
  }
  return 'expired ' + ago(expires_at_secs);
}

function mcpOAuthStatus(server) {
  if (server.auth !== 'oauth') return 'not required';
  // The manager has a step-up URL ready, so the token is missing scopes the
  // server asked for.
  if (server.reauthorize_url) return 're-authorization required';
  if (server.oauth_authorized) {
    return mcpOAuthExpiry(server.oauth_expires_at_secs);
  }
  return 'not authorized';
}

function renderMcpServers() {
  mcpList.textContent = '';
  if (mcpServers.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = 'no MCP servers yet — add one to give sessions external tools';
    mcpList.appendChild(empty);
    return;
  }
  for (const server of mcpServers) {
    const row = document.createElement('div');
    row.className = 'mcp-row';

    const primary = document.createElement('button');
    primary.type = 'button';
    primary.className = 'mcp-primary';
    primary.title = 'edit ' + server.name;
    primary.appendChild(mcpDot(server));
    const name = document.createElement('span');
    name.className = 'mcp-name';
    name.textContent = server.name;
    primary.appendChild(name);
    primary.addEventListener('click', () => editMcpServer(server));
    row.appendChild(primary);

    const meta = document.createElement('div');
    meta.className = 'mcp-meta';
    meta.textContent = server.url + ' · ' + mcpAuthLabel(server.auth)
      + ' · oauth: ' + mcpOAuthStatus(server);
    row.appendChild(meta);
    if (server.last_error) {
      const error = document.createElement('div');
      error.className = 'mcp-error';
      error.textContent = server.last_error;
      row.appendChild(error);
    }

    const actions = document.createElement('div');
    actions.className = 'mcp-actions';
    const edit = document.createElement('button');
    edit.type = 'button';
    edit.textContent = 'Edit';
    edit.addEventListener('click', () => editMcpServer(server));
    actions.appendChild(edit);
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.textContent = server.enabled ? 'Disable' : 'Enable';
    toggle.addEventListener('click', () => setMcpServerEnabled(server));
    actions.appendChild(toggle);
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.textContent = 'Retry';
    retry.addEventListener('click', () => retryMcpServer(server));
    actions.appendChild(retry);
    if (server.auth === 'oauth') {
      const authorize = document.createElement('button');
      authorize.type = 'button';
      authorize.textContent = (server.oauth_authorized || server.reauthorize_url)
        ? 'Re-authorize' : 'Authorize';
      authorize.addEventListener('click', () => authorizeMcpServer(server));
      actions.appendChild(authorize);
    }
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'danger';
    remove.textContent = 'Remove';
    remove.addEventListener('click', () => removeMcpServer(server));
    actions.appendChild(remove);
    row.appendChild(actions);

    mcpList.appendChild(row);
  }
}

// One status dot: up (enabled, no error), warn (disabled), down (error).
function mcpDot(server) {
  const dot = document.createElement('span');
  dot.className = 'dot ' + (server.last_error ? 'down' : server.enabled ? 'up' : 'warn');
  return dot;
}

function mcpAuthValue() {
  return formMcpAuth.value || 'none';
}

function resetMcpForm() {
  mcpEditingName = null;
  formMcpName.value = '';
  formMcpName.disabled = false;
  formMcpUrl.value = '';
  formMcpAuth.value = 'none';
  formMcpBearer.value = '';
  mcpBearerLabel.hidden = true;
  formMcpOAuthClient.value = '';
  formMcpOAuthSecret.value = '';
  mcpOAuthClientLabel.hidden = true;
  mcpOAuthSecretLabel.hidden = true;
  btnMcpSubmit.textContent = 'Add';
}

function editMcpServer(server) {
  mcpEditingName = server.name;
  formMcpName.value = server.name;
  formMcpName.disabled = true;
  formMcpUrl.value = server.url;
  formMcpAuth.value = server.auth;
  formMcpBearer.value = '';
  formMcpOAuthClient.value = '';
  formMcpOAuthSecret.value = '';
  mcpBearerLabel.hidden = server.auth !== 'bearer';
  mcpOAuthClientLabel.hidden = server.auth !== 'oauth';
  mcpOAuthSecretLabel.hidden = server.auth !== 'oauth';
  btnMcpSubmit.textContent = 'Update';
  mcpAdd.hidden = false;
  formMcpUrl.focus();
}

formMcpAuth.addEventListener('change', () => {
  mcpBearerLabel.hidden = mcpAuthValue() !== 'bearer';
  mcpOAuthClientLabel.hidden = mcpAuthValue() !== 'oauth';
  mcpOAuthSecretLabel.hidden = mcpAuthValue() !== 'oauth';
});

btnMcpCancel.addEventListener('click', () => {
  mcpAdd.hidden = true;
  resetMcpForm();
});

formMcpName.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    btnMcpSubmit.click();
  }
});

btnMcpSubmit.addEventListener('click', async () => {
  const name = formMcpName.value.trim();
  const url = formMcpUrl.value.trim();
  const auth = mcpAuthValue();
  if (!name) {
    showToast('enter a server name', 'error');
    return;
  }
  if (!url) {
    showToast('enter a server URL', 'error');
    return;
  }
  const editing = mcpEditingName !== null;
  const payload = { url, auth };
  if (auth === 'bearer' && formMcpBearer.value) {
    payload.bearer_token = formMcpBearer.value;
  }
  if (auth === 'oauth') {
    if (formMcpOAuthClient.value) payload.oauth_client_id = formMcpOAuthClient.value.trim();
    if (formMcpOAuthSecret.value) payload.oauth_client_secret = formMcpOAuthSecret.value;
  }
  btnMcpSubmit.disabled = true;
  try {
    const target = editing ? '/mcp/servers/' + encodeURIComponent(mcpEditingName)
      : '/mcp/servers';
    if (editing) {
      await post(target, payload);
      toastOk('updated ' + mcpEditingName);
    } else {
      await post(target, { name, ...payload });
      toastOk('added ' + name);
    }
    mcpAdd.hidden = true;
    resetMcpForm();
    await refreshMcpServers();
  } catch (error) {
    showToast((editing ? 'update ' : 'add ') + (editing ? mcpEditingName : name) + ': ' + error.message, 'error');
    await refreshMcpServers();
  } finally {
    btnMcpSubmit.disabled = false;
  }
});

async function setMcpServerEnabled(server) {
  try {
    await post('/mcp/servers/' + encodeURIComponent(server.name) + '/enabled', {
      enabled: !server.enabled,
    });
    await refreshMcpServers();
  } catch (error) {
    showToast('mcp server: ' + error.message, 'error');
  }
}

async function retryMcpServer(server) {
  try {
    await post('/mcp/servers/' + encodeURIComponent(server.name) + '/retry', {});
  } catch (error) {
    showToast('retry ' + server.name + ': ' + error.message, 'error');
  }
}

// A server that answered with a challenge for more scopes already has a flow
// started by the manager: visiting its URL completes that flow, and starting
// another would drop the one the manager remembers.
async function authorizeMcpServer(server) {
  if (server.reauthorize_url) {
    window.location = server.reauthorize_url;
    return;
  }
  try {
    const response = await post('/mcp/servers/' + encodeURIComponent(server.name) + '/authorize', {});
    const body = await response.json();
    window.location = body.authorize_url;
  } catch (error) {
    showToast('authorize ' + server.name + ': ' + error.message, 'error');
  }
}

async function removeMcpServer(server) {
  if (!window.confirm('Remove MCP server ' + server.name + '?')) return;
  try {
    const response = await fetch('/mcp/servers/' + encodeURIComponent(server.name), {
      method: 'DELETE',
    });
    if (!response.ok) throw new Error('HTTP ' + response.status);
    toastOk('removed ' + server.name);
    await refreshMcpServers();
  } catch (error) {
    showToast('remove MCP server: ' + error.message, 'error');
  }
}
