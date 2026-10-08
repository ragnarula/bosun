// The pane as an app on the device: the service worker that keeps a copy of
// the pane for when the control plane cannot be reached, push notifications,
// and the badge on the app's icon. A browser allows them only in a secure
// context, which is HTTPS or the machine's own address. Anywhere else the
// pane is a page, and the notifications control says why it cannot turn on.

import { $ } from './dom.js';
import { post, showStatus, toastError, toastOk } from './common.js';

export { closeNotifications, setBadge };

const notifyBtn = $('btn-notify');
const hasWorker = window.isSecureContext && 'serviceWorker' in navigator;
const canPush = hasWorker && 'PushManager' in window && 'Notification' in window;

if (hasWorker) {
  // The worker's file changes with each build. `updateViaCache: 'none'` keeps
  // the browser's HTTP cache out of the check for a new one.
  navigator.serviceWorker.register('/sw.js', { updateViaCache: 'none' }).catch((error) => {
    showStatus('offline copy: ' + error.message);
  });
  // A tapped notification asks an open pane to show its tree.
  navigator.serviceWorker.addEventListener('message', (event) => {
    const open = event.data && event.data.open;
    if (typeof open === 'string' && open.startsWith('#s=')) location.hash = open;
  });
}

function bytesOf(base64url) {
  const base64 = base64url.replace(/-/g, '+').replace(/_/g, '/');
  const raw = atob(base64 + '='.repeat((4 - (base64.length % 4)) % 4));
  return Uint8Array.from(raw, (char) => char.charCodeAt(0));
}

async function controlPlaneKey() {
  const response = await fetch('/push/key');
  if (!response.ok) throw new Error('HTTP ' + response.status);
  return bytesOf((await response.json()).public_key);
}

// Whether a subscription was made with `key`. A browser that does not say
// which key it holds is taken to hold this one.
function madeWith(subscription, key) {
  const held = subscription.options && subscription.options.applicationServerKey;
  if (!held) return true;
  const bytes = new Uint8Array(held);
  return bytes.length === key.length && bytes.every((byte, index) => byte === key[index]);
}

function showNotifying(on) {
  notifyBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
  notifyBtn.title = on ? 'Notifications on' : 'Notifications off';
}

// A subscription this device holds is posted again on each load, so a
// control plane that lost it sends to it again. One made with another
// control plane's key cannot be read here and is dropped.
async function syncSubscription() {
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.getSubscription();
  if (!subscription) {
    showNotifying(false);
    return;
  }
  if (!madeWith(subscription, await controlPlaneKey())) {
    await subscription.unsubscribe();
    showNotifying(false);
    return;
  }
  await post('/push/subscriptions', subscription.toJSON());
  showNotifying(true);
}

async function turnOff() {
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.getSubscription();
  if (subscription) {
    const response = await fetch('/push/subscriptions', {
      method: 'DELETE',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ endpoint: subscription.endpoint }),
    });
    if (!response.ok) throw new Error('HTTP ' + response.status);
    await subscription.unsubscribe();
  }
  showNotifying(false);
  toastOk('Notifications off');
}

async function turnOn() {
  // Safari grants a permission request only while the tap that asked is
  // being handled, so the request comes before any other wait.
  if ((await Notification.requestPermission()) !== 'granted') {
    toastError('Notifications are blocked for this site in the browser settings');
    return;
  }
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: await controlPlaneKey(),
  });
  await post('/push/subscriptions', subscription.toJSON());
  showNotifying(true);
  toastOk('Notifications on');
}

notifyBtn.addEventListener('click', async () => {
  if (!window.isSecureContext) {
    toastError('Notifications need the pane on HTTPS');
    return;
  }
  if (!canPush) {
    toastError('This browser cannot notify here. On an iPhone, add Bosun to the Home Screen first.');
    return;
  }
  try {
    if (notifyBtn.getAttribute('aria-pressed') === 'true') await turnOff();
    else await turnOn();
  } catch (error) {
    toastError('notifications: ' + error.message);
  }
});

if (canPush) {
  syncSubscription().catch((error) => showStatus('notifications: ' + error.message));
}

// Closes the notifications of the tree whose root is `tag`, once the reader
// has the tree on screen.
function closeNotifications(tag) {
  if (!hasWorker) return;
  navigator.serviceWorker.ready
    .then((registration) => registration.getNotifications({ tag }))
    .then((shown) => {
      for (const notification of shown) notification.close();
    })
    .catch(() => {});
}

// The number of trees that need the reader, on the app's icon. Browsers
// without badges ignore it.
function setBadge(count) {
  if (!('setAppBadge' in navigator)) return;
  const done = count > 0 ? navigator.setAppBadge(count) : navigator.clearAppBadge();
  done.catch(() => {});
}
