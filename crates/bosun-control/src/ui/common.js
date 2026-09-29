// Helpers every screen uses: the POST call, the status line, toasts, and
// time and id formatting.

import { statusEl, toastEl } from './dom.js';

export { ago, post, shortId, shortSha, showStatus, showToast, toastError, toastOk };

function showStatus(text) {
  statusEl.textContent = text;
}

let toastTimer = null;

// A transient bottom-line notice for user actions: errors and confirmations.
// Background fetch failures keep showing in #status, which does not compete
// with a thumb's work; the toast is for the messages the user caused.
function showToast(text, kind) {
  toastEl.textContent = text;
  toastEl.className = kind || '';
  toastEl.hidden = false;
  if (toastTimer) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toastEl.hidden = true;
    toastEl.className = '';
  }, 3000);
}

function toastError(text) {
  showToast(text, 'error');
}

function toastOk(text) {
  showToast(text, 'ok');
}

function ago(secs) {
  const diff = Math.max(0, Math.floor(Date.now() / 1000) - (secs || 0));
  if (diff < 60) return diff + 's ago';
  if (diff < 3600) return Math.floor(diff / 60) + 'm ago';
  if (diff < 86400) return Math.floor(diff / 3600) + 'h ago';
  return Math.floor(diff / 86400) + 'd ago';
}

function shortId(id) {
  return id.length > 8 ? id.slice(0, 8) : id;
}

function shortSha(sha) {
  return sha.length > 8 ? sha.slice(0, 8) : sha;
}

async function post(url, body) {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error('HTTP ' + response.status + ': ' + (await response.text()));
  }
  return response;
}
