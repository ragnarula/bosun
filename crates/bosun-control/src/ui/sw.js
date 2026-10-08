// The pane's service worker. It keeps one build of the pane on the device and
// answers with it only when the control plane does not: every file comes from
// the network first. It also shows the control plane's push notifications.
// It never answers for the API or the event streams.
//
// The control plane writes the build and the file list in when it serves this
// file, so each build serves a different file and the browser installs the
// new worker on the next load.

const BUILD = '{{BOSUN_VERSION}}';
const PANE_FILES = {{PANE_FILES}};
const MERMAID = '/ui/mermaid.min.js';
const CACHE_PREFIX = 'bosun-pane-';
const CACHE = CACHE_PREFIX + BUILD;
const VERSION_HEADER = 'x-bosun-version';
// How long a request waits for the control plane before the kept copy
// answers. Without a limit, a phone on a link that connects but carries
// nothing waits for the browser's own timeout, which is minutes.
const NETWORK_LIMIT_MS = 5000;
// The page loads whose build the worker remembers, newest last.
const REMEMBERED_LOADS = 20;

// The build each page load came from, by the load's client id. A worker that
// stops between a page's fetch and its modules' fetches forgets the load, and
// the modules are then taken as the network gives them.
const builds = new Map();

self.addEventListener('install', (event) => {
  event.waitUntil(keepBuild().then(() => self.skipWaiting()));
});

// Fetches every file of this worker's build. A file that names another build
// means the control plane changed builds since this worker was fetched: the
// install fails, and the next load fetches the newer worker.
async function keepBuild() {
  const cache = await caches.open(CACHE);
  await Promise.all(PANE_FILES.map(async (path) => {
    const response = await fetch(path, { cache: 'no-store' });
    const build = response.headers.get(VERSION_HEADER);
    if (!response.ok || (build && build !== BUILD)) {
      throw new Error(path + ' is not from build ' + BUILD);
    }
    await cache.put(path, response);
  }));
}

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    for (const name of await caches.keys()) {
      if (name.startsWith(CACHE_PREFIX) && name !== CACHE) await caches.delete(name);
    }
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname === MERMAID) {
    event.respondWith(mermaid(event));
  } else if (PANE_FILES.includes(url.pathname)) {
    event.respondWith(paneFile(event, url.pathname));
  }
});

// The network's answer, or null when it fails or takes longer than the limit.
function withinLimit(answer) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), NETWORK_LIMIT_MS);
    answer.then(
      (response) => {
        clearTimeout(timer);
        resolve(response);
      },
      () => {
        clearTimeout(timer);
        resolve(null);
      },
    );
  });
}

function remember(client, build) {
  if (!client || !build) return;
  builds.delete(client);
  builds.set(client, build);
  if (builds.size > REMEMBERED_LOADS) builds.delete(builds.keys().next().value);
}

function kept(path) {
  return caches.match(path, { cacheName: CACHE });
}

// The page, a stylesheet, a module, a font or an icon: the network's answer
// when it comes in time, else this build's kept copy. A file that names
// another build than its page did is answered from the kept build instead,
// when the page is this worker's build, so one load never runs two builds.
async function paneFile(event, path) {
  const navigation = event.request.mode === 'navigate';
  const client = navigation ? event.resultingClientId : event.clientId;
  const network = fetch(event.request);
  const response = await withinLimit(network);
  if (response && response.ok) {
    const build = response.headers.get(VERSION_HEADER);
    if (navigation) {
      remember(client, build);
      return response;
    }
    const pageBuild = builds.get(client);
    if (build && pageBuild && build !== pageBuild && pageBuild === BUILD) {
      const copy = await kept(path);
      if (copy) return copy;
    }
    return response;
  }
  const copy = await kept(path);
  if (copy) {
    if (navigation) remember(client, BUILD);
    return copy;
  }
  // Nothing kept: the network's answer, even an error or a late one.
  return response || network;
}

// The diagram bundle: the network first, and a copy kept from its first load.
async function mermaid(event) {
  const network = fetch(event.request);
  const response = await withinLimit(network);
  if (response && response.ok) {
    const copy = response.clone();
    event.waitUntil(caches.open(CACHE).then((cache) => cache.put(MERMAID, copy)));
    return response;
  }
  return (await kept(MERMAID)) || response || network;
}

// A notification from the control plane: a tree asked the reader a question
// or completed its tasks. The tag is the tree, so a newer notification
// replaces the tree's last one.
self.addEventListener('push', (event) => {
  let notice = { title: 'Bosun', body: '', tag: 'bosun', url: '/' };
  try {
    notice = event.data.json();
  } catch (error) {
    // A message that is not the control plane's JSON still shows, as Bosun.
  }
  const shown = self.registration.showNotification(notice.title, {
    body: notice.body,
    tag: notice.tag,
    renotify: true,
    icon: '/ui/icons/icon-192.png',
    data: { url: notice.url },
  });
  const badged = self.navigator.setAppBadge
    ? self.navigator.setAppBadge().catch(() => {})
    : Promise.resolve();
  event.waitUntil(Promise.all([shown, badged]));
});

// Opens the notification's tree: an open pane is focused and told to open
// it, and with no pane open, a new one starts there.
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = new URL((event.notification.data && event.notification.data.url) || '/', self.location.origin);
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const pane = windows.find((client) => new URL(client.url).origin === url.origin);
    if (pane) {
      await pane.focus();
      pane.postMessage({ open: url.hash });
      return;
    }
    await self.clients.openWindow(url.href);
  })());
});
