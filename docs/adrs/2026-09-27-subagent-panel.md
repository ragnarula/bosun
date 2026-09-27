# ADR: A subagent is watched in a panel beside the session, on its own stream

**Date:** 2026-09-27
**Author:** Raghav

## Context

The web pane shows one session at a time. A child session appears in its parent's transcript as one line — a `ChildEvent` block, `[child <id> report] …` — whose id is a link that opens the child as the session view. Following a subagent therefore costs the parent's place: the transcript is replaced, the event stream switches, and the reader has to find their way back.

The pane's pieces this decision sits on:

- One `EventSource` per open session, created by `showSession` and closed by `closeSession`. Durable frames carry their event seq as the SSE id, so a reconnect resumes with `Last-Event-ID` with no work from the pane.
- A session view is a column: header, activity console, transcript, watch-only banner, composer. The composer carries the session's own drafts, the ask record and the persona switch.
- A child's transcript is stored exactly as any session's is, and its blocks are the same blocks: text, tool calls and results, reasoning, asks, summaries, model calls, clears, and its own children's events.
- Sheets are the pane's phone pattern: a fixed panel that covers the view (`#view-sheet`), or an inline card inside the composer (`#ask-sheet`).
- The transcript's scroll behaviour is the reader's: auto-follow turns off when they scroll up, and the control that returns to the newest line is part of the transcript's own box.

Adjacent work: the list half of subagents is issue #23 (children fold under their parent in the session list), and issue #12 lets a session spawn a child on another node. Neither changes what a child's transcript is.

## Decision Drivers

- A reader must be able to watch a child work without losing the parent's place, its scroll position, or its composer.
- Live, not sampled: the child's own event stream already exists and already resumes; a timer would be a second mechanism with a lag.
- One child at a time: the pane is used one-handed on a phone, and a tab strip or a stack of panels is more controls than the task needs.
- The panel is not a second session view. It carries no composer, adds no browser history entry, and shows no watch-only banner: following a child is not opening it.
- A phone cannot show two columns, so the panel has to change shape rather than the pane gaining a second layout.
- The parent's existing behaviour must not change: the session's own stream, its history entry and its scroll control stay as they are.

## Options Considered

**1. A collapsible panel beside the transcript, driven by the child's own stream. (chosen)**

The panel is a second column in the session view, collapsed until the reader follows a child from its line. It draws the child's durable messages with the session's own block renderers, closes the child's stream when it collapses, and on a phone covers the view as a full-height sheet.

**2. Keep opening the child as the session view. (rejected)**

That is what the pane does today, and it is the cost the issue names: the parent's transcript, its scroll position and its composer all leave the screen, and the reader's place in the parent is lost. The address bar is the natural way to move between two sessions — but only when the reader *means* to move.

**3. A tail of the child's transcript fetched on a timer. (rejected)**

It needs a polling interval, a cursor, and a decision about how much to fetch, to reproduce what one more `EventSource` gives for free — including the resume behaviour the main stream already has. It would also lag behind a child that is mid-turn, which is the one time a reader is watching.

**4. Several children at once, in tabs or a stack. (rejected)**

The operator's default is one at a time. Tabs would need a selection, a close control and an ordering, and on a phone they would take the top of the screen that the sheet's header already uses. Nothing here prevents a later decision to show more than one.

**5. A second set of renderers for a child's transcript. (rejected)**

A child's thread holds the same blocks as any session's, so a second implementation would be a copy that drifts at the first new block kind. The panel draws with the session's renderers, and the two transcripts therefore share one stylesheet rule set.

**6. Following a child through the pane's history: a `#s=` entry per panel. (rejected)**

The panel would add entries the reader has to traverse, and the browser's back button would have to mean two things: leave the session, or collapse the panel. Following is not opening, so it writes nothing.

**7. A panel outside the session view, shared with the todos panel (#24). (rejected for now)**

The operator left open whether the todos panel and this one share a column. This panel is about the *open session's* children, so it lives in, and hides with, that session's view; a shared column would need a scope neither of them has yet. #24 can propose the shared shape when it is built.

## Decision

The session view holds a row, `#conversation`, with the transcript's box on the left and the panel on the right. The panel is hidden until a child is followed, so the pane opens with no panel at all, and nothing about it is stored: a reload, a new session, or leaving the session leaves no panel state behind.

A child's line in the parent's transcript carries a `watch` control beside its id. The id keeps its meaning — it opens the child as the session view — and the watch control follows the child in the panel without touching the address bar. `followChild(id)` closes whatever was followed, names the child in the panel's header beside its state dot, and opens a second `EventSource` on that child's events.

The panel's own frame handler renders the child's durable `message` frames and ignores every other frame kind. The session-state frames — the header dot, the status label, the activity console, the ask record, the live paragraph — describe the session the pane is showing, and a child's frames never touch them. The block renderers append to a single module-level target, so a child's frame points that target at the panel's transcript and puts it back afterwards.

The panel's transcript scrolls on its own flag: it follows the child's newest line until the reader scrolls the panel up, exactly as the session's transcript does for the session.

Collapsing closes the child's stream, forgets the child, clears the panel's lines and hides the panel. Nothing else changes: the session's transcript, its scroll position, its composer and its history entry are as they were. `closeSession` closes the panel too, so a session the pane leaves takes its panel, its child and its stream with it.

On a screen wider than 640px the panel is a column beside the transcript, 420px wide and never more than 45% of it. At 640px and below it becomes a fixed, full-height sheet over the view, keeping the same header — which names the child and closes the panel.

The two transcripts share one rule set: the block rules are scoped `:is(#transcript, #child-transcript)`, so a block draws the same in either, and a new block kind needs no second set of styles.

## Consequences

- Watching a child costs one more event stream and one more column, and the reader keeps the parent's place throughout: the parent's transcript does not move, its composer does not change, and the back button still leaves the session.
- The panel made the block renderers target-agnostic. That is a small indirection — one variable the frame handlers set and restore — but it is load-bearing: a renderer that appended to the session's transcript directly would draw a child's lines into the parent.
- A child's frames are filtered by kind rather than reusing `handleFrame`. The session-state arms of that function are the reason: they would drive the session's dot, its activity console and its ask composer from the child's stream.
- The block stylesheet now names two containers. A rule written as `#transcript .block` after this decision would style the session's transcript and not the panel's, and the pane's checks fail such a rule by name rather than letting it pass unnoticed.
- A child that has authored nothing has no line to watch from: the panel is reachable from a child's line, and a silent child has none. The reader can open such a child as the session view (the id link), or wait for its first authored event.
- One child is watched at a time, and collapsing loses the panel's scroll position and its lines: reopening the panel starts the child's transcript from the beginning of the stream's replay.
- The panel is not steerable. It has no composer and no banner, so watching a child is read-only in the panel; steering it is a message to the parent's model, as it always was.
- Nothing about the panel is remembered: not the child, not the collapse, not the scroll. A reload is a fresh session view with no panel.

## Revisit When

- A reader needs more than one child at a time, or a child and the parent side by side with both live: the panel then needs tabs, a stack, or a split.
- The todos panel (#24) needs the same column: a shared panel column with a selection is then the shape, and this panel becomes one of its views.
- The panel must be steerable: a composer in the panel is a second input path for a child, which the tree's watch-only rule has so far refused.
- The panel must survive a reload: the followed child would then be pane state worth storing, and probably an entry of its own.
- Several panels open at once make the per-child streams a cost worth multiplexing, or worth keeping alive when collapsed.
