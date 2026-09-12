# ADR: The turn model lives in the harness contract and the waiting rule is removed

**Date:** 2026-09-12
**Author:** Raghav

## Context

Sprint 008 added a rule beside the live-children manifest: "Each child's report or ask wakes this session; to wait for a child, end your turn. Message a child only to answer, redirect, or cancel it." It was stated only when a session had live children. It told a session with live children to end its turn, which parked sessions that still had work of their own.

## Decision Drivers

- A session should keep working until its task is complete, and end its turn only when nothing is left or the user is needed.
- Waiting is already safe because a child event or user message wakes the loop, so it does not need a per-turn, children-specific rule.
- The turn model belongs in one place, the fixed contract.

## Options Considered

- **Keep `WAITING_RULE`.** Rejected: it is conditional on children existing and reads as "if children are running, stop".
- **Keep a shortened rule without the parking clause.** Rejected: it is still a per-turn repetition and still children-specific.
- **Add a `wait` tool.** Already rejected in `2026-09-06-mid-wake-child-visibility.md`: the event channel is held by the turn's `select!`, so the tool would poll, and a parked tool call reads as `running`.
- **State the general turn model in the fixed contract (chosen).**

## Decision

`WAITING_RULE` is deleted and the live-children manifest is data only. The contract states: keep working until the task is complete; end the turn only when nothing is left or the user is needed; a user message or a child event wakes the session; a child reports by ending its turn; use `message_child` only to answer, redirect, or cancel a child.

## Consequences

- A supervising session is less likely to park while it still has work.
- The contract now carries tree semantics for every session, including sessions that never spawn, at a small token cost.
- This is a behaviour change from sprint 008.

## Revisit When

- Supervising sessions still park with work remaining.
- The contract's turn section grows past a short list.
- The waiting semantics change.
