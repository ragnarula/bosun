# ADR: A session may clear its own context and continue from a fresh prompt

**Date:** 2026-09-27
**Author:** Raghav

## Context

A session's model reads a window: the active messages of its store thread, oldest first, plus the fixed session context and manifest. `crates/bosun-agent/src/agent_loop.rs` reads that window at the start of every turn, and `maybe_compact` retires the oldest half of it when a completion reports a full context, replacing the retired tail with a `Block::Summary` message and archiving the rows it retired. The reader — the terminal client and the web pane — sees a different thing: the durable event stream, where every message row also lands as an event, and archived rows stay in it.

A long session accumulates dead ends: explorations that led nowhere, listings that no longer matter, an abandoned approach. Nothing let the session itself get a clean head. The user could stop a session or start another, and both cost the working copy's continuity or the user's place in the work.

Constraints fixed going in:

- The reader's transcript keeps everything. A clear is not a deletion.
- The model's next request must start from a prompt the session supplies, with no part of the discarded thread in it.
- Work that arrives while the session is running is not lost: a queued user message, a child's event, or a parent's message must still reach the model.
- Compaction and the session summary keep working across the break.
- The tool requires a reason, so a clear says what it is for.
- A read-only session keeps the tool: it changes the session's own context and nothing outside it.
- Nothing outside the session changes: no file, no machine, no other session, no user-facing control.

## Decision Drivers

- The agent's own dead ends are the problem, so the agent must be able to act on them; the user cannot see them.
- The reader must be able to tell where the break was and why, or the transcript reads as if the session lost its mind.
- The window must be exactly the fresh prompt plus what arrives after it, or the clear has not cleared anything.
- Nothing the session has not read may be discarded with the history: a clear is the session's act, not the user's message's.
- Existing machinery must carry the load: the archive flag already means "stored but not in the window", and the event stream already means "what the reader sees".

## Options Considered

**1. A durable marker row is the boundary, and the fresh instructions are appended after it. (chosen)**

The tool writes a `Block::ContextCleared { reason, instructions }` row, archives every row up to and including it, and appends the instructions as a user row. The model's next window holds the instructions, then whatever arrives after them; the reader's transcript holds the whole history and the marker; the rows stay stored.

**2. Delete the discarded rows. (rejected)**

Simplest to reason about, and it is what "clear" sounds like. Rejected because the reader's transcript is the durable record of what happened: a session that deleted its own history would leave a hole the user cannot see into, and the pane and the client both replay stored rows. The archive flag already expresses "out of the model's window, still stored", which is exactly the wanted effect.

**3. A hidden clear: no marker row. (rejected)**

A clear that leaves no trace reads to the user as the session suddenly forgetting its task, with nothing to explain it. The marker also carries the reason, which is the tool's only guard.

**4. A user-only reset. (rejected)**

The user cannot see that a session is stuck in its own dead ends, and asking them to decide costs a round trip through the model. The issue asks for the agent-level equivalent of a reset; the user already has stop and a new session, which are the coarse versions of this.

**5. The fresh instructions only inside the marker. (rejected)**

If the archived marker were the only record, the model's window would start with no task at all: it must begin from a row it can answer as its user turn, which is what a fresh user message is.

**6. Clearing by starting a fresh root session, or a fresh child. (rejected)**

A new root loses the user's place in the conversation and the tree's continuity; a child does not take over its parent's work. Both also cost a new executor and a new working copy or a clone.

**7. A cap on clears, or a user approval before one. (rejected)**

The system is single-user with no approval channel, and the tree already has no creation-time caps. The required reason is the guard that fits: it makes the clear explainable in the transcript, and a model that writes a plausible reason to dodge work is not something a cap would catch either.

## Decision

`clear_context(instructions, reason)` is a canonical tool: both fields are required strings, a blank one is a tool error, and a read-only session is offered it, since it changes the session's own context and nothing outside it. It is advertised at every depth, so a child clears its own context like a root.

When the loop runs it, in this order:

1. The call and its result are recorded as any tool's are, so the transcript shows the request and the answer.
2. The marker row `Block::ContextCleared { reason, instructions }` is appended. It is durable and visible: every message row also lands as an event, so both readers draw it as a break — a divider in the accent colour, worded `context cleared: <reason> · fresh instructions: <instructions>`.
3. `mark_archived(session_id, marker_id)` is called: every row up to and including the marker leaves the model's window. That covers the discarded history, the clear's own call, its result and the marker itself, so the next window holds no tool result without its call — a request a provider refuses.
4. The instructions are appended as a user row: the first thing the model reads in its next request.
5. Rows another writer appended during the turn — a user message, a child's event — are put back in the window with `unarchive_message`, one row at a time. They were never in the turn's window, so the session has not read them; the boundary must not swallow them.

The turn ends normally after the tool result, so the clear takes effect for the next request: the loop's next turn reads the window fresh and starts from the instructions. A message queued meanwhile is appended to the thread and its wake runs a turn that holds it.

A pending ask cannot coexist with a clear. A completion that calls `ask` ends the turn there, so a later call in the same completion never runs, and a session waiting on an ask has no turn to clear in.

Compaction respects the boundary without knowing about it. A wake fixes its compaction boundary at the wake's start, and after a clear every row of the window lies above that boundary, so the wake that cleared retires nothing; later wakes fix their boundary on the fresh window and compact it as usual. `mark_archived` from either path is the same flag, and the wake's `handled_through` advance steps over its own rows that are no longer active instead of stalling on them.

The session summary reads the active thread, so after a clear it describes the work the session picked up after the boundary rather than the thread it discarded.

## Consequences

- A session can drop a dead-end thread and continue, in one call, with the reason and the fresh instructions recorded for the reader. Nothing outside the session's own context changes.
- The discarded thread is out of the model's reach for the rest of the session. A session that clears cannot answer a later question about what it did before the break, and the instructions it writes are what it knows — so a clear is a real loss of information, not a view change.
- The reason is a soft guard. The tool cannot tell a good reason from a plausible one, so a session can still clear to escape a hard problem; what the guard buys is that the escape is written down and reviewable.
- A clear resets the thread only. The todo list, the cost meter, the working copy and the model's provider cache are untouched, and a session that clears still holds whatever it had in those.
- Archived rows stay in the database: a cleared session keeps its whole history on disk, and nothing prunes it.
- Two paths now archive rows the model was reading — compaction and a clear — so the wake bookkeeping that walks a turn's own appends must tolerate its own rows being gone. That is one condition in the advance, but it is load-bearing: without it a clear would stall the boundary and make handled child events look unhandled.
- Every reader needs a case for the new block: the pane and the terminal client each render it, and a client that does not falls back to its unknown-block shape. Dropping the row shape later, or renaming the variant, breaks stored transcripts, which pre-1.0 dev data accepts as it has for every block shape change.
- A clear is invisible to the tree: a child's clear touches neither its parent's thread nor its siblings', and the parent's manifest does not change.

## Revisit When

- A user must see, undo, or review a clear as an action of its own: a pane control, or an event the session list highlights.
- A clear should also reset what it leaves behind — the todo list, the summary, or the cost view — rather than only the thread.
- Clears become frequent enough to need a budget, or a clear's marker needs to name the range of rows it discarded so a reader can jump to the thread it closed.
- Archived rows need pruning, which would make a cleared session's history finite and the reader's transcript no longer complete.
- The window must start from something richer than one user row: a handover summary, or the last child report, written in front of the instructions.
