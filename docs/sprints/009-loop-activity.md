# Sprint 009 — The loop narrates itself

A session's status says `running` for every busy phase of a wake, so a watcher cannot tell waiting for the model from a run that has stalled. The loop records its activity as durable events, the status shows the live phase with an elapsed counter, a debug console lists what the loop has done, and the web pane shows each model call's input and output context again.

Status: **proposed**.

> The durable-event placement and the phase vocabulary are recorded in `../adrs/2026-09-12-loop-activity.md`. The terminal client already renders `model_call` context lines; this sprint restores the web pane's equivalent and adds the indicator and console to both clients.

## Confirmed decisions

- **The loop is the only thing that knows its phase.** The indicator and the console read phase transitions the loop appends, never a client-side guess built on `running`.
- **Activity is durable, in the existing events table.** New `Event::Activity` events ride the same replay, 500ms poll, and reconnect the transcript uses. No new table, endpoint, connection, or session state.
- **Tokens and cost keep one source of truth.** `Activity` carries the stop reason and durations; token counts stay on the existing `model_call` event.
- **A phase transition is a point in time, not a pulse.** The loop appends one `Activity` event per change of phase, each carrying `at_ms`. The current phase is the newest event; durations are deltas between consecutive events.
- **The waiting indicator replaces only the `running` word.** While a session is running, the status shows the live phase and elapsed (`awaiting model · 4s`); idle states keep today's words, and a running session with no activity yet still reads `running`.
- **The current phase's elapsed time uses local receipt time.** Clients count up from when the newest activity frame arrived, not from `at_ms`, so clock skew does not distort the live counter. `at_ms` orders the console's history and gives past durations.
- **A debug console is live machinery, not conversation.** It renders `Activity` events only; tool call/result content stays in the transcript, and the model's thread is untouched because it reads messages, not events.

## CLI surface

Unchanged. No command or flag changes. The terminal client gains a `^D` keybinding to toggle the debug console while attached.

## User stories in implementation order

- [ ] **S1 — The loop records its activity**

As a developer, I want the loop to append an `Activity` event at each phase transition, so a watcher can reconstruct what it is doing.

- `Event::Activity` joins `bosun-common`'s `Event` enum, serialized with `kind: "activity"`, carrying `at_ms` and a typed phase with its detail: `wake_started`, `wake_dropped`, `request_sent` (model, provider), `first_token` (latency), `response_complete` (stop reason), `tool_started` (name), `tool_finished` (name, ok, elapsed), `empty_retry` (attempt, limit, reason), `compaction_started` (input tokens), `compaction_finished` (retired messages).
- The loop appends them through `store.append_event` — the events table is unchanged, since `at_ms` lives in the payload.
- Emit points: `wake_started`/`wake_dropped` in `handle_wake`; `request_sent` before the provider stream, `first_token` on the first stream event, `response_complete` at the collected stop; `tool_started`/`tool_finished` around tool execution; `empty_retry` when a turn enters the retry backoff; `compaction_started`/`compaction_finished` in `maybe_compact`.
- An interrupted stream emits no `response_complete`, so the record never claims a finished turn that did not finish.
- Tests: a clean text turn emits `request_sent`, `first_token`, `response_complete` in order with `at_ms`; a tool turn emits `tool_started` then `tool_finished` with the elapsed; three empty replies emit `empty_retry` for attempts 1, 2, 3; compaction emits `compaction_started` then `compaction_finished`; a redundant wake emits `wake_dropped`; an interrupted stream emits no `response_complete`.

- [ ] **S2 — Activity is delivered over the existing stream**

As a developer, I want activity events to reach both clients through the existing durable replay and poll, so a reconnected screen replays the loop's history.

- No API change: `durable_frame` already serializes any `Event`, and the 500ms poll delivers newly appended events.
- Both clients deserialize `Event::Activity` and route it to the debug console, never to the transcript; an unknown event must not render as transcript noise.
- Tests: an SSE reconnect replays activity events from the store; the poll delivers an activity event appended while attached; the web pane and the terminal accept an `"activity"` frame without adding a transcript line.

- [ ] **S3 — The waiting indicator shows the live phase**

As a user, I want the status to say what the loop is doing and for how long, so I can tell waiting from stuck.

- While the session is `running`, the status shows the newest activity phase with a counting elapsed time: `awaiting model · 4s`, `running tool file_read`, `retrying empty reply (2/3)`, `compacting`. Idle states keep today's words; a running session with no activity yet shows `running`.
- The terminal status line carries the phase; a small redraw tick keeps the elapsed counter moving while no input or stream event arrives. The web pane's header label carries the phase, refreshed on a short interval while a session is open.
- Tests: latest activity `request_sent` labels the status `awaiting model` with a counting elapsed; `tool_started` labels it `running tool X`; `empty_retry` labels it `retrying empty reply (2/3)`; `waiting_for_input` is unchanged; `running` with no activity shows `running`.

- [ ] **S4 — A debug console lists the loop's activity**

As a user, I want a console that lists what the loop has done, so I can see the whole sequence and its durations.

- Web pane: a monospace log between the session header and the transcript, closed by default, toggled by tapping the status/phase line; it auto-scrolls and shows each activity row with its phase, detail, and the duration derived from `at_ms`.
- Terminal: a `^D`-toggled overlay pane listing the activity rows; the status line still carries the current phase.
- Tests: toggling works in each client; each activity event renders one row; consecutive rows show a duration; the log replays history after a reconnect.

- [ ] **S5 — The web pane shows per-call context lines**

As a user, I want each model call's input and output tokens in the transcript, so I can see the context used so far.

- The web pane renders each `model_call` event as one dim monospace line, matching the terminal's format: `claude-3-7-sonnet completion (12,340 in · 421 out · $0.04)`.
- The terminal already renders these lines; nothing there changes.
- Tests: the web pane appends the line in the terminal's format; a compaction call renders with `compaction` as its kind.

## Acceptance measures

Measured over one attached session with a delegated sprint of work.

| Measure | Before | Target |
|---|---|---|
| Status text while running | `running` for every busy phase | names the live phase with elapsed |
| Stalls diagnosed | by reading logs after the fact | phase and elapsed on screen, live |
| Web pane model-call context | none | one dim line per model call |
| Loop history after reconnect | none | full activity replay |
| New session states, tables, endpoints | — | none |

## Out of scope

- No stall detection or warning thresholds: the phase plus elapsed is the diagnostic.
- No cumulative running token or cost totals; this sprint restores per-call context only.
- No change to the node, executor, tunnel, or store schema, and no change to the live text-delta broadcast.
- No change to what the model's thread contains: activity is an event, never a message.
