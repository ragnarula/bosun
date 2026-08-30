# ADR: One executor process per session on the node

**Date:** 2026-08-30
**Author:** Raghav

## Context

The MVP started `opencode serve` on each node session to run the agent. Sprint 002 removes opencode; the node must instead host the process that executes tools against the session's working copy. That process must be owned and recycled by the node exactly as `opencode serve` was: spawned on clone, restored on boot, killed on stop.

## Decision Drivers

- Tool calls must run inside the session's working copy with the session's permission mode.
- A shell command must be cancellable and its output streamed, so a runaway `cargo test` can be stopped and its tail seen live.
- The process must be disposable: a crash kills only its in-flight work, and the node restarts it on demand.

## Options Considered

- **Embed the executor in the node process.** Rejected: a crashed tool handler would be a crashed node, and a long-running shell command would share the node's process with every other session. The MVP's fault isolation — one process per session — is worth keeping.
- **One `bosun executor` process per session (chosen).** The node spawns it like `opencode serve` today. It serves the tool API on its own loopback port, keeps its own process table for cancelled shells, and dies with the session. `state.json` keeps its port and pid so restore-on-boot and kill-on-stop reuse the existing lifecycle.

## Decision

Each session has one `bosun executor` process on the node, started as `bosun executor --session-dir <dir> --port <n> --permission <mode>`. It listens on `127.0.0.1:<n>`. The node's relay, which previously forwarded to the opencode port, now forwards to the executor port. The permission mode is fixed per session at spawn and can be changed at runtime through `POST /permission`, so a read-only session can be promoted without recreating the working copy.

## Consequences

- Executing a tool takes one fork per command plus the shell; the executor itself is one process per session, which the node already treats as disposable.
- Shell commands inherit the session directory as their working directory.
- The executor has no provider credentials; it never talks to a provider.

## Revisit When

Sessions must be isolated in containers, or the executor must survive the node process restarting.
