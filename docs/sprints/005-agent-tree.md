# Sprint 005 — Personas and the agent tree

Sessions become a tree of agents. A persona — a model plus a system prompt, permission, and tool allowlist — is the unit of configuration; the user picks one for a session, and any agent in the tree may spawn child sessions under other personas. Subagents are full sessions: they run their own loop and executor on the parent's working copy, store their own transcript, and appear in `bosun list`. The user interacts only with the root of the tree; every child is watch-only. Tool calls travel one multiplexed tunnel per node instead of one per session.

Status: **planned**.

## Confirmed decisions

- A **persona** is `{ name, description, model, system_prompt, permission, allowed_tools }`, configured once on the control plane: a TOML registry in `serve.toml` plus one prompt file per persona under `<data dir>/personas/<name>.md`. `bosun clone` and `bosun dev` take `--persona` instead of `--model`; a `default_persona` config field names what sessions use when none is given. Personas resolve live by name; a missing persona fails the session and reports to its parent.
- Permission is **per-session and persona-declared**, with no tree-wide bound and no creation-time cap. A session's effective surface is its persona's `permission` intersected with its `allowed_tools` (a tool allowlist, commonly `"*"`), enforced by that session's own executor. `bosun clone` no longer takes `--permission`; a read-only session is a read-only persona. A read-only root may spawn a read-write child when the child's persona declares it.
- **Persona switching.** The root's persona may be switched mid-session: a full swap of model, system prompt, permission, and tool set, applied from the next turn. The executor's permission toggles live through the existing `/permission` mechanism; the tool allowlist is filtered control-plane-side per turn. Running children are unaffected, each keeps its own persona. The switch is recorded in the transcript so continuity is visible. Children are watch-only and never switch by user action.
- A subagent **is a session**. Sessions form a tree: every session has `parent_id` (the session that spawned it; none for a root) and `owner_id` (the tree root, always). A child session runs on its parent's node and working copy with its own loop, its own executor process, its own executor port, and its own message set — which is what makes siblings and parent truly concurrent. Multiple root sessions may share one node.
- **Communication is authored messages, never transcripts.** A child reports by ending its turn without an ask: it authors a completion report to its parent and stops. Any child event — report, ask, or failure — wakes the parent, and events that land mid-turn queue until the turn ends. Each wake's turns read the thread as it stood when the wake began (plus the wake's own tool traffic), so an event that lands mid-wake is invisible to the running wake and surfaces only in its own queued wake. Each wake carries a compact **manifest** of live children (`id`, `persona`, `state`, last authored message) in the system prompt, so the parent never loses track of what it spawned. A child leaves the manifest once its completion is handled and not resumed. A parent asks for detail or redirects with `message_child`; the child formulates its answer from its own context, resumed from its archived thread.
- **Ask gating is hierarchical.** `ask` at a child raises a question to its parent, which answers on the user's behalf when confident, denies with a reason, or surfaces the question upward. Only questions no ancestor resolves reach the user, at the root. A surfaced ask binds to the originating leaf; the user's answer routes verbatim down to that leaf. When the user redirects instead of answering, the root model decides the pending ask's fate.
- **Only the root accepts user input.** Every session appears in `bosun list` and is joinable, but attaching to a child is watch-only. The interrupt ladder is root-only: the first interrupt stops the root's own session, the next stops everything else in the tree. User-initiated stops hold until the user acts; crash-interrupted children report to their parent, which re-decides each (resume, abandon, or swap persona). Stopping the owner cascades the whole tree. The cause of an interrupt — user or crash — is recorded on the session.
- **Recursion is unlimited.** Any session whose `allowed_tools` permits `spawn` may spawn children and supervises them exactly as the root does, one level down. The persona catalog is advertised to spawn-capable sessions as `name + description`, never as prompt text.
- **Tool surface.** `spawn(persona, instructions)` creates a child session and returns its id without blocking; `message_child(id, text)` resumes or redirects a child; `ask` is contextual — at the root it reaches the user, at a child it reaches its parent; `todowrite` is root-only. Every other tool is available to any session whose persona allows it. The old `spawn_subagent` tool that returned a summary is gone.
- **Transport is one tunnel per node.** A node holds one outbound connection to the control plane; every tool call is a logical connection addressed by session id, and the node relay dials that session's executor port. This supersedes the one-tunnel-per-session arrangement of `2026-08-21-nodes-dial-out-only.md` as carried into `2026-08-30-tool-protocol-over-tunnel.md`; the frame codec, per-connection flow control, and relay survive unchanged.
- **Repo standards.** The loop scans the working copy for `AGENTS.md` and `CLAUDE.md` and injects a presence notice into each session's context; the contents are read on demand with file tools. Persona prompts stay purely about role and behaviour.
- **States and store.** The five session states are reused unchanged; a child that completes transitions to `stopped`, and completed-versus-failed-versus-user-stopped is distinguished by the final authored event, not the state. A stopped or interrupted child resumes when its parent messages it, from its archived thread. `model_calls` stay per session; cost rolls up by `owner_id` in the views.
- The `Block::Subagent` transcript hack and the sprint-002 "subagent type is `{ name, model, permission }`" model are superseded by personas and real child sessions. Stored transcripts that contain `{"kind":"subagent",...}` blocks stop deserializing once `Block::Subagent` is gone, so a pre-S4 session that used `spawn_subagent` can neither run a turn nor replay its transcript; there is no migration (pre-1.0 dev data). The S5 rename from `child_report` to `child_event` (with its `event_kind` field) likewise breaks transcripts written by the committed S4 build, which serialize `{"kind":"child_report",...}` blocks; no migration (pre-1.0 dev data). Tests drive the loops with per-loop scripted providers plus an event-injection seam, so interleavings are deterministic.

## CLI surface

`clone`, `dev`, `list`, `open`, `stop`, `nodes`

- `bosun clone --persona <name> <git-url>` and `bosun dev --persona <name>` replace `--model` and `--permission`.
- Persona switch on a session: `POST /sessions/{id}/persona` plus a command in the interactive client and the web pane, root sessions only.
- `bosun list` shows the tree: children grouped under their owner, each with persona, state, and node.
- `bosun open <session>` attaches to a session. Attaching to the root is interactive; attaching to a child is watch-only.
- `bosun stop <session>` on a root stops its whole tree; on a child it stops that child's subtree.

## User stories in implementation order

- [ ] **S1 — Persona registry**

As a user, I want to configure personas once on the control plane and pick one when I start a session, so the session runs under the model, system prompt, permission, and tool set I chose.

- `ControlConfig` replaces `models`/`subagents`/`default_model` with `personas`, `default_persona`, and a `models`-per-persona binding (`model` names a provider entry as today).
- `PersonaConfig` has `model`, `permission`, `allowed_tools` (default `"*"`), `description`, and a prompt body read from `<data dir>/personas/<name>.md` when present; boot validates the model exists and tool names are canonical.
- `bosun clone`/`bosun dev` take `--persona`; the choice is stored on the session and the session's executor is spawned with the persona's permission and allowed-tool set.
- The persona catalog is resolved live by name at session start and at spawn; an unknown persona is a clear error.

- [ ] **S2 — Persona switch on the root**

As a user, I want to switch the root session's persona mid-session, so I can change role, model, or safety mid-task without losing the thread.

- `POST /sessions/{id}/persona` takes a persona name; a root session's persona changes live, and the switch is recorded in the transcript as an event.
- The next model call uses the new persona's model and system prompt, and the tool schema is rebuilt from the new persona's `allowed_tools`.
- When the new persona's permission differs, the executor's permission toggles live through `/permission`; the stored session permission follows.
- A switch to an unknown persona is refused with a clear error; a switch applies to the root only and never to running children.

- [ ] **S3 — Sessions form a tree**

As a developer, I want sessions to carry parent and owner links and for child sessions to be first-class store rows, so any agent can run as a session with the existing lifecycle.

- `Session` and the `sessions` table gain `parent_id`, `owner_id`, and `persona`; `model` is derived from the persona.
- A child session is born `creating` on its parent's node and directory, with its own executor process and port, and moves to `running` on its assignment.
- `bosun list` and the sessions API group children under their owner; children are joinable and read-only over the API.
- The interrupt cause (user or crash) is recorded on the session so stop semantics can differ.

- [ ] **S4 — Spawn and report**

As a user, I want the session agent to hand a task to a child session under a chosen persona and get back a report, so work routes to the right model and prompt without leaving the session.

- `spawn(persona, instructions)` creates a child session whose first user message is the assignment and returns the child's id; the parent's turn continues without waiting.
- The child loop mirrors the root loop except for its role: ending a turn without an ask authors a completion report to its parent and stops; a root ending without an ask waits for the user as today.
- A child runs concurrently with its parent and siblings (own executor), and its report, transcript, and model calls are stored on the child session.
- The parent's transcript renders the child as authored events, not raw tool traffic.
- Note: until S6 delivers ask gating, a child that calls `ask` hangs — `ask` is still advertised to children, and no parent-answer path exists yet.
- Note: until S8 owns the stop cascade, a completed child keeps its executor, node session record, and tunnel until the tree is stopped, so a long-lived tree accumulates live executors.

- [ ] **S5 — Authored events and the manifest**

As a developer, I want the parent loop to receive child events and track outstanding children, so supervision is event-driven and nothing is forgotten.

- Child events (report, ask, failure) are delivered to the parent's inbox and wake the parent; events arriving mid-turn queue until the turn ends; each wake is handled serially.
- Every wake injects a manifest of live children — id, persona, state, and last authored message — into the system prompt.
- A child leaves the manifest once its completion is handled and the parent does not resume it.
- A parent asks a child for detail with `message_child(id, text)`; the child resumes from its archived thread, formulates an answer from its own context, and reports.
- The loop tests add an event-injection seam so child events are delivered in a scripted order.
- Note: building the manifest scans the parent's full archived thread for each child's last authored message — cheap while transcripts are small; revisit with a per-child lookup if archives grow.

- [ ] **S6 — Ask gating and user surfacing**

As a user, I want only questions worth asking to reach me, answered or denied by the agents that know the answer, so I am not interrupted by every worker's doubt.

- `ask` at a child raises a question to its parent instead of to the user; at the root it reaches the user as today.
- A parent answers a child's question on the user's behalf when confident, denies with a reason, or surfaces the question upward.
- A surfaced ask binds to the originating leaf; the user's answer routes verbatim down the tree to that leaf and resumes it.
- When the user redirects instead of answering, the root model decides the pending ask's fate; a cancelled ask is notified to the leaf.

Routing is mechanical in the control plane, not model-mediated. Surfacing a bound ask records a durable `pending_asks` store row (root session, bound child, the surfaced question, and the surfaced Ask block's message id — the block itself may be compacted away); the answer path never wakes the root model. A root has one pending ask at a time, and a bound child must itself be waiting on an unanswered question (`ask` with a `child_id` is refused otherwise, and a second surface while one is pending is refused). `POST /sessions/{id}/messages` distinguishes an answer (default) from a redirect (`redirect: true`). An answer with no pending binding, or with the binding's child gone, is an ordinary root message. A redirect never clears the binding: the root model wakes with the surfaced ask still in its thread and decides — messaging the bound child via `message_child` cancels the pending ask and clears the binding, ending the turn without messaging it holds, so a later answer still routes. The terminal client sends redirects with Ctrl-R, the web pane with a Redirect button.

- [ ] **S7 — Recursive tree and watch-only clients**

As a user, I want any agent to be able to delegate and for the tree to be visible and inspectable, so deep work composes and I always know what is running.

- Any session whose `allowed_tools` includes `spawn` may spawn children; each level supervises its own children with its own manifest and gating.
- The persona catalog is advertised to spawn-capable sessions as name and description; `todowrite` stays root-only.
- Attaching to a child is watch-only: the client renders the child's live transcript and state but sends no input.
- `bosun list` renders the whole tree with states; the terminal client and web pane show child activity and expansion into a child's thread.

- [ ] **S8 — Interrupt, crash, and stop**

As a user, I want interrupt and stop to behave predictably across the tree and a crash to lose nothing, so I can halt work and recover by hand.

- From the root, the first interrupt stops the root's own session; the next stops every other session in the owner's tree. Interrupted children stay stopped until the user acts.
- On control-plane boot, `running`/`creating` sessions become `interrupted`; crash-interrupted children report to their parent, which re-decides each — resume from archive, abandon, or swap persona.
- `bosun stop` on the owner cascades to the whole tree; stopping a child cascades its subtree.
- A stopped or interrupted child resumes from its archived thread when its parent messages it.
- Note: a control-plane crash between the node starting a child's executor and the child row existing can orphan an executor on the node.
- Note: a crash between a child appending its event and the parent's loop being woken loses the ephemeral wake — the event itself is durable in the parent's thread, and the parent sees it on its next wake, so the loss is a delay, not data loss; revisit whether the parent should rescan for unhandled events after a crash.

- [ ] **S9 — One tunnel per node**

As a node operator, I want the node to hold a single connection to the control plane for all its sessions, so a tree of sessions does not multiply connections.

- The node establishes one tunnel per node; tool-call logical connections carry the session id, and the relay dials the session's executor port.
- The control plane keys its tunnel registry by node; a session's tool calls reach its executor regardless of how many sessions share the node.
- Tunnel reconnect keeps every session's executor running; a dropped tunnel restores all of a node's sessions on reconnect.
- Flow control stays per logical connection; a protocol violation tears down the node tunnel, and sessions reconnect together.

- [ ] **S10 — Repo standards scan**

As a user, I want every agent in a session to know the repo's governing documents exist, so work follows the standards the repo sets without every agent guessing.

- The loop scans the working copy for `AGENTS.md` and `CLAUDE.md` and injects a presence notice into each session's context, for the root and every child.
- Contents are not injected; an agent reads them on demand with file tools when its task needs them.

## Out of scope

No real authorization model (single-user, no security): a child's permission is its persona's, enforced by its own executor, with no tree-wide bound and no escalation approval channel. No per-session budget or concurrency caps — the model decides how many children to spawn. No user messaging to children (watch-only stays watch-only). No compaction of child threads beyond what each session's loop already does. No cross-session transcript access by agents.
