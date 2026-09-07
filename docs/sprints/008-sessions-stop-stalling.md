# Sprint 008 — Sessions stop stalling

A session stops only when it has finished or failed. An empty model response becomes a fault that is retried and then fails, instead of a finished turn that parks the session. Each turn of a wake reads the thread as it stands, so a parent sees a child's report as soon as it lands and never polls a child that has already answered. A session waits for its children by ending its turn, which costs no model calls and no elapsed time.

Status: **complete**. All five stories are implemented and tested.

> This sprint changes two decisions recorded in `../adrs/2026-09-03-agent-tree.md`: that an event landing mid-wake is invisible to the running wake, and that a parent has no way to wait other than the tools it already has. S5 records the change in a new ADR and adds a supersession note to the agent-tree ADR. The persona, ask-gating, and transport decisions there stand.

## Confirmed decisions

- **A turn that produces no text and no tool calls has produced nothing.** The test is on content, not on token counts: a reply that spends its whole output budget on reasoning is as empty as one that returns nothing. Such a turn is retried up to three times with a short backoff and then fails. It never counts as a finished turn.
- **A failed turn keeps the meaning it already has.** The session becomes `interrupted` with cause `Crash`; a child authors a *failure* event to its parent instead of a blank report, and the parent re-decides it. No new session state and no new machinery.
- **A retry re-sends an identical request.** An empty turn writes no text, appends no tool call, and leaves compaction below its threshold, so nothing in the thread changes between attempts. The extra `model_calls` row is correct: two calls were made.
- **Each turn of a wake refreshes the thread and the live-children manifest.** A re-read of the store's active thread is correct by construction, because `record_in_wake` writes to the store before the window and a re-read after compaction returns the compacted thread.
- **The wake's single boundary value splits in two.** A `wake_boundary`, fixed when the wake begins, stays the limit of what compaction may retire. A `handled_through`, advanced as each turn is built, decides which authored child events count as unhandled. One value cannot serve both: advancing the compaction limit would let compaction retire the wake's own tool traffic.
- **A queued wake with nothing new is dropped.** Surfacing an event inside a running wake would otherwise still burn a model call on the wake that event queued.
- **Waiting is ending the turn.** The mechanism exists and works: a parked session is woken by its child's authored event and consumes nothing meanwhile. No `wait` tool is added; the loop states the rule in the system prompt beside the live-children manifest.
- **A session parked with live children is displayed as waiting for its children.** The clients derive this from the session state and the child rows they already fetch, so no sixth session state is added.

## CLI surface

Unchanged. No command, flag, or output format changes.

## User stories in implementation order

- [x] **S1 — An empty model response is a failed turn**

As a user, I want a session to stop only when it has finished or failed, so it never parks mid-task with no explanation.

- `run_turn_inner` returns a new `TurnOutcome::Empty` when the stream yielded no text and no tool calls.
- `handle_wake` retries an `Empty` turn up to three times with a short backoff, counting consecutive empty turns in the wake loop and resetting the count on any other outcome. The limit is fixed, so the cost of a failing provider is bounded.
- Exhausted retries fall into the existing `TurnOutcome::Failed` arm, which already interrupts the session as a crash and makes a child author a failure event to its parent.
- Each empty response logs at `warn` with the session id, provider, model, and attempt number; giving up logs at `error`. Nothing recorded these before, so the fault could only be found by reading zero-token `model_calls` rows.
- Tests: an empty response followed by a normal one succeeds and records only the real reply; three empty responses on a root end the session `interrupted` with cause `Crash`; three on a child author a failure event rather than an empty report; empty responses interleaved with productive turns reset the count; an interrupt during a retry ends the turn interrupted; a reply with text and no tool calls still finishes.

- [x] **S2 — The adapters read the provider's stop reason**

As a developer, I want a truncated reply to be distinguishable from a clean finish, so the loop treats it as the fault it is.

- `openai.rs` reads `finish_reason` and `anthropic.rs` reads `stop_reason`; both carry it on `StreamEvent::Stop`.
- A reply cut off by the output budget that produced no text and no tool calls takes S1's retry path and logs as a truncation rather than as an empty response; a truncated reply that produced text still finishes, because the fault test is on content.
- `parse_event` in `openai.rs` no longer returns as soon as it finds `delta.content`, so a chunk carrying both text and a tool call no longer drops the tool call.
- Tests: per-adapter unit tests over recorded chunk sequences for a clean stop, a length stop, and a chunk carrying both text and a tool call.

- [x] **S3 — A wake sees its thread as it stands**

As a user, I want a parent to notice a child's report immediately, so it never polls a child that has already answered.

- `handle_wake` refreshes the window from the store's active thread and rebuilds the live-children manifest before each turn, instead of once when the wake begins.
- `LoopState.surfaced_through` splits into a local `wake_boundary` passed to `maybe_compact` and a `LoopState.handled_through` advanced per turn and passed to `live_children`.
- A plain `WakeKind::Turn` whose thread holds nothing newer than `handled_through` is dropped, beside the existing check that blocks wakes on stopped and user-interrupted sessions.
- Tests: an event landing mid-wake is visible to the next turn of that wake; the manifest reports each child's current state and latest authored message rather than the wake-start snapshot; the wake that event queued is dropped instead of run; compaction still refuses to retire rows above the wake boundary; an event landing between wakes still surfaces exactly once.

- [x] **S4 — Waiting for a child costs nothing**

As a user, I want a supervising session to wait without spending model calls or elapsed time, so a sprint of delegated work does not sleep through most of its duration.

- The live-children block in `system_prompt` states the rule: a child's report wakes the session, so a session waits for a child by ending its turn, and it messages a child only to answer, redirect, or cancel it.
- The web pane and the terminal client render a session that is `waiting_for_input` with live children as waiting for its children, with the count. Both already fetch the session and its children, so no API change is needed.
- Tests: the prompt carries the rule whenever the manifest is non-empty; a parked session with live children renders as waiting for children.

- [x] **S5 — The decision on record**

As a developer, I want the changed decisions written down, so a later reader knows what was ruled out and why.

- `../adrs/2026-09-06-mid-wake-child-visibility.md` records S3 and S4 as one decision about how a supervising session observes and waits for its children.
- Its Options Considered carries the rejected alternatives with the reason each lost: a `wait` tool, which would have to poll the store on a timer because the loop's event channel is held by the turn's `select!`, and which would hold a session `running` while it idles; a sixth session state for waiting on children, which would reach the store, the API, and both clients for a distinction the clients can derive; and an incremental fetch of rows above the last known id, which is the mitigation if the per-turn re-read ever measures.
- `../adrs/2026-09-03-agent-tree.md` gains a supersession note on its mid-wake-visibility and waiting paragraphs.

## Acceptance measures

Measured over a delegated sprint of comparable size, against the run that prompted this work.

| Measure | Before | Target |
|---|---|---|
| Empty model responses that ended a session | 11 of 11 | 0 |
| `message_child` calls sent after that child had already reported | 31 of 33 | 0 |
| Elapsed time spent in `sleep` | 2h 53m | none |
| Reports authored by one child for one unit of work | up to 8 | 1 |

## Out of scope

- **Child events while a session is user-interrupted.** A plain wake is discarded rather than queued, so a tree can deadlock if the user interrupts and does not return. `../adrs/2026-09-03-agent-tree.md` decides this deliberately — user-interrupted sessions hold until the user acts — and changing it is a separate decision about whether a child's report counts as the user acting.
- No change to personas, ask gating, the tool surface, the store schema, the tunnel, or the executor.
- No new session state, and no change to the five states of `../adrs/2026-08-30-session-states.md`.
