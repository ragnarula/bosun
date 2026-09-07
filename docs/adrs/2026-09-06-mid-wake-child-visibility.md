# ADR: A supervising session sees its thread as it stands and waits by ending its turn

**Date:** 2026-09-06
**Author:** Raghav

## Context

`2026-09-03-agent-tree.md` made communication between parent and child authored messages, and made each wake's turns read the thread as it stood when the wake began plus the wake's own tool traffic, so an event landing mid-wake was invisible to the running wake and surfaced only in its own queued wake. It also gave a parent no way to wait for a child other than the tools it already had. Over a delegated sprint of real work this made parents react to children's reports one wake too late — messaging children that had already answered — and supervising sessions spent hours in `sleep` waiting on children while consuming model calls. Sprint 008 changes both decisions.

Sprint 008's other decision — an empty model response is retried a bounded number of times and then fails — is recorded in `../sprints/008-sessions-stop-stalling.md`.

## Decision Drivers

- A parent must see a child's report, ask, or failure as soon as it lands: within the wake that is running, not in a later one.
- Waiting for children must cost no model calls and no elapsed time.
- The five session states of `2026-08-30-session-states.md` stand unchanged; there is no sixth state.

## Options Considered

- **A full per-turn re-read of the store's active thread (chosen).** Each turn of a wake reads the session's active thread as it stands and rebuilds the live-children manifest from it; the re-read is correct by construction because `record_in_wake` writes each message to the store before the working window sees it, so a re-read returns exactly the thread the turn must see.
- **A `wait` tool.** Rejected: the loop's event channel is held by the turn's `select!` while a turn runs, so a waiting turn could not see the event that would release it — the tool would have to poll the store on a timer — and a session parked in a tool call is `running`, so its idle wait reads as work and buys model-free time only by reporting a state that does not mean what it shows.
- **A sixth session state for waiting on children.** Rejected: it would reach the store, the API, and both clients for a distinction the clients already derive from the session state and the child rows they fetch.
- **An incremental fetch of rows above the last known id.** Kept as the fallback: if the chosen re-read ever measures as a cost, fetching only the rows above the last known id replaces it.

## Decision

Each turn of a wake re-reads the session's active thread from the store and rebuilds the live-children manifest before the turn is built, so a child event or user message that lands mid-wake is visible to the next turn of that wake, and the manifest reports each child's current state and latest authored message, not the wake-start snapshot.

The wake's single boundary value splits in two. A `wake_boundary`, fixed when the wake begins, is the limit of what compaction may retire; advancing it would let compaction retire the wake's own tool traffic. A `handled_through`, advanced as each completed turn is built, decides which authored child events count as unhandled.

A plain wake whose thread holds nothing newer than `handled_through` is dropped instead of run, so the wake an event queued does not also burn a model call once the running wake surfaced that event. A user message or a parent's `message_child` always wakes; only a plain turn-shaped wake can be redundant.

Waiting for a child is ending the turn: a child's authored event wakes the parent's loop, so a parked session consumes nothing. The rule is stated in the system prompt beside the live-children manifest — a child's report or ask wakes the session, so a session waits for a child by ending its turn, and messages a child only to answer, redirect, or cancel it. No `wait` tool is added.

A session parked with live children is displayed as waiting for its children, with the count. The web pane and the terminal client derive this from the session's `waiting_for_input` state and its direct children whose state is not `stopped`; there is no sixth state and no API change.

## Consequences

- Each turn of a waking session now re-reads its thread, and the full thread for the manifest, from the store, so a wake costs more store reads — acceptable while transcripts are small — and the fallback above is the incremental fetch when that measures.
- A plain wake with nothing new is dropped, so a test or client that expects a redundant wake to run a turn gets none.
- The re-read and the advance treat an unseen mid-turn append as unhandled and keep its wake, so a duplicate wake may still run exactly when another event also landed mid-turn: a bounded extra model call, never a lost event.

## Revisit When

- The per-turn re-read of the thread and manifest measures against a window of real sessions; the incremental fetch of rows above the last known id replaces it.
- A supervising session must wait in a state the clients cannot derive, or must be interrupted while parked.

This supersedes the mid-wake-invisibility sentence and the no-way-to-wait constraint of `2026-09-03-agent-tree.md`, which carries a supersession note. The persona, ask-gating, and transport decisions of that ADR stand.