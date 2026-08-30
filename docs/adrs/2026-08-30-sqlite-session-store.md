# ADR: SQLite session store on the control plane

**Date:** 2026-08-30
**Author:** Raghav

## Context

The MVP made nodes the source of truth for sessions: the control plane rebuilt an in-memory view from heartbeats (see `2026-08-18-nodes-source-of-truth-for-sessions.md`). Sprint 002 moves the agent loop onto the control plane, so the loop's transcript and its progress must survive control-plane restarts. The control plane now needs durable state of its own.

## Decision Drivers

- The loop must rehydrate its transcript after a control-plane crash and never lose a completed message.
- The session API and the SSE stream must answer from one store, so the state shown is the state that is.
- A single-user control plane must not require operating a database server.

## Options Considered

- **Keep nodes as source of truth; add an append log for the transcript.** Rejected: two stores for one session, and the session list still cannot be answered while a node is down.
- **SQLite, bundled, on the control plane (chosen).** One file in the control-plane data directory, WAL mode, compiled from source so there is nothing to install. Synchronous access is cheap and fits a single-user control plane.

## Decision

The control plane persists sessions in one SQLite database. Tables: `sessions`, `messages`, `tool_calls`, `model_calls`, and `events`. `sessions` is the source of truth for sessions; the node registry keeps only node liveness. `events` carries every transcript delta and state change with a monotonically increasing sequence, which is what the SSE stream replays. `messages` holds the full transcript for the loop's context window and for the archive after compaction. `tool_calls` and `model_calls` hold the per-session records the metering view reads.

## Consequences

- A control-plane restart repopulates sessions from SQLite and marks interrupted ones `interrupted` (see `2026-08-30-session-states.md`).
- The node registry shrinks to liveness only: what the node reports about its sessions no longer drives the session list.
- The bundled SQLite build needs a C compiler at build time.

## Revisit When

More than one control-plane process must serve the same sessions, or the database must be reachable from more than one host.
