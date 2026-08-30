# Sprint 002 — Agent executor

Bosun runs its own agent sessions. The agent loop lives on the control plane, tool calls run on nodes, and the opencode dependency is removed. A terminal client drives sessions; a web pane follows.

Status: **complete**. All eleven stories are implemented and tested.

## Confirmed decisions

- The agent loop runs on the control plane per session. Nodes execute tools. Session history lives in a SQLite store on the control plane.
- Nodes dial the control plane only. Tool calls ride the existing session tunnel as HTTP/1.1 over a logical stream; the node relay dials the executor on loopback. Cancellation is `POST /tool/{id}/cancel`.
- The executor is one `bosun executor` process per session, on its own loopback port, spawned and restored by the node like `opencode serve` is today.
- The loop is hand-rolled against the provider API over HTTP and SSE. One canonical tool list with JSON Schemas; a small adapter per provider serializes it and parses tool calls back.
- Tool surface: `shell`, `file/read`, `file/write`, `edit`, `grep`, `glob`, `ask`, `todowrite`, `git` (read and commit, no push), `webfetch`. `websearch` is out of scope.
- Permission modes: read-only and read-write. Read-only keeps mutating tools out of the model's schema and refuses them at the executor.
- Models and subagent types are configured once on the control plane in TOML; keys are `env:VAR` references.
- Skills follow the agent skills specification, are discovered from the working repo and injected from the control plane, and run with full authority. They are advertised to the model and loaded on demand. Skills launch subagents; subagent types are `{ name, model, permission }`.
- Session states: `creating`, `running`, `waiting_for_input`, `interrupted`, `stopped`. A session is born with an optional prompt; with one the loop starts immediately, without one it idles. `waiting_for_input` means the turn ended.
- Any crash kills the in-flight turn, never the thread. On restart, `running` and `creating` become `interrupted`; loops rehydrate from the store; tunnels reconnect on their own.
- Context compaction replaces the retired tail of the transcript with a summary; the full transcript stays archived in the store and visible in the pane.
- Every model call is recorded from day one, so the deferred cost and efficiency views have data when they arrive.
- The terminal client is the first surface: `bosun open` becomes an interactive attach. The web pane follows on the same API and event stream.
- The old opencode dependency is gone. The executor replaced `opencode serve` as the node's loopback server, and the control-plane gateway that routed opencode client traffic by host was removed.
- This sprint supersedes parts of `2026-08-20-no-opencode-config-injection.md` (models return to the control plane), `2026-08-18-nodes-source-of-truth-for-sessions.md` (the control plane becomes the source of truth), and `2026-08-22-session-host-routing-only.md` (the gateway routes by host no longer). New ADRs record the agent loop, the executor, the tool protocol over the tunnel, the store, the session states, and the tool surface.

## CLI surface

`serve`, `node`, `executor`, `clone`, `dev`, `list`, `open`, `stop`, `nodes`, `config`

- `bosun clone --node <name> [--model <model>] [--permission read-only|read-write] <git-url> [ref] [--message <prompt>]`
- `bosun dev` gains the same flags, with the directory picked by browsing.
- `bosun open <session>` attaches an interactive terminal client; with no id it lists sessions to pick from.
- `bosun list` shows session states truthfully: running, waiting for input, interrupted, stopped.

## User stories in implementation order

- [x] **S1 — Executor on the node**

As a developer, I want a `bosun executor` process that serves the tool API on loopback, so the control plane's agent loop can run commands and edit files in the session's working copy.

- `bosun executor --session-dir <dir> --port <n> --permission <mode>` starts an HTTP server on `127.0.0.1:<n>`.
- Routes: `POST /tool/shell`, `/tool/file/read`, `/tool/file/write`, `/tool/edit`, `/tool/grep`, `/tool/glob`, `/tool/git`, `/tool/webfetch`; `POST /tool/{id}/cancel`; `GET /health`.
- Shell output streams; cancel kills the command; output is bounded and lost with the session.
- Read-only refuses shell, file write, edit, and mutating git.
- The node spawns the executor in place of `opencode serve`; `state.json` keeps pid and port; restore-on-boot and kill-on-stop reuse the existing lifecycle.

- [x] **S2 — SQLite store on the control plane**

As a developer, I want the control plane to persist sessions and transcripts in SQLite, so the thread survives a crash and the state shown is the state that is.

- One SQLite file in the control plane data directory, WAL mode, bundled.
- Tables: `sessions`, `messages`, `tool_calls`, `model_calls`.
- The store is the source of truth for sessions; the node registry keeps only node liveness.

- [x] **S3 — Model registry and provider adapter**

As a user, I want to configure models once on the control plane, so every session can use my provider keys without per-node setup.

- Models and subagent types live in control-plane TOML; keys are `env:VAR` references resolved at boot.
- One canonical tool list with JSON Schemas; a per-provider adapter serializes it and parses tool-call messages back.
- `bosun clone` and `bosun dev` accept `--model`; the choice is recorded on the session.

- [x] **S4 — Session creation and the agent loop**

As a user, I want a cloned or dev session to start working immediately, so I do not need a separate attach step.

- `bosun clone --message <prompt>` creates a session and the loop starts the first turn at once; without a message the session idles at `waiting_for_input`.
- The loop builds each request from the system prompt, the skill advertisements, the tool schemas, and the transcript window; it streams the completion, dispatches tool calls over the tunnel, and feeds results back.
- The `ask` tool ends a turn and presents options to the user; `waiting_for_input` is truthful.
- Interrupt: a control channel cancels the model call and posts `cancel` to the executor.

- [x] **S5 — Session API and event stream**

As a developer, I want the control plane to expose sessions over REST and live events over SSE, so the terminal client and web pane can be thin clients of one protocol.

- REST: `GET /sessions`, `GET /sessions/{id}`, `POST /sessions`, `POST /sessions/{id}/messages`, `POST /sessions/{id}/interrupt`, `POST /sessions/{id}/permission`, `POST /stop`.
- `GET /sessions/{id}/events` is SSE; every event carries a monotonic sequence from the store and replay starts at `after=<seq>`.

- [x] **S6 — Terminal client**

As a user, I want `bosun open <session>` to attach interactively to a session, so I can watch it work, answer it, interrupt it, and switch permission mode from my terminal.

- Renders the streaming transcript: text, tool calls, results, file changes, subagent activity.
- Input box, interrupt key, permission toggle, reconnect by `Last-Event-ID`.
- `list`, `clone`, `stop` stay non-interactive.

- [x] **S7 — Crash recovery and compaction**

As a user, I want sessions to survive a control-plane or node restart, so a crash never loses my thread.

- On boot the control plane marks `running` and `creating` sessions `interrupted`, rehydrates their loops from the store, and lets tunnels reconnect.
- When the request window fills, the loop compacts: the retired tail becomes a summary in the model context, and the full transcript stays archived in the store.
- The pane shows the full archived transcript regardless of compaction.

- [x] **S8 — Skills and subagents**

As a user, I want the agent to use the working repo's skills and the control plane's injected skills, and to hand work to subagents on the right model, so hard work routes to the right model automatically.

- Skill discovery scans the session's working copy and the control plane's injected skills.
- Skills are advertised to the model and loaded on demand through the `skill` tool.
- `spawn_subagent(type, instructions)` runs a nested loop on the control plane with the type's model and permission, synchronously; its events appear in the session transcript.

- [x] **S9 — Model-call metering**

As a user, I want every model call recorded with tokens and cost, so the deferred budgets and efficiency views start with real data.

- The loop records one `model_calls` row per completion and compaction, with model, provider, tokens, and cost.
- A per-session summary endpoint exposes them. Dashboards are out of scope.

- [x] **S10 — Web pane**

As a user, I want a web view of nodes and sessions with the same live transcript, so I can manage everything from one place.

- Nodes list, sessions list, and a session view with the transcript, diffs, and subagent activity.
- Start, stop, resume, permission switch, and model choice reuse the session API and SSE stream.

- [x] **S11 — Decommission opencode**

As a developer, I want the opencode dependency and its routing gone, so the system has no external agent server to break.

- Remove the gateway's host routing, `session_origin`, the `--cors` flag, `connect_command`, and the per-session `XDG_DATA_HOME` data directories.
- The executor replaces `opencode serve` as the node's loopback target.
- Update `docs/developer/workflows/local-development.md` and the e2e test to drive a session through the terminal client or the session API.

## Out of scope

No one-shot task API, no node allocation, no tracker integration, no done-criteria verification, no budget enforcement, no `websearch`, no `git push`, no risky-action detection, no per-session containers, no multi-user, no cost or efficiency dashboards.
