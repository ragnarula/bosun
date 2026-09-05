# ADR: Session executors run in the node process on typed frames

**Date:** 2026-09-05
**Author:** Raghav

## Context

Sprint 002 made each session's executor one `bosun executor` process on the node, started as `bosun executor --session-dir <dir> --port <n> --permission <mode>`, serving its tool API on its own loopback port, with port and pid kept in the node's `state.json` and restored at boot. That arrangement is recorded in `2026-08-30-executor-per-session.md`. Tool calls ride the node tunnel as HTTP/1.1 over a logical connection that the node relay splices to the executor's loopback port; that protocol is recorded in `2026-08-30-tool-protocol-over-tunnel.md` and re-addressed per session by `2026-09-03-one-tunnel-per-node.md`. Sprint 005 (`2026-09-03-agent-tree.md`) makes a tree of sessions share one node and records the consequences of a live tree holding one executor process per running or stopped-but-resumable session.

The process boundary is now mostly machinery, and several of its failure modes are documented consequences:

- A live tree keeps one executor process per running or stopped-but-resumable session, so a long-lived tree accumulates processes until the owner is stopped.
- Every start needs a free port, a health poll, a `Child` to kill, and a pid; `state.json` persists both port and pid.
- A node restart or self-update changes pids, so killing a stale executor must first scan `ps` to avoid signalling a reused pid (`kill_pid_if_alive` in the node manager).
- A control-plane crash between the node starting a child's executor and the child's session row existing orphans an executor on the node.
- The isolation the process boundary promised is not delivered. The node never monitors or restarts a crashed executor, so a crashed executor is a dead session until the node restarts anyway. And `bosun stop` kills only the executor process, orphaning its in-flight shells, which run in their own sessions via `setsid`.

## Decision Drivers

- Remove the executor's process identity: no port, no pid, no health poll, and no persisted copy of either.
- Tool calls must keep running inside the session's working copy under the session's permission, stay cancellable, and stream output.
- One session's slow tool call must not stall the node's other sessions now that they share a process.
- Single-user, no security model: the control plane is trusted, and it already decides which session a logical connection names.

## Options Considered

- **Keep one `bosun executor` process per session.** Rejected: it is the machinery above, its crash isolation is not real (a crashed executor is not restarted, so the session is dead until a node restart), and `bosun stop` orphans in-flight shells. The ADR's promise that "the node restarts it on demand" is not implemented.
- **One executor process per node hosting every session.** Rejected: it keeps the process identity, ports, and pid bookkeeping for no gain — a crash still takes down every session on the node, which is exactly what in-process embedding does without the machinery.
- **Embed the executor as one task per session behind its own loopback listener.** Rejected: the loopback port existed only because the executor was a separate process. An in-process executor has nothing to gain from a port, and keeping one keeps `executor_port`, the relay dial, and most of the start machinery.
- **Keep the executor's HTTP surface and have the node serve each session's Router over each logical connection.** Rejected: HTTP/1.1 and SSE over the tunnel exist only to reach a process on a port. With the executor inside the node, HTTP is a second protocol to maintain and debug with no boundary left to justify it, and it keeps a hyper client and an SSE parser on the control plane.
- **Terminate HTTP at the node and dispatch typed calls underneath.** Rejected: the control plane still speaks HTTP, so the node must re-encode HTTP/1.1 and SSE by hand. It keeps the codec it removed and adds new code to produce it.
- **Stateless per-call dispatch with permission carried on each call.** Rejected: permission is per-session state that must change live on a persona switch, and the agent-tree ADR makes the session's own executor the enforcement point. Erasing that boundary moves enforcement to the caller.
- **Typed tool frames end to end (chosen).** Each tool call is a typed message on its own logical connection; the executor is a thin per-session state object with a typed interface, and the relay dispatches to it in-process. HTTP/1.1, SSE, and their parsers leave the tool path entirely.

## Decision

**The node hosts every session's executor in-process, and tool calls ride the tunnel as typed frames.**

**Residency and lifecycle.** `NodeManager.start_in_dir` builds one `ExecutorState` per session — `session_dir`, a live `permission`, and the `running` shell map, the same shape the executor crate keeps today — and registers it under the session id. `clone`, `dev`, and `start` construct this state instead of spawning a `bosun executor` process, binding a loopback port, or waiting on `/health`. `stop` removes the state and kills the session's running shells; it no longer kills a process and orphans them. `state.json` keeps `PersistedSession` rows without `executor_port` and `pid`; boot restore rebuilds the states from the session row's directory, permission, and repo fields. `pick_free_port`, `wait_for_health`, `kill_pid_if_alive`, and `is_bosun_executor` are deleted.

**Tool protocol.** Each tool call is one typed request — `run_id`, tool name, arguments — sent by the control plane's `TunnelToolExecutor` on a fresh logical connection addressed to the session. The node relay resolves the session's `ExecutorState` instead of dialing a port; a session the node does not run closes the connection, so the call fails rather than hangs. Responses are typed: one result for the JSON tools, and for shell a stream of `out` events ended by a `done` code. A cancel is a typed message on its own connection, mirroring today's `POST /tool/{run_id}/cancel`. The internal presence calls (`skills`, `skill/read`, `repo_standards`) stay tool calls in the typed protocol. The tunnel transport is untouched: the frame codec, per-logical-connection flow control, reconnect, and session-addressed Opens all survive; only the payload on each logical connection changes.

**Executor internals.** The executor crate stops being an HTTP server. Its tool functions and `ExecutorState` form a library the node links. Shell execution keeps today's semantics: each shell is an OS child in its own session and process group, owned by one task that reaps it and answers a kill signal; the owner task kills the group on cancel, on connection drop, and on session stop. The synchronous file, directory, and skill-presence tools run on the blocking pool (`spawn_blocking`), because they now share the node's runtime with every other session and the tunnel.

**Permission.** `ExecutorState.permission` stays the enforcement point: dispatch refuses shell, `file/write`, and `edit` for a read-only session regardless of the caller. A persona switch forwards a typed permission update to the session's state; best-effort and store-authoritative, as the agent-tree ADR records. The per-turn tool allowlist stays filtered on the control plane.

**CLI and crate.** The `bosun executor` subcommand, its `ExecutorArgs`, and `run_executor` are removed from `cmd`. `bosun-executor` becomes a library dependency of `bosun-node`; the node owns relay and dispatch.

**Unchanged.** Command delivery over `/poll` (`clone`, `dev`, `start`, `stop`) keeps its HTTP shape. The agent loop, personas, session states, and the store are untouched. Shell output caps and the drain grace period survive.

This supersedes the process decision in `2026-08-30-executor-per-session.md` and the HTTP payload decision in `2026-08-30-tool-protocol-over-tunnel.md`, and the loopback-dial sentences in `2026-09-03-one-tunnel-per-node.md`; those files carry supersession notes. The transport decisions of `2026-09-03-one-tunnel-per-node.md` survive.

## Consequences

- No executor process, port, or pid exists. Orphaned executors are impossible; `state.json` carries neither field; the stale-pid kill and the health poll are gone. A stopped-but-resumable child in a tree costs one small in-memory state, so the agent tree's process accumulation stops being a cost.
- Process-level fault isolation is gone. A fault that kills the node process kills every session it hosts. Panics in a per-connection or shell-owner task abort only that task; the executor code must not panic in shared paths. Blocking tool work no longer occupies one session's private runtime and must be bounded on the blocking pool. The single-user scope accepts this.
- `bosun stop` now kills a session's in-flight shells instead of orphaning them; this fixes a latent leak but is a behaviour the node now owns explicitly. Two shell-orphan cases remain, both pre-dating this change and unchanged by it: a shell whose leader has already exited leaves backgrounded grandchildren running (the process-group kill refuses a reaped leader rather than signal a possibly reused pid), and shells in flight when the node process itself is killed or restarts are not signalled — nothing kills them on the way down.
- A node update or restart tears the executor states down with the node and rebuilds them at boot from `state.json`, where today it kills stale processes and re-spawns them. Same recovery, less machinery.
- The control plane's tool client, the agent crate's SSE parser, and the executor's HTTP layer are rewritten away; their protocol tests are rewritten to typed calls or removed.
- The window between the node starting a child session and the control plane recording it still exists, but it now leaves at most a stray in-memory state and a working-copy directory, never a process.

## Revisit When

- Sessions must be isolated from one another at the OS level — containers, cgroups, or per-session resource budgets.
- An executor must outlive the node process, run outside the node binary, or be disposable per session again.
- The tunnel transport needs per-connection semantics that a byte stream cannot carry; the typed payload layer then moves into the frame codec.
