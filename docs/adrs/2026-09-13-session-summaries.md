# ADR: The loop describes each session with a model-written summary

**Date:** 2026-09-13
**Author:** Raghav

## Context

The session list shows each session's node, directory, persona, short id and age. A user running several sessions cannot tell what one is for, or what it is doing now, without opening it. Nothing names a session: `prompt` holds the request that created it, and no title, rename path or derived name exists in the store, the API, the CLI or the web pane.

Sessions run a per-session loop on the control plane, and the loop owns the provider call for their turns. It already makes one request of its own — the compaction summariser, `summarize_tail` in `crates/bosun-agent/src/agent_loop.rs` — whose request and answer never enter the transcript.

## Decision Drivers

- The description is written without the user asking, and stays current as the session's work moves.
- It must not enter the transcript or the model's thread. The only trace it leaves in a session's own record is the metered call, as a compaction already leaves one.
- Its cost must be bounded: a session left running for days must not spend an unbounded share of its budget on its own name.
- No new endpoint, no new table, and no second client connection: the pane already fetches the session list on a timer.

## Options Considered

- **A name derived from the first user message.** Rejected: the first message is often one line of a larger task, and it never changes, so a long session's row still says nothing about what the session is doing now.
- **A name the model writes inside its answer**, which the harness strips before recording. Rejected: it puts naming on the turn's output path, where a missing or malformed name becomes a turn failure, and it changes the transcript the user reads.
- **A durable `Event::Summary` and a new case in both clients.** Rejected: the pane polls the session list every 3 seconds, so a live frame buys nothing, and an event per refresh stores a row for every name the session ever had.
- **A background task per session on a wall-clock timer.** Rejected: the loop is event-driven, and a per-session timer adds a second writer of the session row and a second source of truth for when a session is quiet.
- **The loop describes the session at the end of a wake, from the row it already owns (chosen).** The loop is the only component that knows a session's work has stopped, and it already holds the session's provider, model and prices.

## Decision

`Session` gains `summary`, an optional field stored in the `summary` column of the `sessions` table and carried to clients in the existing session-list and session-detail responses. The store writes it through `set_summary`, which updates the row and appends no event. A row written before the column existed has no summary, and `Store::open` adds the column to an existing database.

The loop writes it. After a wake ends, the loop waits `SUMMARY_IDLE_BEFORE` (15 seconds) for the next event instead of blocking on the channel forever; if none arrives, it calls `refresh_summary`. An event that arrives inside that window starts the next wake, so a session that is still working is never described mid-task.

`refresh_summary` skips its call when the newest message is not newer than the one the last summary covered, and when the last summary is younger than `SUMMARY_MIN_INTERVAL` (5 minutes). The time of the last summary and the message id it covered live in `LoopState`, so a control-plane restart writes one summary at its next idle. An answer the provider leaves empty is metered and records the attempt like any other, so it cannot buy a call at every idle, and the row keeps the name it has.

The call is a request the loop makes for itself, sent through `ask_out_of_band` — the helper the compaction summariser now shares — with the instruction `SUMMARY_PROMPT`, the request that created the session, and the newest transcript messages. Everything after the instruction — the creating request, truncated at a character boundary, the two headings and the transcript lines — is counted against `SUMMARY_INPUT_BYTES` (4,000 bytes), so the request is bounded however long the session has run and whatever request created it. The answer is stored on the session row, trimmed and cut at `SUMMARY_MAX_BYTES` (200 bytes), and metered as a `ModelCall` of kind `summary`. Neither the request nor the answer is appended to the transcript, so the model's thread is unchanged.

A session whose wake ends in `Stopped` is described too: stop ends a session's turns, not this line about it.

The web pane shows the summary as the row's leading line and drops the node and directory to the meta line under it; a session with no summary keeps its present row. `bosun list` is unchanged.

## Consequences

- A session costs at most one extra model call per `SUMMARY_MIN_INTERVAL` while it works, and everything that call sends after the instruction stays inside `SUMMARY_INPUT_BYTES`; an idle session and a wake that surfaced nothing cost none.
- The stored name is bounded at `SUMMARY_MAX_BYTES` (200 bytes), so a provider that answers at length cannot grow the session row that every session-list response carries. On a 390 px phone a row shows about 51 characters, eight words, of it; a longer name is ellipsised rather than wrapped.
- The call is metered, so it appears in the clients' per-call context lines, as a compaction call already does: `claude-3-7-sonnet summary (1234 in, 40 out, $0.0040)`. That line is bookkeeping about a call, not a transcript entry; no message is added, and the model's thread does not contain it.
- A description appears about `SUMMARY_IDLE_BEFORE` after a wake ends, so a session that never stops working is described by its state rather than by its summary until it does.
- While the summary call is in flight the loop is not reading its event channel, so an event that lands then waits for the call to finish — seconds, bounded by the response cap.
- The summary is written by the session's current model at that model's prices, and a persona switch changes the writer on the next refresh without restarting the loop.
- A control-plane restart loses the last-summary time and the covered message id, so the first idle after a restart writes a summary even when the thread has not moved.
- The transcript and the model's context are unchanged: the summary text never becomes a message, and the only line a session's own record gains is the metered call. A store that refuses the write is logged and the loop carries on, because a name is not worth a session.

## Revisit When

- Sessions are created faster than they are read, and the extra calls measure as a cost; then the summary moves to a configured cheaper model, or to one call per session.
- A user wants to edit the name: the column and `set_summary` are already there, and only an API route is missing.
- A row must carry more than one line, or a summary must be readable in full; then the session screen shows it rather than the list row.
- The compaction summariser and the summary call diverge in the request they make; then `ask_out_of_band` grows the two shapes it serves, or they separate again.
