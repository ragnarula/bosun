# ADR: A session's stream can open at its newest messages, and older pages are read back by seq

**Date:** 2026-09-28
**Author:** Raghav

## Context

`GET /sessions/{id}/events` in `crates/bosun-control/src/api.rs` is an SSE stream. It replays the session's durable events from the `events` table after a cursor (`after=` or `Last-Event-ID`, otherwise from the start), then polls the store for new events and forwards live text deltas. The web pane (`crates/bosun-control/src/ui/index.html`) draws every replayed event into the transcript as it arrives. The terminal client uses the same stream.

A long session holds thousands of events, most of them `activity` and `model_call` events between the messages. Replaying and drawing all of them before the reader sees anything locks up a phone.

Constraints fixed going in:

- Clients that do not ask for less keep the full replay. The terminal client does not change.
- A reconnect resumes from its cursor, as it does now.
- The stream stays one route, with the SSE id as the event seq.

## Decision Drivers

- The newest messages show at once on a phone, whatever the session's length.
- A page is the same size to a reader, whatever the loop recorded between messages.
- The reader can reach every older message, in order, with no duplicate and no gap.
- One request cannot read a whole long session.

## Options Considered

**1. A `tail=` parameter on the stream, and a separate history route that pages back by seq, both counted in message events. (chosen)**

The stream keeps its shape for every client that does not send `tail`. The page boundary is a seq, which the stream already uses as its cursor, so the tail and the pages meet exactly.

**2. Count the page in events of every kind. (rejected)**

Most events are `activity` and `model_call`. A page of 60 events can hold one message or sixty, so the reader sees a different amount of transcript on each open.

**3. Count the page in bytes. (rejected)**

It bounds the transfer, not what the reader sees, and it needs the payload sizes before the cut is known. A single large tool result would take a whole page.

**4. Page back over the stream, with a `before=` cursor on the SSE route. (rejected)**

An SSE response stays open and polls. A read-back is one finite answer, and a plain JSON route gives it without a second long-lived connection per page.

**5. Keep the full replay, and render it faster in the pane. (rejected)**

The transfer and the parse still grow with the session, and the store still reads every row on each open.

## Decision

`Store::events_page(session_id, before, messages)` in `crates/bosun-store/src/store.rs` returns the last `messages` message events before `before` (the session's end when `before` is `None`), with every other event between the first of them and `before`, and `more`, which says whether older events exist. A page holds at least one message, so a page that says `more` always moves back.

`GET /sessions/{id}/events?tail=N`, with no cursor, replays `events_page(id, None, N)`. The stream first sends a frame with no SSE id: `{"history": {"before": <first seq or null>, "more": <bool>}}`. The poll then continues after the last replayed seq. A cursor wins over `tail`, so an EventSource reconnect to the same URL resumes where it left off and sends no history frame.

`GET /sessions/{id}/history?before=SEQ&messages=N` returns `{"events": [{"seq", "event"}], "more"}` from `events_page(id, Some(SEQ), N)`. `N` defaults to `HISTORY_PAGE_DEFAULT` and is capped at `HISTORY_PAGE_MAX`, which also caps `tail`.

The pane opens a session with `tail=TAIL_MESSAGES`. It reads back `EARLIER_MESSAGES` at a time, when the transcript scrolls near its top or when the reader taps the row at its top. A page is drawn off screen and inserted above the drawn lines, with the reader's distance from the bottom kept. Of a page's events, the pane draws the ones the transcript shows (messages, model calls and warnings). State, permission, persona and activity events describe the present, which the header and the live stream already carry.

## Consequences

- A session opens with its newest messages at once, and the work to open it does not grow with the session's length.
- The terminal client, and any client that does not send `tail`, is unchanged.
- The activity console on the pane holds only the activity since the tail's start.
- A page cut can separate things the pane joins when it draws them in order. The pane joins a question to its answered copy across the cut. A `message_child` result whose call is on the other side of the cut shows no watch control.
- A diagram drawn after a page is inserted changes the height above the reader. A browser without scroll anchoring, such as iOS Safari, then moves the lines the reader is on.
- The child panel still opens a child's stream with a full replay.

## Revisit When

- A reader needs to open a transcript at a point in its middle, such as a search result or a link to a message.
- The terminal client needs the same windowing.
- A single turn grows so large that a page of messages is itself too slow to draw.
