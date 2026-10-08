# ADR: A service worker keeps the pane, and the network answers first

**Date:** 2026-10-08
**Author:** Pete

## Context

The web pane is the control plane's browser client. `crates/bosun-control/src/ui.rs` embeds the page, its stylesheet, its modules, its fonts and the mermaid bundle, and serves the page and the modules `no-store` (see `2026-09-29-pane-as-embedded-modules.md`). Nothing of the pane is kept on the device between loads, so the browser shows its own error page when the control plane cannot be reached. Every response carries `X-Bosun-Version`.

The pane is installable as a web app: `/manifest.webmanifest` names it, its icons and a standalone window. An installed pane on a phone is often opened while the phone is offline, or while the machine that runs the control plane is asleep.

Each operator chooses how the pane is reached: plain HTTP on a LAN, or HTTPS through Tailscale Serve or another proxy. A browser runs a service worker only in a secure context: HTTPS, or the machine's own address.

## Decision Drivers

- A fix reaches the phone on its next load. The page is `no-store` because a phone that kept an old page kept the bug the new one fixed.
- The pane opens when the control plane cannot be reached, and says so.
- One load never runs files from two builds.
- The API and the event streams behave as if no worker existed. An `EventSource` request is a fetch, and its resume depends on `Last-Event-ID`.
- No build step, and no second list of the pane's files to keep in step with `ASSETS`.
- The pane works the same over plain HTTP, where no worker can run.

## Options Considered

**1. Network first, with a time limit, and one kept build per worker. (chosen)**

The worker answers the page, its stylesheet, modules, fonts, icons, manifest and the mermaid bundle. Each goes to the control plane first, and the kept copy answers only when the request fails or takes longer than 5 seconds. The pane is as fresh as it is without a worker.

**2. Cache first, with the update installed in the background. (rejected)**

The pane would draw before any request: the fastest start on a slow link. But every load after a new build runs the old build once, and needs a "new version" message and a reload. That brings back the stale-pane problem `no-store` exists to prevent, for the one load that most needs the fix.

**3. Network first with no time limit. (rejected)**

A phone on a link that connects but carries nothing waits for the browser's own timeout, which is minutes, before the kept copy answers. The limit costs one more branch in the worker.

**4. The worker also answers `GET /sessions` from a kept copy. (rejected)**

The pane would show the last list without knowing where it came from, and the browser checks hold and rewrite `/sessions` through Playwright routes, which do not see a request a worker answers. The pane keeps the last list itself, in `localStorage`, and knows when it shows it.

**5. Versioned URLs for the modules. (rejected)**

`/ui/<version>/main.js` would stop two builds mixing in one load without a worker, but the pane-as-modules ADR rejected it for the import graph's fixed names, and it does not open the pane offline.

## Decision

`/sw.js` is the worker, served from `crates/bosun-control/src/ui/sw.js` with the build's version and the list of paths written in by `ui::service_worker_js`. The list is built from `ASSETS`, `FONTS` and `ICONS`, plus `/`, `/ui` and the manifest. It is served at the root because a worker controls only the paths below its own URL. It is served `no-store`, and the pane registers it with `updateViaCache: 'none'`, so the browser compares it on each load and installs a new build's worker at once.

On install, the worker fetches every path in its list into the cache `bosun-pane-<version>`. A response that names another build in `X-Bosun-Version` fails the install: the control plane changed builds since the worker was fetched, and the next load fetches the newer worker. The worker then calls `skipWaiting()`, and on activate deletes every other `bosun-pane-` cache and calls `clients.claim()`. The mermaid bundle is kept on its first load, not at install, so a new build does not fetch 5.5 MB at once.

On fetch, the worker answers only a `GET` for a path in its list or the mermaid bundle. Each goes to the network first and waits at most `NETWORK_LIMIT_MS`, 5 seconds. A navigation records the build its page names, by the load's client id. A later file of that load that names another build is answered from the kept copy when the page is the worker's own build. Every other request, including all of the API and every stream, is not answered, so it goes to the network unchanged.

`device.js` registers the worker only when `window.isSecureContext` is true. Over plain HTTP the pane is a page, as before.

The session list is kept by the pane: `session-list.js` writes each list it reads to `localStorage`, and when `fetch('/sessions')` rejects, it draws the kept list if it has drawn none, and shows `#offline` with the time of the kept list.

The HTTP responses stay `no-store`. The worker's cache is separate from the browser's HTTP cache, and only the worker reads it.

## Consequences

- A pane loads as fresh as it did without a worker. The worker saves no time on a good network.
- An installed pane opens offline, or with the control plane down, and shows the last session list with a note. Sessions cannot be opened or driven until the control plane answers.
- A page and its modules from two builds cannot mix in one load, as long as the worker is still running when the modules are fetched. A browser can stop an idle worker, and one stopped between the page's fetch and its modules' fetches forgets the page's build.
- A load where the control plane answers slower than 5 seconds gets the kept copy, which can be one build old. The next load fetches the new build.
- Over plain HTTP to another machine there is no worker, so no offline copy and no notifications.
- The browser checks block the worker unless a check asks for it, because Playwright's routes do not see a request the worker answers.
- A worker that must be removed needs a build whose `sw.js` unregisters itself and deletes its caches. No such build exists yet.

## Revisit When

- The pane gains a build step, so its files can carry their build in their names.
- Reading or driving a session offline is wanted, such as queueing a message to send when the control plane answers.
- A browser the pane must support stops idle workers often enough that two builds mix in one load.
