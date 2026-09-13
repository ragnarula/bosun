# ADR: Durable events carry their append time, and clients render it in local time

**Date:** 2026-09-12
**Author:** Raghav

## Context

Sessions run a per-session agent loop on the control plane. Both clients — the terminal client and the web pane — watch a session over one SSE stream that replays the store's durable events, polls for new ones, and streams live text deltas. The transcript therefore says what happened and never when: the only times on screen are the activity console's durations, which are deltas between `Event::Activity` stamps. A reader cannot tell whether a turn took two seconds or two minutes, so a session that moves quickly reads like one that has stalled.

The events table holds `seq`, `session_id` and `payload`, and the payload is the serialized `Event`. `Event::Activity` carries `at_ms` inside that payload; `docs/adrs/2026-09-12-loop-activity.md` records that choice, and that no timestamp column was added to the events table.

## Decision Drivers

- A reader must see when each transcript entry landed, not only what it said.
- The record must survive a reconnect and a screen opening after the fact: a time replayed from history must be the time the event happened.
- Both clients must agree on the time of one event.
- One source of truth per fact. The store is the only writer of durable events, and the only place that knows when an append happened.
- No new table, no new endpoint, no new connection.

## Options Considered

- **Client receipt time.** Each client stamps an entry when its frame arrives. Rejected: both clients replay the whole event history on attach, so every replayed entry would carry the moment of the reconnect, and two clients opened at different times would show different times for the same event.
- **A timestamp column on the events table.** Rejected: the time would sit beside a payload that already carries times, and it needs a migration for every existing database. `Event::Activity` keeps its `at_ms` in the payload, so a column would give one fact two homes.
- **A server-formatted time on the frame.** Rejected: the control plane runs in one timezone and readers in others, so the server cannot format a time that a reader compares against their own clock.
- **A stamp in the event payload, written by the store at append (chosen).** It mirrors `Event::Activity`: replay, the 500ms poll and reconnects carry it unchanged, and no schema changes.
- **UTC on screen.** Rejected: a reader times a session against their own day, and only the client knows its own clock. The wire keeps UTC; each client renders local time.

## Decision

Every durable `Event` variant other than `Activity` — `Message`, `State`, `Permission`, `Persona`, `ModelCall` — carries `at_ms: Option<u64>`, unix milliseconds, and `Event::at_ms()` reads the stamp of any variant. `Activity` keeps its existing `at_ms: u64`. The field is repeated on each variant rather than wrapped in an outer struct, because `Event` is an internally tagged enum that both clients match on directly, and a wrapper would nest it one level deeper in the stored payload.

The store writes the stamp as it appends the event, in the same transaction as the row the event describes: `set_state`, `mark_interrupted`, `set_permission`, `switch_persona`, `append_model_call`, `insert_message`, and the `Event::Message` that `route_answer` appends for a resolved ask.

It is `Option<u64>` because events appended before this change carry no `at_ms`. They keep parsing, they replay, and they render without a time. A stored event is never restamped on read.

Both clients render a dim `HH:MM:SS` local time at the start of each durable entry's first row, and leave a live text delta unstamped because a delta is not durable yet. The terminal client writes the time into a fixed-width gutter before the per-kind prefix, so wrapped rows stay aligned. The web pane writes it in a leading gutter column beside the entry, so the time stays on the entry's first row whatever the entry's first block element is. An entry whose event carries no stamp shows no time.

The activity console's live counter keeps counting elapsed time from local receipt, not from `at_ms`, so a skewed control-plane clock cannot distort a counter that is running now; the console's rows for past activity keep deriving their durations from `at_ms`, as they did before this change.

## Consequences

- The control plane's clock decides the transcript's times and the activity console's past durations. A skewed clock shows skewed values; the console's live counter, counted from local receipt, stays right.
- The events payload grows by about 25 bytes per event, and a session's replay carries all of it.
- Events written before this change show no time, and no time for them can be reconstructed.
- Both clients must render the stamp; a third client added later must too, and it must decide its own local formatting.
- The terminal client needs timezone data to render local time, so the `bosun` binary now depends on `chrono`; the web pane gets its offset from the browser and needs no crate. The two clients format independently, so their output can differ in locale details.
- A reader can now see the gaps between entries and the tempo of a running session.

## Revisit When

- Times for sessions whose events predate the field become necessary, and deriving an approximate time from `seq` order or from the session's start is good enough.
- The payload growth measures as a cost on long sessions, and a coarser stamp — one per turn — replaces the per-event one.
