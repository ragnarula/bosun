# ADR: The loop states its context size in the transcript

**Date:** 2026-09-27
**Author:** Raghav

## Context

Every completion reports its input tokens, and the loop keeps them: `last_input_tokens` is what `maybe_compact` compares against the compaction threshold, and each count is recorded as a `ModelCall` event, which both clients draw as a row — `claude-test completion (12 in, 60 out, $0.0000)`. The model's own thread holds nothing of the kind. An agent working a long session therefore cannot see how full its context is, how close it is to compaction, or that the thread it is reading is about to be replaced by a summary, so it cannot decide to clear its context, to write something down, or to stop going deeper.

The operator fixed the shape: one durable transcript row at the completion that ends a turn, visible to the user and carried in the model's own window, stating that completion's input tokens, the window the loop measures them against, and how close the next compaction is; emitted only while that count is at or above half the window, and at most once per turn; the count is the loop's own `last_input_tokens` and not a fresh estimate; the limit stays the existing constant, with per-model windows a follow-up; the row replays like any message; and there is no separate warning row, because the row's own percentage is the warning.

## Decision Drivers

- The agent has to be able to read it, so it cannot be an activity: activities reach the reader alone.
- The row is itself part of the context it describes, so it must not be written on every completion of a young session, and not more than once in a turn: a turn that took ten completions says the same thing once.
- The count has to be one the loop already trusts. An estimate of the request about to be sent would differ from the count compaction uses, and a session would then be told a number that disagrees with its own compaction.
- The reader should see the same thing the model sees, in the same place, so the two cannot drift.
- Nothing else changes: the limit stays a constant, and no second warning is added.

## Options Considered

**1. A durable message block, written once a completion's input tokens reach half the window. (chosen)**

`Block::ContextSize { tokens, window, compact_at }`, appended when the completion that ends the turn is recorded, as an assistant row. The pane draws it as a centred, dim line; the terminal client draws it left-aligned with the `──` divider, the way it draws a summary, which is what that client does with prose a session wrote about itself.

**2. An activity phase, like the model-call row. (rejected)**

It is the cheapest thing to add — the loop already emits activities and the readers already draw them — and it is exactly wrong: an activity is an event, and events do not enter the model's window. The issue is the agent's blindness, so a row the agent cannot read answers nothing.

**3. A message on every completion. (rejected)**

Simplest and always current, and it pays for itself in the one currency the row is about: a session at 3% would carry a line per completion describing a context that has nothing to decide, and the lines accumulate in the thread they describe. Half the window is the point where a session has a decision to make, and below it the note is silent.

**4. A count on request, through a tool. (rejected)**

A tool would let the agent ask when it cares, and cost nothing otherwise; it also needs a tool, a schema and a call, and the agent can only ask once it suspects it should. The operator chose the standing row above the threshold, which is the moment the question matters.

**5. A separate near-compaction warning. (rejected)**

Two rows would say the same number twice, and the second would need its own threshold to argue about. The row's percentage against the window is the warning; the count that fires compaction is on the same line.

**6. Per-model windows, read from config or the provider. (rejected for now)**

A single constant is wrong for any model but the one it was chosen for, and the providers do not report a window over the API. Reading it from config is its own change — which model field, what default, what the pane shows when it is missing — and the operator left it as a follow-up.

## Decision

`Block::ContextSize { tokens, window, compact_at }` is a durable block: `tokens` is the completion's reported input tokens, `window` is `CONTEXT_WINDOW_TOKENS`, and `compact_at` is the loop's own `compact_at_input_tokens`, which in production is the constant derived from the same window.

The loop appends one at the completion that ends the turn, when that completion stopped normally and its input tokens are at or above `CONTEXT_WINDOW_TOKENS / 2`. That is where the turn has no calls left to run, so a note can never sit between a call and its result, and it is the turn's last completion, so a turn that took several completions writes one note and not several. An empty completion writes none: it is retried with an identical request, and the retry's own count is the one that matters. A completion that was interrupted writes none either: its counts never arrived, and the previous turn's count would be a lie. A turn that ends by asking a question writes none, because its next request is the answer rather than the thread.

The row is an assistant message, so it is stored, replayed and archived like any other. `context_size_line` words it in the loop, and both clients carry their own copy of the same words, as they do for the other blocks that read as prose: `context: 600000 / 1000000 tokens (60%), compaction at 950000`.

## Consequences

- The count is the last completion's, so a session reading the note sees the context as the request that just went out held it. The next request holds a little more: the note itself, and whatever the turn appended after it. The note's percentage is therefore a slight understatement, which errs the way an agent should — it does not think it has more room than it has.
- The row is only as current as its threshold allows. Between half the window and the point where the count changes, a session sees the count of the completion that ended each turn above half, and a reader sees them as history rather than as a live gauge.
- Writing it costs tokens in the very context it describes, and two bounds keep that small: nothing is written below half the window, and a turn writes one note however many completions it took. A long turn that crosses the line repeatedly says so once, at its end.
- Compaction treats the note like any row: it can be retired with the tail it sits in, and the summary stands where that tail was. A note is not special-cased, and a session whose last note was retired learns the size again from the next completion above the threshold.
- An agent can act on what it reads — it can clear its own context, write down what it must not lose, or stop going deeper — but it cannot trigger compaction, which is automatic and has no directive yet (issue #10). The note tells it that a compaction is coming, not that it can ask for one.
- Nothing else moved: no configuration, no API, no client state. The limit is one constant for every model, which is the next thing to fix.

## Revisit When

- A model's window differs from the constant: the note then states a limit that is not the model's, which is issue #11's own follow-up.
- An agent may trigger compaction, which would make the count that fires it an action rather than a fact.
- The note's threshold or its wording proves wrong in use: a session at half the window may find it noise, or a session at 90% may want it on every completion.
- The transcript's own growth from notes matters, which would argue for a note that replaces the previous one rather than accumulating.
