# Sprint 014 — Installable Pane

The web pane runs in a browser tab. On a phone, the browser's address bar and toolbar take space from the chat, the pane has no icon on the home screen, and the reader learns that a session needs them only by opening the tab. When the control plane is down or the phone is offline, the browser shows its own error page. This sprint makes the pane a Progressive Web App (PWA): a phone or desktop can install it with its own icon and window, and a service worker keeps a copy of the pane on the device and shows a notification when a session asks a question or finishes its tasks.

Status: **planned**. The decisions are confirmed. No story has started.

## Confirmed decisions

- **Each operator chooses the address.** One control plane is served over plain HTTP on a LAN, another over HTTPS through Tailscale Serve or another proxy. The pane works the same over both. What needs a secure context, which is the service worker, notifications and the install prompt, turns on only where the browser allows it, and the pane says so where a control depends on it.
- **The network first.** Every file of the pane comes from the control plane when it answers. The service worker's cache answers only when the control plane does not. The pane is as fresh as it is today.
- **Two notifications: a question, and all tasks complete.** A tree notifies when one of its sessions asks the reader a question, and when every task on its task list is complete.
- **The icon is the Blue Peter.** See [The icon](#the-icon).

## What installing needs

A browser offers to install a page when all of these hold:

- **A secure context.** The page is served over HTTPS, or from `localhost` or `127.0.0.1`. Service workers and notifications have the same rule.
  - Over plain HTTP on any other address, a browser can still add a shortcut to the home screen. It opens in a browser tab, with no service worker and no notifications. The pane checks `window.isSecureContext` and registers no worker there. The notifications control says that it needs HTTPS.
  - Over HTTPS, the pane is installed with its own window, and every story below applies.
- **A web app manifest**, linked from the page with `<link rel="manifest">`. It needs `name`, `short_name`, `start_url`, `display: "standalone"`, and icons of 192 and 512 pixels. Chrome no longer requires a service worker for installing, but the other gains below need one.
- **Icons as PNG files.** Android needs 192 and 512 pixel icons, and one 512 pixel `maskable` icon that keeps its content inside the centre safe zone. iOS reads `<link rel="apple-touch-icon">` at 180 pixels.

On iOS, installing is always manual: Share, then Add to Home Screen. Safari shows no prompt. Android Chrome shows its own install prompt.

### Manifest fields

| Field | Value | Why |
|---|---|---|
| `id` | `/` | Keeps the installed app the same app if `start_url` changes later. |
| `name`, `short_name` | `Bosun` | |
| `start_url` | `/` | Home. A session opens from its `#s=` link, which a notification names. |
| `scope` | `/` | The pane is served at `/` and at `/ui`. |
| `display` | `standalone` | No browser bar. |
| `background_color`, `theme_color` | `--sea-900`, `#0a1220` | The splash screen and the title bar match Night. A `<meta name="theme-color">` per `prefers-color-scheme` gives Day its own `#eef2f7`. |
| `icons` | 192, 512, 512 maskable | |
| `shortcuts` | New session, Projects | A long press on the icon goes straight there. |
| `launch_handler` | `{"client_mode": "focus-existing"}` | Chrome only: opening the app or a notification reuses the open window instead of starting a second pane. |

### The icon

The icon is the Blue Peter, the International Code of Signals flag P, which the pane already draws in `signal.js` for the lead persona. A ship flies the Blue Peter when all of its crew must come aboard because it is about to sail, which is the bosun's call. It is also the clearest flag at a small size: one blue field and one white square.

- The cloth is `--flag-blue` (`#1f5fd1`) with a `--flag-white` (`#f4f6fa`) square in its centre, in the 24 × 18 proportions that `signal.js` draws.
- The background is `--sea-900` (`#0a1220`), filling the whole square, so Android's mask and iOS's rounded corners cut only the background.
- The flag spans the middle 60% of the width, inside the 80% safe zone of a maskable icon, so one drawing serves every size.
- The source is one SVG, `crates/bosun-control/src/ui/icons/icon.svg`. The PNG files are drawn from it once and committed. They are embedded with `include_bytes!` like the fonts, so the build stays `cargo` only.

## What a service worker does for the pane

With the network first, the worker does not make a normal load faster. It does four things:

1. **Notifies when a session needs the reader.** This is the largest gain. See [Notifications](#notifications).
2. **Opens when the control plane cannot be reached.** The pane opens from its cache and says that the control plane is not reachable, instead of the browser's error page. It shows the session list from the last answer it had, marked as not current.
3. **Never mixes two builds.** The pane-as-modules ADR accepts that a control plane restarting between the page's fetch and its module fetches serves an old page with new modules. The worker reads the page's `X-Bosun-Version` header, and when a module from the network names another build, it answers with that module from the page's own build in its cache.
4. **Puts a badge on the icon.** `navigator.setAppBadge(n)` shows the number of trees that need the reader. This works on desktop Chrome and Edge and on iOS 16.4 and later. Android Chrome does not support it, but Android shows a dot for an unread notification.

Not in this sprint:

- **Sending a message while offline, through Background Sync.** It is Chromium only. A message sent late can also answer a question that has changed, so the pane says it is offline and keeps the draft, as it does today.
- **A share target.** Android could send a link from another app into the new-session sheet. It is Chrome on Android only, and it can follow later.

## How the worker loads the pane

- **Network first, with a time limit.** The worker sends each request for the page, a module, the stylesheet or a font to the control plane. A response is stored in the cache of the build it names and returned. When the request fails, or the control plane has not answered in 5 seconds, the worker returns the copy from the cache. Without the time limit, a phone on a link that connects but carries nothing would wait for the browser's own timeout, which is minutes.
- **One cache per build.** The cache is named by the version. When a load's page names a new build, the worker deletes the caches of older builds once that load's files are all stored.
- **The mermaid bundle** is 5.5 MB and is cached for a day over HTTP. The worker stores it when the pane first loads it, not at install, so an update does not fetch 5.5 MB on a mobile link.
- **The worker does not touch the API or the streams.** An `EventSource` request is a fetch, and a worker that answered it would break the resume by `Last-Event-ID`. The fetch handler answers only the page, the files in `ASSETS` and `FONTS`, the mermaid bundle and the icons. Every other request goes to the network as if no worker existed. The one exception is `GET /sessions`: the worker keeps its last answer, for the list shown when the control plane cannot be reached.
- **The worker updates itself at once.** `sw.js` is served `no-cache` with the version in its body, so a new build changes its bytes and the browser installs it on the next load. The new worker calls `skipWaiting()` and `clients.claim()`. With the network first, the pane needs no "new version" message. A later build can serve a `sw.js` that unregisters itself and clears its caches, if a worker ever needs removing.

## Constraints from the code and the ADRs

- **No build step.** The service worker is a hand-written file in `crates/bosun-control/src/ui/`, embedded like the modules. The list of files it may answer is built from `ASSETS` and `FONTS` by the control plane and written into it, the same way `{{BOSUN_VERSION}}` is written into the page. There is no second list to keep in step.
- **The worker is served at `/sw.js`.** A worker controls only the paths below its own URL. One at `/ui/sw.js` would control `/ui/` but not `/` or `/ui`, which serve the pane. `/sw.js` and `/manifest.webmanifest` join `RESERVED_PATHS`, and the icons go under a new `RESERVED_SEGMENT_PARENTS` entry, `/ui/icons/`.
- **The HTTP responses stay `no-store`.** The worker's cache is separate from the browser's HTTP cache, and only the worker reads it.
- **The browser tests.** Playwright drives service workers in Chromium. The browser tests serve the pane from `127.0.0.1`, which is a secure context, so the worker registers there. Each test gets a new browser context, so no cache is carried between tests. New checks cover the manifest, a load with the control plane stopped, the build check, and a pane served from a non-secure address, which must register no worker and still work.
- **A new ADR.** The worker's cache changes the pane-as-modules ADR's decision that nothing of the pane is kept on the device. The new ADR records the network-first cache, the build check, and why the HTTP responses stay `no-store`.

### If the shared password lands

The `control-plane-password` branch puts HTTP Basic on every route. Two things then change:

- A browser fetches the manifest without credentials unless the link says otherwise. The link must be `<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials">`, or the manifest is answered `401` and the pane cannot be installed.
- A push message comes from the push service, not from the page, so it carries no credential. That is fine: the worker only shows it. Opening the pane after a tap goes through the browser's Basic prompt as usual.

## Notifications

Web Push needs:

- **A VAPID key pair** on the control plane, made once and kept in the store. The public key goes to the pane so it can subscribe.
- **A subscription per device.** The pane asks for permission when the reader taps the notifications control. A browser blocks a request that does not follow a tap, and iOS allows one only in an installed pane. The pane posts the `PushSubscription` (an endpoint URL and two keys) to a new route, `POST /push/subscriptions`, and the store keeps it in a new table. A `404` or `410` from the push service deletes the subscription.
- **Sending.** The control plane encrypts the message to the subscription's keys (RFC 8291) and posts it to the endpoint with a VAPID signature (RFC 8292). The endpoint belongs to the browser's push service: Google for Chrome, Mozilla for Firefox, Apple for Safari. The control plane already reaches the internet for the model APIs. A crate such as `web-push-native` builds the request, and the existing `reqwest` client sends it. The push service sees when a message is sent and its size, but not its text.
- **The triggers.** The session overview already carries `asking` and `tasks` (`total`, `done`):
  - **A question.** A session in the tree changes `asking` from false to true. The notification names the persona and shows the question.
  - **All tasks complete.** The tree's tasks change from some open to `total > 0` and `done == total`. The notification shows the tree's summary.
  - Each tree has one notification at a time. A new one replaces the last through the same `tag`, so a busy tree does not fill the phone. There is no notification while a pane on that device is visible.
- **The worker.** On `push`, it shows the notification. On `notificationclick`, it focuses an open pane and moves it to `#s=<id>`, or opens a new one there.

Support: Chrome, Edge and Firefox on Android and desktop. Safari on macOS. Safari on iOS 16.4 and later, only for a pane added to the home screen.

## Standalone mode

- **Back.** An installed pane has no browser back button. Every screen already has its own back control (`.bs-back`), and Android's back gesture follows the history entries that `history.js` keeps.
- **The viewport.** A standalone window has no browser bar, so its height and its safe-area insets differ from a tab. The keyboard handling in `viewport.js` needs checking on the phone with `?viewport-report` in both modes.
- **Background polls.** `main.js` polls `/sessions`, `/nodes` and `/projects` every 3 to 5 seconds, with no check for visibility. An installed app spends more time in the background than a tab does. The pane stops its polls while hidden and refreshes everything at once when it comes back, so the reader sees the current state at once instead of after the next poll.

## User stories in implementation order

- [ ] **S1 — The manifest and the icon.** `/manifest.webmanifest`, the Blue Peter as SVG and PNG, the `apple-touch-icon` and `theme-color` tags. The pane can be installed from an HTTPS address and added to the home screen from an HTTP one.
- [ ] **S2 — Polls follow visibility.** The pane stops polling while hidden and refreshes on return.
- [ ] **S3 — The service worker.** `/sw.js`, registered only in a secure context. Network first with the 5-second limit, one cache per build, the build check, the mermaid bundle stored on first use. API and stream requests are not touched. The ADR.
- [ ] **S4 — Opening without the control plane.** The pane opens from the cache, says it cannot reach the control plane, and shows the last session list it had.
- [ ] **S5 — Notifications.** VAPID keys, the subscriptions table and route, the push sender, the two triggers, and the worker's `push` and `notificationclick` handlers. A control in the pane turns them on for the device, or says that they need HTTPS.
- [ ] **S6 — The badge and the shortcuts.** `setAppBadge` with the number of trees that need the reader, and the manifest's `shortcuts`.
- [ ] **S7 — The docs that own the current state say so.** CLAUDE.md, the README, and in the operator docs, that installing and notifications need HTTPS.

## Out of scope

- An app store package, such as a Trusted Web Activity for the Play Store.
- Sending messages or answers while offline.
- A share target.
- Notifications by email or chat.
