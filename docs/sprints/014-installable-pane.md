# Sprint 014 — Installable Pane

The web pane runs in a browser tab. On a phone, the browser's address bar and toolbar take space from the chat, the pane has no icon on the home screen, and the reader learns that a session needs them only by opening the tab. Each load fetches the page and about twenty modules, none of them cached. When the control plane is down or the phone is offline, the browser shows its own error page. This sprint makes the pane a Progressive Web App (PWA): a phone or desktop can install it with its own icon and window, and a service worker keeps a copy of the pane on the device and shows notifications when a session needs the reader.

Status: **proposed**. This is the research. The decisions below need confirming before work starts.

## What installing needs

A browser offers to install a page when all of these hold:

- **A secure context.** The page is served over HTTPS, or from `localhost` or `127.0.0.1`. Service workers have the same rule.
  - The Tailscale Serve address (`https://<machine>.<tailnet>.ts.net`) qualifies. Tailscale issues it a real certificate.
  - `http://127.0.0.1:8090` qualifies on the machine that runs the control plane.
  - `http://192.168.1.18:8090` on the homelab does not. A phone on the LAN can still add a shortcut to it, but it gets no service worker, no notifications and no install prompt. The homelab needs HTTPS, for example Tailscale Serve there too, or `tls_cert` and `tls_key` with a certificate the phone trusts.
- **A web app manifest**, linked from the page with `<link rel="manifest">`. It needs `name`, `short_name`, `start_url`, `display: "standalone"`, and icons of 192 and 512 pixels. Chrome no longer requires a service worker for installing, but every other gain below needs one.
- **Icons as PNG files.** Android needs 192 and 512 pixel icons, and one 512 pixel `maskable` icon that keeps its content inside the centre safe zone. iOS reads `<link rel="apple-touch-icon">` at 180 pixels. The pane has no image files today. The icon can be the Bosun Signal flag, drawn once and committed as PNG files. They are embedded like the fonts, with `include_bytes!`, so the build stays `cargo` only.

On iOS, installing is always manual: Share, then Add to Home Screen. Safari shows no prompt. Android Chrome shows its own install prompt, and the pane can also offer an Install button through the `beforeinstallprompt` event.

### Manifest fields

| Field | Value | Why |
|---|---|---|
| `id` | `/` | Keeps the installed app the same app if `start_url` changes later. |
| `name`, `short_name` | `Bosun` | |
| `start_url` | `/` | Home. A session opens from its `#s=` link, which a notification can name. |
| `scope` | `/` | The pane is served at `/` and at `/ui`. |
| `display` | `standalone` | No browser bar. |
| `background_color`, `theme_color` | `--sea-900`, `#0a1220` | The splash screen and the title bar match Night. A `<meta name="theme-color">` per `prefers-color-scheme` gives Day its own `#eef2f7`. |
| `icons` | 192, 512, 512 maskable | |
| `shortcuts` | New session, Projects | A long press on the icon goes straight there. |
| `launch_handler` | `{"client_mode": "focus-existing"}` | Chrome only: opening the app or a notification reuses the open window instead of starting a second pane. |

## What a service worker can do for the pane

In order of value to a reader on a phone:

1. **Notify when a session needs the reader.** This is the largest gain. Today a session that asks a question, waits for approval or finishes is seen only when the reader opens the pane. With Web Push, the control plane sends a notification. Tapping it opens the pane on that session's `#s=` link. Home already groups trees by what needs the reader, and the same state is the trigger. See [Notifications](#notifications).
2. **Open at once, from the device.** The service worker keeps every file of one build: the page, `pane.css`, the modules and the fonts. A load then makes no network request before the pane draws, and only the data requests (`/sessions`, `/nodes`, the streams) go to the control plane. On a slow mobile link this removes about twenty round trips before the first draw.
3. **Never mix two builds.** The pane-as-modules ADR accepts that a control plane restarting between the page's fetch and its module fetches serves an old page with new modules. A service worker that fills its cache from one build, checks each file's `X-Bosun-Version` header, and swaps the whole cache at once removes that case.
4. **Open when the control plane cannot be reached.** The pane opens from its cache and says that the control plane is not reachable, instead of the browser's error page. It can also show the session list from the last answer it had, marked as not current.
5. **A badge on the icon.** `navigator.setAppBadge(n)` puts the number of trees that need the reader on the app icon. This works on desktop Chrome and Edge and on iOS 16.4 and later. Android Chrome does not support it, but Android shows a dot for an unread notification.
6. **Receive shares.** A manifest `share_target` lets Android send a link or text from another app, such as an issue or a pull request, to Bosun, which opens the new-session sheet with it filled in. Chrome on Android only.

Not proposed:

- **Sending a message while offline, through Background Sync.** It is Chromium only. A message sent late can also answer a question that has changed, so the pane should say it is offline and keep the draft, as it does today.
- **Precaching the mermaid bundle.** It is 5.5 MB and already cached for a day over HTTP. The service worker caches it on first use instead, so an update does not fetch 5.5 MB on a mobile link.

## Constraints from the code and the ADRs

- **No build step.** The service worker is a hand-written file in `crates/bosun-control/src/ui/`, embedded like the modules. Its list of files to cache is built from `ASSETS` and `FONTS` by the control plane and written into it, the same way `{{BOSUN_VERSION}}` is written into the page. There is no second list to keep in step.
- **The service worker is served at `/sw.js`.** A worker controls only the paths below its own URL. One at `/ui/sw.js` would control `/ui/` but not `/` or `/ui`, which serve the pane. `/sw.js` and `/manifest.webmanifest` join `RESERVED_PATHS`, and the icons go under a new `RESERVED_SEGMENT_PARENTS` entry, `/ui/icons/`.
- **`sw.js` is served `no-cache`**, with the version in its body. The browser compares the file byte by byte on each load and at least once a day. A new build changes the bytes, and that starts the update. Registering with `updateViaCache: 'none'` keeps the HTTP cache out of the check.
- **The worker must not touch the API or the streams.** An `EventSource` request is a fetch, and a worker that answered it would break the resume by `Last-Event-ID`. The fetch handler answers only the files in its list and the page. Every other request goes to the network as if no worker existed.
- **A stale pane must not outlive a fix.** The page is served `no-store` because "a phone that kept the old page keeps the bug it fixed". A cached pane brings that risk back, so the update path must be certain:
  - The new build installs in the background and is used from the next load.
  - The pane shows a quiet line, "Bosun updated", with a Reload button. It reloads by itself when it comes back from the background and the composer is empty. Drafts are already saved.
  - A broken worker can be removed from the server side. A later build serves a `sw.js` that unregisters itself and clears its caches.
- **The browser tests.** Playwright drives service workers in Chromium, and each test gets a new browser context, so no cache is carried between tests. The checks that exist need no change. New checks cover the manifest, the first install, an update between two versions, and a load with the control plane stopped.
- **A new ADR.** Caching the pane on the device changes the pane-as-modules ADR's decision to serve every file `no-store` and its rejection of long-lived caching. The new ADR says what the worker caches, how one build replaces another, and why `no-store` stays on the HTTP responses.

### If the shared password lands

The `control-plane-password` branch puts HTTP Basic on every route. Two things then change:

- A browser fetches the manifest without credentials unless the link says otherwise. The link must be `<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials">`, or the manifest is answered `401` and the pane cannot be installed.
- A notification's push message comes from the push service, not from the page, so it carries no credential. That is fine: the worker only shows it. Opening the pane after a tap goes through the browser's Basic prompt as usual.

## Notifications

Web Push needs:

- **A VAPID key pair** on the control plane, made once and kept in the store. The public key goes to the pane so it can subscribe.
- **A subscription per device.** The pane asks for permission after a tap. A browser blocks a request that does not follow a tap, and iOS allows one only in an installed pane. The pane posts the `PushSubscription` (an endpoint URL and two keys) to a new route, `POST /push/subscriptions`, and the store keeps it in a new table. A `404` or `410` from the push service deletes the subscription.
- **Sending.** The control plane encrypts the message to the subscription's keys (RFC 8291) and posts it to the endpoint with a VAPID signature (RFC 8292). The endpoint belongs to the browser's push service: Google for Chrome, Mozilla for Firefox, Apple for Safari. The control plane already reaches the internet for the model APIs. A crate such as `web-push-native` builds the request, and the existing `reqwest` client sends it. The push service sees when a message is sent and its size, but not its text.
- **Triggers.** A tree changes into a state that needs the reader: a question, an approval, an error, or the end of its work. One notification per tree, replaced by the next one through the same `tag`, so a busy tree does not fill the phone with notifications. There is no notification while the pane is open and visible on that device.
- **The worker.** On `push`, it shows the notification with the session's summary as its text. On `notificationclick`, it focuses an open pane and moves it to `#s=<id>`, or opens a new one there.

Support: Chrome, Edge and Firefox on Android and desktop. Safari on macOS. Safari on iOS 16.4 and later, only for a pane added to the home screen.

## Standalone mode

- **Back.** An installed pane has no browser back button. Every screen already has its own back control (`.bs-back`), and Android's back gesture follows the history entries that `history.js` keeps.
- **The viewport.** A standalone window has no browser bar, so its height and its safe-area insets differ from a tab. The keyboard handling in `viewport.js` needs checking on the phone with `?viewport-report` in both modes.
- **Background polls.** `main.js` polls `/sessions`, `/nodes` and `/projects` every 3 to 5 seconds, with no check for visibility. An installed app spends more time in the background than a tab does. The pane should stop its polls while hidden and refresh everything at once when it comes back, so the reader sees the current state at once instead of after the next poll. This is not part of the worker, but installing makes it matter more.

## Proposed stories

- [ ] **S1 — The manifest and the icons.** `/manifest.webmanifest`, the PNG icons, the `apple-touch-icon` and `theme-color` tags. The pane can be installed from the Tailscale address.
- [ ] **S2 — Polls follow visibility.** The pane stops polling while hidden and refreshes on return.
- [ ] **S3 — The service worker caches the pane.** `/sw.js`, one cache per build, checked by `X-Bosun-Version`. The page and its files come from the cache. API and stream requests are not touched. The ADR.
- [ ] **S4 — Updates.** A new build installs in the background. The pane says so and reloads at a safe moment. A `sw.js` that removes itself is ready for an emergency.
- [ ] **S5 — Opening offline.** The pane opens without the control plane, says it cannot reach it, and shows the last session list it had.
- [ ] **S6 — Notifications.** VAPID keys, the subscriptions table and route, the push sender, the triggers, and the worker's `push` and `notificationclick` handlers. A control in the pane turns them on for the device.
- [ ] **S7 — The badge.** `setAppBadge` with the number of trees that need the reader.
- [ ] **S8 — Shortcuts and share target.** Manifest `shortcuts`, and `share_target` that opens the new-session sheet.

S1 and S2 are useful alone and carry little risk. S3 and S4 must ship together. S6 is the largest story and the one with the most value.

## Decisions to confirm

- **HTTPS on the homelab.** The homelab pane is served over plain HTTP on the LAN, which rules out everything above except a home-screen shortcut. Which address should the installed pane use: the laptop's Tailscale address, or the homelab behind Tailscale Serve or a certificate?
- **Cache first, or network first, for the page.** The proposal serves the pane from the cache and updates in the background, which is the fastest start but runs the old build for one load after an update. Network first keeps today's freshness, but saves nothing when the network is slow and needs care to avoid mixing builds.
- **Notification triggers.** Which states notify: questions and approvals only, or also errors and finished work?
- **The icon.** The Bosun Signal flag on `--sea-900`, or another design.

## Out of scope

- An app store package, such as a Trusted Web Activity for the Play Store.
- Sending messages or answers while offline.
- Notifications by email or chat.
