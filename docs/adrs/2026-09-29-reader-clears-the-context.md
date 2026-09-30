# ADR: The reader can clear a session's context

**Date:** 2026-09-29
**Author:** Raghav

## Context

`clear_context(instructions, reason)` is a canonical agent tool, recorded in `2026-09-27-context-clear.md`. When a session's model runs it, the control plane writes a durable marker row `Block::ContextCleared { reason, instructions }`, archives every row up to and including the marker, puts back the rows another writer appended while the turn that cleared was running, and appends the instructions as a user row the session continues from. The reader's transcript keeps everything, and both observers — the web pane and the terminal client — draw the marker as a break.

The tool belongs to the session's model. A session whose persona does not carry it cannot be cleared at all: the architect persona does not carry `clear_context`, so a session under it that has accumulated dead ends has no way to drop them. The reader watching that session cannot help, and the coarse alternatives are stop and a new session, which cost the working copy's continuity or the user's place in the work.

The reader can already act on a session from the pane's actions sheet: fork it, change its permission, switch its persona, interrupt it, stop it. A clear fits the same place, and it differs from the tool's in one way: the reader writes no instructions, so the session must end up waiting for input, and the reader's next message is what starts the new thread.

Constraints fixed going in:

- The reader's clear performs the same cut as the tool's: the same marker row, the same archive flag, the reader's transcript keeps everything.
- No instructions are invented, and no words are put in the reader's mouth.
- The agent tool path does not change: the loop still needs its own window list and its instructions row.
- The terminal client does not change: no new command, no new terminal control.
- A clear that would cut a thread a writer is still appending to is refused rather than raced.
- The pane's child view stays watch-only: a child's context is its own, but a child's pane offers no user actions.

## Decision Drivers

- The reader must be able to clear a session whose persona does not carry the tool, or the state is unreachable.
- The mechanism must be the one already recorded: the archive flag means "stored but not in the model's window", the event stream means "what the reader sees", and the marker is what explains the break.
- One store write, one transaction, one lock hold: a restart mid-clear must leave the thread as it was, not half-cut.
- The two clears must not drift. A second copy of the marker-and-cut transaction would be a second place to fix when the boundary changes.
- The pane must not become a second writer of transcript rows: the divider arrives on the session's own stream, as it does for the tool's clear.
- A refusal must reach the reader where the control is, and the refusal must not have written anything.

## Options Considered

**1. A control plane route over a store method that shares the tool's cut. (chosen)**

`POST /sessions/{id}/clear` takes the session id, checks its state, and calls `Store::clear_context_by_reader`, which runs the marker-and-cut transaction with no instructions row. The route answers `204 No Content` and sends no loop event. The agent's tool keeps calling `Store::clear_context` directly, and the two store methods share one private helper.

**2. Route the agent's tool through the same endpoint. (rejected)**

The tool's clear carries two things a REST caller does not have: the ids of the rows the turn wrote itself, which the boundary retires with the history, and the instructions row. Passing them in a body would put the loop's own store write behind an HTTP call inside the turn, and the handler refuses a running session — which is exactly the state the tool runs in. The loop's call and its result are recorded as the turn's own rows either way, so the endpoint would be a worse path to the same transaction.

**3. A CLI-only control, `bosun clear <id>`. (rejected)**

The terminal client is a client of the same routes: a command would either call the route it would be an alternative to, or carry a second copy of the state and pending-ask checks. It also adds a command the reader has to remember for an action that the pane's actions sheet already has a place for. The pane is where fork, interrupt and stop live.

**4. A loop event carrying the clear. (rejected)**

A `LoopEvent` would hand the request to the session's loop task, which is the machinery a wake already uses. It would make the clear run inside a turn: a turn exists to answer a model request, and a clear has no completion to answer, so the loop would run a turn with nothing to send and then have to append the turn's own rows behind the boundary. The reader's clear is one store write with no model call and no turn, and the next turn reads the fresh window anyway.

**5. Reuse the tool by synthesizing instructions. (rejected)**

A reader's clear has no instructions. A placeholder such as `continue` would put words in the reader's mouth, and the session's next request would answer a task nobody wrote, with the reader's real message behind it. The marker records the empty string, which states that there are none, and the next window starts from the reader's own message.

**6. A root-only control, refused for a child like the sheet's other actions. (rejected)**

The other user actions change something outside the session — a fork creates a root, a permission change changes what the session may do — and the pane hides them on a child because only the tree owner accepts them. A clear changes the session's own context and nothing else, and a child's dead ends are the child's problem; the refusal would make the state unreachable again for a child whose persona lacks the tool. The pane still hides the row on a watch-only child, because the pane's child view offers no user actions at all.

## Decision

`POST /sessions/{id}/clear` is a session route in `crates/bosun-control/src/api.rs`, beside `/sessions/{id}/fork`. The handler `clear` reads the session row and refuses before it writes:

- `404 Not Found` when no session holds the id, as `ApiError::SessionNotFound`.
- `409 Conflict` with `the session is running; interrupt it first` when the session is `running`. A running session's loop holds the window its turn reads. That state is the mark the wake writes for itself — `crates/bosun-agent/src/agent_loop.rs` sets `SessionState::Running` as the wake begins, inside the wake — so the check reads the loop's own mark rather than the moment a message arms one, which is the closest the route can come to the turn's own edge.
- `409 Conflict` with `the session is still being created; try again when it is waiting for input` when the session is `creating`: there is no thread to read yet.
- `409 Conflict` with `a question is pending; answer it, or reject it, before clearing the context` when `Store::get_pending_ask` answers `Some`. This is the tool's own refusal, for the same reason: the question lives in the rows the cut archives while the `pending_asks` binding survives, so the question would leave the model's window and the next `ask` would still be refused.

Otherwise the handler calls `Store::clear_context_by_reader(&id, READER_CLEAR_REASON)`, logs `session_id` at `info`, and answers `204 No Content`. `READER_CLEAR_REASON` is `the reader started fresh`: the tool takes its reason from the model, and the reader's clear has none to take, so the pane's control records one fixed reason that says what happened.

The route is not root-only: any session may be cleared, a child's context is its own. It sends no `LoopEvent` and wakes nothing — the session keeps its state, and its next turn reads the fresh window.

`Store::clear_context_by_reader(session_id, reason) -> Result<i64, StoreError>` is in `crates/bosun-store/src/store.rs`, beside `clear_context`. In one transaction and one lock hold it inserts the marker `Block::ContextCleared { reason, instructions: "" }`, archives every row up to and including the marker, writes no row after it, and commits. It returns the marker's id. Every active row is retired and none is restored: this clear runs outside a turn, so there is no row the session has not read, and the reader's next message is a new row above the boundary, active like any other.

Both clears share one cut: `cut_context_tx(tx, session_id, own, reason, instructions)` writes the marker and archives every row up to and including it. `own: Some(ids)` is the agent's clear, and every other active row goes back into the window; `own: None` is the reader's, and nothing is restored. `instructions` is what the marker records — the prompt for the agent's clear, the empty string for the reader's. Each public method then adds what it owns inside that same transaction: `clear_context` inserts the instructions row and returns the marker's id and that row's id, and `clear_context_by_reader` inserts nothing and returns the marker's id. The tool's behaviour, its return values and the marker's shape are unchanged.

The marker lands as a durable `message` event because `insert_message` also appends one, exactly as the tool's marker does. That event is what draws the divider in both clients: the pane and the terminal client render `context cleared: <reason> · continuing from a fresh prompt` from the block, and the marker's own row is the only record of the break. The pane appends no transcript row itself.

The pane carries the control in its actions sheet. The row `row-clear`, the button `btn-clear` and the note `view-clear` are marked up in `crates/bosun-control/src/ui/index.html`, looked up with every other element in `ui/dom.js`, and wired in `ui/session-view.js`, the module that owns the session view and its sheet. The button asks first — `Clear this session's context? The transcript keeps everything; the model starts fresh.` — then writes `clearing…` into the note, captures the open session, disables the button, and posts to `/sessions/{id}/clear`. On success, and only while the pane still shows that session, it clears the note, closes the sheet and toasts `context cleared`. On failure, and only while the pane still shows that session, it writes `clear: <message>` into the note: a refusal is reported beside the control, and the fork's control works the same way, writing `forking…` and then `fork: <message>`. A request re-enables its control only on the screen it was made for; the teardown sets the next screen's state. The row is hidden for a watch-only child (`rowClear.hidden = watchOnly`), like every other user action, and the session view's teardown shows the row again, re-enables the control and the fork's beside it, and clears the note, so a request the reader left behind cannot hold the next session's control or leave its note there.

The terminal client is unchanged: no new command, and no control for a clear. The agent's `clear_context` tool keeps its own path — the loop calls the store directly, because it needs its own window list and its instructions row — and the tool surface, the loop and `Block::ContextCleared` are unchanged.

## Consequences

- A session whose persona does not carry `clear_context` can still be cleared: the reader does it from the pane's actions sheet, and the session waits for the reader's next message, which is the row its next window starts from.
- The reason on a reader's clear is one fixed string. A reader cannot say why they cleared, and the marker's divider reads `context cleared: the reader started fresh · continuing from a fresh prompt` for every such clear.
- One residue is not closed by the state checks. A message appended in the instant before the clear's write commits is retired with the history: the route reads the mark the wake writes for itself, and that write happens inside the wake, so a message that arms a wake after the check and before the cut lands under the boundary. The armed wake is not dropped — the loop drops a redundant turn only when the active thread's newest row is at or below the row it has already handled, and after the cut that thread is empty — so it runs a turn whose window holds nothing. The message it was armed for is archived, and the request carries the system prompt and the session context with no conversation at all; what comes back answers nothing the reader sent. The reader can send the message again. The window is one store write wide, and closing it would mean writing from the route inside the loop's own state transition, which is what option 2 was rejected for.
- The caller of the route learns nothing about what was cut: the response carries no count and no marker id. The transcript shows the break, and that is the record.
- Two paths now write a clear, and they share one cut: `cut_context_tx` writes the marker and the archive boundary, and each public method adds what it owns inside its own transaction — `clear_context` the instructions row, `clear_context_by_reader` nothing. The helper carries no branch for a case neither caller has.
- A reader's clear of a session whose model is mid-turn is refused rather than queued: the reader interrupts first, then clears. Two presses for a case the tool handles in one call inside the turn.
- The pane's clear is a user action like the others, so a child's context cannot be cleared from the pane: the row is hidden on a watch-only child. A child clears its own context with the tool when its persona carries `clear_context`; a child under a persona that does not carry it has no way to clear at all, which is the state this decision removes for a root and leaves for a child.
- The control is spread over three pane files — the row in the page, the lookups in `dom.js`, the wiring in `session-view.js` — because the pane ships as embedded modules with no build step. A rename touches each, and the pane's source checks in `crates/bosun-control/src/ui.rs` read them as one text so a checked token still has to be there. What the row does when it is pressed is checked where it can be seen: `crates/bosun-control/tests/browser.rs` and its `tests/browser/pane.py` drive the real pane in a phone-sized Chromium, and cover the row showing for a root session, a 204 closing the sheet and toasting with no row drawn by the pane, a 409 writing the refusal into the note, the row hidden for a watch-only child, a request held open with the control off and the note naming what it waits for, and the session change that gives the control and the note back.

## Revisit When

- A clear must say something of the reader's: a note recorded with the marker, or instructions typed into the actions sheet.
- A running session should be cleared without interrupting it, which needs a rule for the turn's own rows and the boundary it holds.
- A message that arrives just before a clear is lost with the history often enough to matter: that needs the wake's own state transition and the cut to be one write.
- The transcript needs to distinguish a reader's clear from the session's own, beyond the reason string.
- A clear should also reset what it leaves behind — the todo list, the summary, or the cost view — rather than only the thread.
