# Sprint 011 — Sessions describe themselves

The session list says where each session works and when it started, and nothing about what it is for. A session's `prompt` holds the request that created it and is shown nowhere, so a row on a long-running session cannot be told from another's. This sprint has a session's own model write a one-line description of it — what the session is for and what it is doing now — and shows that line as the session row's headline. The description is a field of the session, written on the session's own provider budget, and never a transcript entry.

Status: **complete**. All five stories are implemented and tested.

> The trigger, the storage rule and the cost bound are recorded in `../adrs/2026-09-13-session-summaries.md`.

## Confirmed decisions

- **The description is a session field, never a transcript message.** `Session.summary` travels in the session list and session detail responses; neither the request nor the answer is appended to the thread, so the transcript the user reads and the model's context are unchanged.
- **The loop writes it**, not a client and not a timer: the task that runs the turns is the only component that knows a session's work has stopped, and it already holds the session's provider, model and prices.
- **A wake must have ended and the session must then sit idle.** The loop waits `SUMMARY_IDLE_BEFORE` (15 seconds) after a wake ends; an event inside that window starts the next wake instead, so a session still working is never described mid-task.
- **The cost is bounded twice.** The newest message must be newer than the one the last summary covered, the last summary must be at least `SUMMARY_MIN_INTERVAL` (5 minutes) old, and the request carries at most `SUMMARY_INPUT_BYTES` (4,000 bytes) of transcript, read from its newest end.
- **One way to make a request of the loop's own.** `ask_out_of_band` holds the request, the stream collection and the interrupt check; the compaction summariser and the session summary both go through it. The summary call is metered as a `ModelCall` of kind `summary`, so it appears in the clients' per-call context lines as a compaction call already does. That line is bookkeeping about a call: no message is added, and the model's thread does not contain it.
- **No event, no endpoint, no new connection.** The pane fetches the session list every 3 seconds, so the summary needs no frame of its own: `set_summary` writes the session row alone, and no event carries the summary text.
- **The pane leads the row with it.** A summarized session shows the summary in the headline slot and drops the node and directory to the meta line; a session with no summary keeps its present row.
- **`bosun list` is unchanged.** Its table has fixed columns and no room for a variable line.

## CLI surface

Unchanged. No command, flag, or output changes.

## User stories in implementation order

- [x] **S1 — A session carries its summary**

As a developer, I want the summary stored on the session row, so a client reads it from the list it already fetches.

- `Session.summary: Option<String>`, `#[serde(default)]`, so an older payload parses.
- The `sessions` table gains a `summary` column, and `Store::open` adds it to a database written before it existed.
- `set_summary(id, summary)` updates the row and appends no event.
- Tests: an insert with a summary reads it back; a summary survives a serde round trip and a payload without the field parses as `None`; a pre-summary database is migrated and takes a write; `set_summary` leaves the transcript and the event stream untouched.

- [x] **S2 — The loop describes an idle session**

As a user, I want each session's row to say what it is for and what it is doing, so I can tell my sessions apart without opening them.

- `SUMMARY_PROMPT` states the answer the loop asks for: one line, at most 12 words, no preamble.
- `summary_prompt` composes the request from the instruction, the request that created the session, and the newest transcript messages up to `SUMMARY_INPUT_BYTES`.
- After a wake ends the loop waits `SUMMARY_IDLE_BEFORE` for the next event and calls `refresh_summary` if none arrives; the event arms of the wait start the next wake, and `wake_of` holds the one mapping from a loop event to a wake so the wait and the idle path cannot drift.
- `refresh_summary` sends the request through `ask_out_of_band`, stores the trimmed answer cut at `SUMMARY_MAX_BYTES` with `set_summary`, and meters the call as kind `summary`.
- Tests: a wake whose session then goes idle writes the summary and one `summary` model call; the transcript holds only the turn's own messages; the summary request offers no tools and carries no system prompt, and carries the transcript.

- [x] **S3 — The cost is bounded**

As an operator, I want a session left running for days to cost a bounded number of calls for its own name.

- A refresh is skipped when the newest message is not newer than the one the last summary covered, and when the last summary is younger than `SUMMARY_MIN_INTERVAL`.
- Everything the request adds after the instruction — the request that created the session, the two headings, and the transcript lines — is counted against `SUMMARY_INPUT_BYTES`; the transcript is read newest first, and thinking is left out, as it is for compaction.
- `LoopState` holds the last-summary time and the message id it covered, documented as per-process, so a control-plane restart writes one summary at its next idle.
- An answer the provider leaves empty is metered and records the attempt like any other, so it cannot buy a call at every idle; a row with a name keeps the name it has.
- A store error on this path is logged and the loop carries on: a name costs at most a log line, never a session.
- The stored name is cut at `SUMMARY_MAX_BYTES` (200 bytes), at a character boundary, so a provider that answers at length cannot grow the session row that every session-list response carries. The request asks for at most 12 words, which is well inside the cap.
- Tests: a second wake inside the interval writes no second summary although the thread moved on; a refresh whose thread holds nothing new makes no call; an empty answer is metered and holds the interval; an answer far longer than the cap is stored cut at the cap without splitting a character; a long transcript and a session created from a request far larger than the cap each produce a request inside it, keeping the newest messages and dropping thinking.

- [x] **S4 — The pane shows the summary in the row**

As a user, I want the session list to lead each row with the session's description, so the list reads as what my sessions are for.

- `appendSessionRow` appends the summary first when the session has one, and the node/dir pair moves to the meta line under it; the headline rule is shared with `.row-node` and made a block box, so a line wider than the row ends in an ellipsis.
- No event handling, no new fetch: the pane's existing session poll carries the field.
- Tests: a source-presence check that the row leads with `session.summary` and demotes the node/dir line when it is set; the pane's existing checks stand.

- [x] **S5 — The docs that own the current state say so**

As a developer, I want the shipped feature stated where the project keeps its current state, so a reader meets it from the entry point.

- `CLAUDE.md`'s current-state line names session summaries and points at this sprint.
- The README's status paragraph names them too.
- `../adrs/2026-09-13-session-summaries.md` records the trigger, the storage rule, the cost bound and the option that lost.

## Acceptance measures

Measured on one control plane with several sessions left running.

| Measure | Before | Target |
|---|---|---|
| A session's purpose in the list | node/dir, persona, id, age | the model's one-line description leads the row |
| Model calls a session spends on its own name | none | at most one per 5 minutes, only after 15 seconds of quiet |
| Transcript entries a summary adds | — | none |
| New endpoints, tables, events, connections | — | none |
| Requests one summary call carries | — | the instruction, then at most 4,000 bytes of creating request, headings and transcript |
| A stored name's size | — | at most 200 bytes, of which a phone's row shows about eight words |

## Out of scope

- No editable name: nothing in the API or the pane renames a session.
- No summary in the open session's header or on the session screen: the list row is the one place it shows.
- No summary on a child's line in the list: a child row is one banner line under its parent's, and the summary would push the child's state and id off it.
- No summary for the terminal client's `bosun list` table.
- No history of names: the session row holds the newest one, and no event records the ones before it.
- No change to what the model's thread contains: the summary is a session field, never a message.
- No change to compaction, beyond sharing the one request helper it now has.
