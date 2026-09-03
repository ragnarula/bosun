# ADR: Session states

**Date:** 2026-08-30
**Author:** Raghav

> Relaxed by sprint 005 (S8, `../sprints/005-agent-tree.md`) for the crash cause: a crash-interrupted session may run a re-decision turn after boot recovery, woken by its children's failure reports, where this ADR says no turn starts until the user sends a message. User-interrupted sessions still hold until the user acts. The final ADR for sprint 005 records this.

## Context

The MVP reported a session as either `running` or stopped, where "running" meant the opencode server answered. Sprint 002's session has an agent loop, an executor, and a user who must answer questions. "Running" is no longer one thing, and the client and the web pane must show a state the user can act on.

## Decision Drivers

- A session with no prompt must not pretend to be working; a session waiting for an answer must say so.
- A crash must be visible as a state, and resuming must be an explicit user action so a turn is never replayed by accident.
- Every state must be reachable through the store and the API, so the client and the loop agree without extra bookkeeping.

## Options Considered

- **Two states plus an idle flag.** Rejected: "idle" was an attribute of `running`, so the client could not distinguish "the model is thinking" from "the model is waiting for me" without extra queries.
- **Five explicit states (chosen).** `creating`, `running`, `waiting_for_input`, `interrupted`, `stopped`. Each is a value in the store, emitted as an event.

## Decision

A session is always in exactly one of five states, stored in `sessions.state` and emitted on the event stream:

- `creating` — the working copy is being prepared; the loop is about to start.
- `running` — a turn is in flight: the model is streaming or a tool is executing.
- `waiting_for_input` — the turn ended and the loop awaits a user message or an answer to an `ask`.
- `interrupted` — the last turn was killed by an interrupt or a crash; nothing is in flight and no turn will start until the user sends a message.
- `stopped` — the session is being torn down and will be removed.

A session is born `creating` with an optional prompt. With one, the loop starts the first turn at once; without one, it moves straight to `waiting_for_input`. On boot the control plane marks every `running` or `creating` session `interrupted`; the loop rehydrates from the store and waits for the user.

## Consequences

- `bosun list` shows the five states truthfully; the terminal client and web pane act on them.
- An interrupted turn is never replayed: resuming always starts from a user message.
- The state machine has five transitions and a crash rule, all unit-testable in the store.

## Revisit When

A session needs to report progress within a turn (for example `thinking` or `compacting`), or work must continue without the user while interrupted.
