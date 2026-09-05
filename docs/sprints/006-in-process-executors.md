# Sprint 006 — In-process executors and a typed tool protocol

Sessions stop running their tools in one `bosun executor` process each. The node hosts every session's executor as a thin in-process state object, and tool calls ride the tunnel as typed frames instead of HTTP/1.1. No executor process, port, pid, or health poll exists; `bosun executor` as a subcommand is gone. See the decision and its record in `../adrs/2026-09-05-in-process-executors.md`.

Status: **in progress**.

## Confirmed decisions

- **Executors live in the node process.** `NodeManager` keeps one `ExecutorState` per session (`session_dir`, live `permission`, `running` shells) and registers it under the session id. `clone`, `dev`, and `start` construct the state instead of spawning a process, binding a loopback port, and waiting on `/health`. `state.json` drops `executor_port` and `pid`; boot restore rebuilds states from the session rows.
- **Tool calls are typed frames.** The control plane sends one typed request (`run_id`, tool, args) per fresh logical connection; the node relay resolves the session's `ExecutorState` and dispatches. Responses are typed: one result for the JSON tools, an `out`/`done` stream for shell. Cancel is a typed message on its own connection. `skills`, `skill/read`, and `repo_standards` stay tool calls in the typed protocol.
- **The tunnel transport is unchanged.** Frame codec, per-logical-connection flow control, reconnect, session-addressed Opens. Command delivery over `/poll` keeps its HTTP shape. Only the payload on a logical connection changes.
- **Permission stays enforced at the executor.** `ExecutorState.permission` gates shell, `file/write`, and `edit`; a persona switch forwards a typed permission update to the session's state, best-effort and store-authoritative. The per-turn tool allowlist stays filtered on the control plane.
- **Shell semantics survive.** Each shell is an OS child in its own session and process group, owned by one task that reaps it; the group is killed on cancel, on connection drop, and on session stop — so `stop` now kills in-flight shells instead of orphaning them.
- **Blocking tool code moves to the blocking pool.** The synchronous file, directory, and skill-presence tools run on `spawn_blocking`, because all sessions now share the node's runtime.
- **`bosun-executor` becomes a library.** Its axum server and the `bosun executor` subcommand are deleted; `bosun-node` depends on the crate and owns relay and dispatch.
- This sprint supersedes the executor-process and HTTP-tool-transport parts of sprints 002 and 005; the ADR carries the record.

## CLI surface

- `bosun executor` and its `--session-dir`/`--port`/`--permission` args are removed. No other CLI surface changes.

## User stories in implementation order

- [ ] **S1 — Executor state in the node process**

As a developer, I want each session's executor to be in-process state, so no executor process, port, or pid exists.

- `start_in_dir` builds an `ExecutorState` (`session_dir`, `permission`, `running`) and registers it under the session id; `clone`, `dev`, and `start` no longer spawn a process or wait for health.
- `SessionRecord` and `PersistedSession` drop `executor_port` and `pid`; `state.json` stops writing them and restore rebuilds states from the remaining fields.
- `stop` tears down the session's state and kills its running shells.
- `pick_free_port`, `wait_for_health`, `kill_pid_if_alive`, and `is_bosun_executor` are deleted with their tests.
- Manager tests that asserted executor-startup-through-a-subprocess (the health-timeout proof, the stale-pid guards) become real in-process start and stop tests.

- [ ] **S2 — Typed tool protocol between control plane and node**

As a developer, I want tool calls to ride the tunnel as typed messages, so the tool path needs no HTTP.

- The payload on a logical connection is a typed request (`run_id`, tool, args) and typed responses: one result, or a stream of `out` events ended by `done`.
- A cancel is a typed message on its own connection, with today's semantics.
- The control plane's `TunnelToolExecutor` sends typed requests instead of HTTP/1.1; the hyper client, `sse_stream`, and the SSE parser leave the tool path.
- The node relay resolves the session's `ExecutorState` instead of dialing `executor_port`; an unknown session closes the connection so the call fails rather than hangs.
- `skills`, `skill/read`, and `repo_standards` move onto the typed path as ordinary tool calls.

- [ ] **S3 — Executor as a library, no HTTP server**

As a developer, I want the executor crate to expose a typed interface, so nothing in it knows about HTTP or processes.

- `bosun-executor` drops the axum server, its routes, and `server::serve`; it keeps the tool functions, `ExecutorState`, and the shell-run machinery behind a typed surface.
- `bosun-node` depends on `bosun-executor` and owns dispatch.
- The `bosun executor` subcommand, `ExecutorArgs`, and `run_executor` are removed from `cmd`.
- Executor tests stop booting HTTP and call the typed interface directly; shell streaming tests keep asserting the same `out`/`done` semantics.

- [ ] **S4 — Blocking tools on the blocking pool**

As a developer, I want no tool call to stall the node's shared runtime, so one session's heavy file or grep work does not stall the tunnel and the other sessions.

- Every synchronous file, directory, and skill-presence tool runs on `spawn_blocking`; `git` and `shell`, already async over child processes, are checked and stay.
- Output caps (`MAX_SHELL_OUTPUT_BYTES`) and the drain grace period survive the move.

- [ ] **S5 — Permission over the typed protocol**

As a developer, I want a persona switch to reach the session's executor as a typed update, so read-only stays enforced at the executor.

- A typed permission message updates `ExecutorState.permission`, mirroring today's `POST /permission`; best-effort and store-authoritative semantics are unchanged.
- Dispatch enforces the current permission; read-only refuses shell, `file/write`, and `edit` regardless of the caller.

- [ ] **S6 — Lifecycle parity and no orphans**

As a developer, I want node restarts, updates, and stop cascades to behave with in-process executors, so recovery loses nothing and nothing is orphaned.

- Boot restore and the update-restart path rebuild every session's `ExecutorState` from `state.json`.
- The stop cascade kills each stopped session's in-flight shells leaves-first; no executor process can be orphaned, including in the window between a child start and its session row existing.
- The protocol-concurrency tests (`concurrent_streams`) and the relay tests, which bound stub HTTP executors on real ports, move to in-process executors or typed callers.

## Out of scope

No OS-level isolation between sessions (single-user; a node crash takes its sessions with it). No per-session CPU, memory, or file-descriptor budgets. No change to the tunnel transport or to command delivery over `/poll`. No change to the agent loop, personas, session states, or the store.
