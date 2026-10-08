# ADR: The control plane sends Web Push notifications when a tree needs the reader

**Date:** 2026-10-08
**Author:** Pete

## Context

The reader learns that a session tree needs them only by opening the pane. Home groups trees by what needs the reader, from each session's overview: `asking` says the session's newest message is an unanswered question, and `tasks` counts its task list by status. A child's question reaches the reader through the root, so the root's `asking` is the tree's.

An installed pane has a service worker (see `2026-10-08-pane-kept-by-a-service-worker.md`) when it is served in a secure context. Web Push lets a server send a message to a browser's push service, which wakes the browser's worker even when the pane is closed. The browser chooses the push service: Google for Chrome, Mozilla for Firefox, Apple for Safari. Safari on iOS allows it only for a pane added to the home screen.

The control plane already reaches the internet for the model APIs. `ring` is already in the dependency tree, through `rustls`.

## Decision Drivers

- A tree notifies when it asks the reader a question, and when every task on its task list is complete.
- One notification per tree at a time: a busy tree must not fill the phone.
- No message text is readable by the push service.
- A restart of the control plane does not send old notifications again.
- No new process, no new config field, no crate added for what the dependency tree already has.
- Nothing a browser sends can make the control plane post to a plain HTTP address.

## Options Considered

**1. Web Push with VAPID, encrypted and signed with `ring`, triggered by polling the trees. (chosen)**

The control plane keeps one P-256 signing key, a table of subscriptions, and a loop that reads the live trees' state every 3 seconds while any browser is subscribed. A tree that changed into a state that notifies sends one message to every subscription.

**2. A Web Push crate such as `web-push` or `web-push-native`. (rejected)**

It saves about 150 lines, but brings its own crypto stack (`p256`, `aes-gcm`, `hkdf`, `ece`) beside `ring`, which already does ECDH, ECDSA, HMAC and AES-GCM. The encryption is fixed by RFC 8291, and its test vector is in the unit tests.

**3. Trigger on the loop's events instead of polling. (rejected)**

It would notify at once rather than within 3 seconds. But a question is an `ask` block, a task change is a `todos` write, and a child's question is routed to its root's thread: three places in the loop and the store, each to keep in step with what Home calls "needs you". Polling reads the same overview Home reads, so the two cannot disagree. The project map polls for the same reason.

**4. Skip the notification when a pane on that device is visible. (rejected)**

The server cannot tell which device is visible, and a worker that receives a push and shows nothing is penalized: Safari revokes the subscription, and Chrome shows a notification of its own. The notification always shows, and the pane closes a tree's notifications when the reader opens that tree.

**5. Notify through email or a chat service. (rejected)**

It needs an account and a config per operator, and it sends the text through another service. A push message is encrypted to the browser.

## Decision

`crates/bosun-control/src/push.rs` holds the whole mechanism.

- **The key.** `PushKey::load` reads the PKCS#8 P-256 key from the store's `push_key` table, or stores a new one: `Store::keep_push_key` keeps the first key stored, so two racing callers get the same key. `GET /push/key` serves its public key, uncompressed and base64url, as the browser's `applicationServerKey` takes it.
- **Subscriptions.** `POST /push/subscriptions` takes the browser's `PushSubscription.toJSON()`. `push::subscription` keeps it only when the endpoint is an `https` URL, `p256dh` is a 65-byte uncompressed point, and `auth` is 16 bytes. The store's `push_subscriptions` table keeps it by endpoint, with the pane's `Origin` header. `DELETE /push/subscriptions` removes it. The pane posts its subscription again on each load, so a control plane that lost it sends to it again, and drops one made with another key.
- **The loop.** `push::run` reads `store.push_subscriptions()` every `POLL_INTERVAL`, 3 seconds. With none, it does nothing more and forgets the trees' state. With some, it reads each live session's overview (not `creating`, not `stopped`), and `tree_states` gives each tree, by root id, `asking` from the root and `tasks_complete` when the tree's members have tasks and all are done. `triggers` compares the read with the one before: a tree that started asking notifies with its question, and one whose tasks became complete notifies with its summary. A tree that does both in one interval notifies once, for its question. The first read after a start, or after a time with no subscriptions, only records the state.
- **The message.** `notice` builds `{title, body, tag, url}`: the tag is the root id, the url is `/#s=<root>`, and the body is cut to 300 characters. `encrypt` and `seal` follow RFC 8291 with a new sender key and salt per message. `PushKey::authorization` signs an ES256 JWT whose audience is the push service's origin, valid for 12 hours, with the pane's origin as `sub` when it is `https`, and `mailto:bosun@localhost` otherwise. The request carries `TTL: 86400`, `Urgency: high` and `Topic` from the tree id, so the push service replaces a tree's message it still holds.
- **Delivery.** Each subscription is sent to in turn. A `404` or `410` removes the subscription; any other failure logs a warning with the push service's host, never the endpoint's path.
- **The worker.** On `push`, `sw.js` shows the notification with its tag and `renotify`, and sets a badge on the app's icon. On `notificationclick`, it focuses an open pane and posts it the tree's `#s=` link, or opens a new pane there. The pane closes the tree's notifications when it opens the tree, and sets the badge to the number of roots asking whenever it reads the session list.
- **The control.** The bell in Home's header asks for permission and subscribes, or unsubscribes. Over plain HTTP, or in a browser with no Push API, it says why it cannot turn on.

## Consequences

- A phone gets a notification within about 3 seconds of a tree asking or completing its tasks, when its push service delivers it.
- The push service sees when a message is sent, its size, and the tree's `Topic`, which is the root's id. It does not see the text.
- Every subscribed browser gets every notification. There is no per-device or per-tree choice.
- A tree with no task list never sends the "all tasks complete" notification.
- While any browser is subscribed, the control plane reads each live session's overview every 3 seconds, which is four small queries per session.
- The signing key is plaintext in the SQLite store. Anyone who can read the store can send notifications to the subscribed browsers.
- A control plane whose store is replaced has a new key, and every browser's subscription stops working until the pane is opened there again and subscribes.
- The control plane posts to any `https` endpoint a pane gives it. Anyone who can reach the API can make it post encrypted messages to an address of their choice. The control plane has no authentication yet, so this is everyone who can reach its port.

## Revisit When

- More than one person uses one control plane, so a notification belongs to a person rather than to every device.
- Readers want to choose which trees or which states notify.
- The 3-second delay matters, or the overview reads show in the store's load with many sessions.
- An endpoint other than the browser push services must be allowed or refused, such as a private push service on the operator's own network.
