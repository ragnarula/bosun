# ADR: The agent loop records its activity as durable events

**Date:** 2026-09-12
**Author:** Raghav

## Context

Sessions run a per-session agent loop on the control plane. The web pane and the terminal client watch a session over one SSE stream that replays the store's durable events, polls for new ones, and streams live text deltas. The durable events give the clients five coarse session states; the `running` state covers every busy phase of a wake: waiting for the provider's first token, streaming, running a tool on the node, the backoff after an empty reply, and compaction. A stalled run therefore reads exactly like a busy one — the transcript shows only durable outcomes and streamed text, and nothing says whether the loop is waiting on the model or has already come back and hung. The loop itself is the only place that knows which phase it is in.

## Decision Drivers

- A watcher must be able to tell waiting from stuck: which phase the loop is in, and for how long.
- The record must survive a reconnect and be complete when a screen opens after the fact, so an earlier stall can be diagnosed.
- No new session state, no new table, no new endpoint, and one SSE connection per client.
- Token and cost counts keep one source of truth: the existing `model_call` event.

## Options Considered

- **A client-side heuristic on session state.** Rejected: `running` alone cannot distinguish awaiting the provider from a long tool run, so any indicator built on it would report "waiting for the model" while a tool executes.
- **Live-only activity frames on the per-session broadcast channel.** Rejected: the broadcast is not stored, so a reconnect loses the history and a stall diagnosed later has no record. This was the "one tube, three lanes" shape considered first.
- **A separate activity table fetched by a new endpoint.** Rejected: the clients would merge two sources with two reconnect cursors, and the replay-volume argument is weak — activity rows are a small fraction of the tool traffic the events stream already carries.
- **New `Event::Activity` variants in the existing events table (chosen).** The loop appends them like any other durable event, so replay, the 500ms poll, reconnects, and both clients' existing SSE handling carry them unchanged. This matches the existing metering event, `ModelCall`, which is already durable bookkeeping rather than conversation.

## Decision

The loop appends `Event::Activity` events — serde `kind` `"activity"` — to the events table at phase transitions, each carrying `at_ms` (unix milliseconds) and the phase's detail. The phases are `wake_started`, `wake_dropped`, `request_sent`, `first_token`, `response_complete`, `tool_started`, `tool_finished`, `empty_retry`, `compaction_started`, and `compaction_finished`.

Tokens and cost stay on `ModelCall`; `Activity` carries the stop reason and durations, never token counts, so no count has two sources of truth. No timestamp column is added to the events table: `at_ms` lives in the payload.

The clients render `Activity` events in a debug console — a collapsible monospace log under the session header in the web pane, a key-toggled overlay in the terminal — and derive the waiting indicator from the newest `Activity` while the session is `running`. The status the clients already show replaces the generic `running` word with the live phase and its elapsed time; idle states keep their current words. A `running` session with no `Activity` yet still shows `running`.

## Consequences

- The events table grows by a few rows per model call, and a session's event replay now includes its activity. Each attach parses those rows, which is a small cost next to the tool traffic already replayed.
- One turn is now described by two durable events — `Activity` with the stop reason and `ModelCall` with the tokens — deliberately split so tokens keep a single source of truth.
- The loop writes more events per turn; a failed append is a turn failure exactly as today, because `append_event` already errors out.
- Clients compute the current phase's elapsed time from local receipt time, not `at_ms`, so clock skew does not distort the live counter; `at_ms` orders history and gives past durations.
- `Activity` events enter the session's event stream but never the model's thread, because the thread reads messages, not events, so the model's context is unchanged.

## Revisit When

- The events table growth measures as a cost on long sessions; a capped ring or a separately fetched activity log replaces this.
- A watcher must see activity while the store is unavailable; then a live-only broadcast supplements the durable replay.
