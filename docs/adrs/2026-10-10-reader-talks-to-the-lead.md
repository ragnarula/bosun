# ADR: The reader talks to the lead, and watches the crew as a chain of command

**Date:** 2026-10-10
**Author:** Pete

## Context

A session tree is a root session and the sessions it spawned, at any depth: every descendant carries the root's id in `sessions.owner_id` and its direct parent's id in `parent_id`. The agent-tree ADR (`2026-09-03-agent-tree.md`) makes children watch-only. The reader's messages, interrupts and setting changes go to the root, every instruction to a child is its parent's `spawn` or `message_child` call, and a child answers its parent with one authored `ChildEvent`: a report, a question or a failure. A question climbs the tree one parent at a time, and the reader's answer is appended to the asking leaf's thread with no model passing it on.

The clients drew the tree flat. The pane's Chat view and `bosun open`'s Chat drew every member's text, file changes and messages to other agents in one stream (`2026-10-08-pane-crew-view.md`, `2026-10-08-terminal-crew-view.md`), and the message box said "Message the crew…". The pane followed one child in a panel beside the transcript (`2026-09-27-subagent-panel.md`), and a child's address opened the child as the session view with a watch-only banner.

The tree stream (`2026-10-08-one-stream-per-tree.md`) already carries every member's messages with their `session_id`, and the session list carries each member's `parent_id`, state and newest activity.

## Decision Drivers

- The screen states who the reader talks to: the root, and only the root.
- The shape of the tree, at any depth, and the orders, reports and questions between its members, readable without the full log.
- One agent's own work readable on its own.
- No change to the control plane, its routes, its stream or the store.
- The pane's principles: one hand on a phone, the message box as the only text field, each fact once.

## Options Considered

**1. Chat holds the root's conversation; Crew holds the tree and the flow; each member has a read-only screen. (chosen)**

Chat draws the root's own thread. The root's orders and its children's reports are part of that thread, so they stay in Chat as compact cards and lines. Everything else a member does is on that member's screen, opened from Crew.

**2. Keep the group chat and label each post with its sender and recipient. (rejected)**

The labels say who wrote what, but the stream still interleaves every member, so a reader with five agents reads five conversations at once to find the one addressed to them. The message box still sits under posts the reader cannot answer.

**3. A message box on each member's screen. (rejected)**

The agent-tree ADR routes every instruction to a child through its parent, so that a parent's model knows what each of its children was told. A message box on a child would either need that ADR reversed, or send the reader's text to the root anyway while looking as if it went to the child.

**4. Show the tree only, with no conversation view. (rejected)**

The reader's conversation with the root is the main thing they do. Hiding it behind a tree node adds a step to every message.

## Decision

- **Chat is the root's thread.** It draws the reader's messages, the root's text, diff cards and folded tool lines, the root's questions, and notes. It draws no child's own text, tool calls or questions to its parent. The message box placeholder names the root's persona: "Message Lead…".
- **Orders are cards.** The `spawn` and `message_child` calls a member makes in a row are one Orders card, a row per call. A spawn row learns its child from the call's result, `{"child_id": …}`. A row shows the child's live state until a `ChildEvent` from that child follows the call in the same thread, then the outcome: done, asked or failed.
- **Reports stay in Chat.** Each `ChildEvent` in the root's thread is one line: "Reviewer reported to Lead: …", "Builder asked Lead: …", or a failure in red. A long text shows three lines until tapped. When the root passes a child's question on to the reader, the child's question line gives way to the question card.
- **A passed-on question names its leaf.** The card names the origin leaf, read from the `origin` of the `ChildEvent` ask from the same child that came before the root's `Ask` block, and the path from the leaf up through each `parent_id` to the reader. The answer still routes mechanically. While the root waits on it, the leaf shows "asks you" and each member between them shows "waiting".
- **Crew is the chain of command, then the flow.** The tree hangs each member under its `parent_id`; a member whose parent the session list no longer holds hangs under the root. Each node shows its face, persona name, state, caption and newest order or report. Flow lists every order, report, question, answer and failure, newest first, with chips for All, Orders, Reports and Questions. The client derives both from the tree stream and the session list.
- **A member's screen is read-only.** It draws the member's own thread: its user-role text as orders from its parent, or as the answer to its own open question; its text, diff cards and folds; its questions; its children's reports; its own orders as cards. It lists the member's children, has Thread and Log views, and ends with a bar that names the parent it takes orders from and an **Ask Lead** button, which opens Chat with "About <member>: " in the message box.
- **Log moves to the member's screen.** The phone tab bar is Chat, Crew, Tasks and Files. The root's node in Crew opens the session's Log view, because the root's thread is Chat. A child's Log follows the child's own `/events` stream in `member-log.js`, which replaces the subagent panel.
- **Addresses.** A member's screen is `#s=<root>&m=<member>`, on a history entry of its own. A child's address `#s=<child>` opens the root's session with the child's screen over it, on the entry that named the child. The watch-only banner is gone.
- **From 900px** the right column shows Crew, Tasks or Files, switched by chips and remembered in `localStorage` as `bosun.side`; the member's screen takes the middle column.
- **The terminal client follows the same rules.** `bosun open` switches Chat, Crew, Tasks and Files on Alt+1 to Alt+4 and F1 to F4, and the picked member's Log on Alt+5 and F5. In Crew, ↑/↓ pick a member and Enter opens its thread, or Chat for the root; Esc returns to Crew from a child's thread or Log. A child's views have no input box. `bosun open <child id>` attaches to the root and opens the child's thread. Tab no longer cycles the chat through members.

## Consequences

- The reader sees one conversation they can answer, and every other agent's work is one tap or key away, grouped by agent.
- The control plane is unchanged: every fact comes from `tree-events` and `GET /sessions`.
- The tree stream replays 300 messages across the whole tree. Chat draws only the root's, so a busy crew leaves Chat with less of the root's history on open than the old group chat showed of everyone's. The Log view reads the root's full history back as before.
- The pane keeps up to 400 recent messages per member to draw a member's thread when its screen opens; an older part of a member's thread is in its Log only. The terminal client keeps one cap of 5000 entries across all threads.
- A child's text arrives a message at a time, as before.
- An open pane holds up to three streams: the root's own, the tree's, and an open member's Log.
- "Crew" names two things in the pane: Home's tab for persona avatars, and the session's chain-of-command view.

## Revisit When

- A reader needs to message a child directly, which reopens the agent-tree ADR.
- The tree stream carries children's live text.
- Chat needs more of the root's history on open than the shared tree tail gives it: read the root's own history back into Chat.
