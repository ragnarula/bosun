# Sprint 015 — Chain of Command

An open session shows a group chat of its whole tree. Every agent's text, file changes and messages to other agents are mixed into one stream, and the message box says "Message the crew…". That is not how Bosun works. The reader's messages go to the root session only, and children are watch-only: every instruction to a child goes through its parent's `spawn` or `message_child` call, and every child answers its parent with a report, a question or a failure. This sprint makes the clients show that. The Chat view holds the conversation between the reader and the lead. A new Crew view shows the chain of command as a tree, with the flow of orders, reports and questions between agents below it. Tapping an agent opens its own thread, read-only.

Status: **planned**.

The screens are drawn on the design canvas "Chain of Command" (https://claude.ai/artifact/7HqtnMSHSNaeQs1tZxWmfB): Chat, Crew, a crew member, and the desktop layout.

## What the control plane already does

This sprint changes the clients only. The control plane already runs a tree of any depth and keeps the reader at the top of it:

- **Any session can spawn.** The loop advertises `spawn` at every depth when the spawner is attached (`spawn_is_advertised_at_every_depth_when_the_spawner_is_attached`). A persona whose allowed tools leave out `spawn`, or a read-only session, does not spawn. There is no depth limit.
- **Every descendant belongs to the root's tree.** A child takes its parent's `owner_id` and its parent's id as `parent_id` (`spawn.rs`), so a grandchild's `owner_id` is the root. `tree-events` serves the whole tree on one stream.
- **Children are watch-only at any depth.** Messages, interrupts, permission and persona changes are refused on any session with a `parent_id` (`user_actions_are_refused_on_a_grandchild_like_on_any_child`).
- **A question climbs the tree, and the answer goes straight down.** A grandchild's question surfaces to the root, and the reader's answer is appended to the grandchild's thread with no model passing it on (`a_user_answer_at_depth_routes_to_the_grandchild_leaf_without_any_model_relay`).

Today's pane and terminal client draw the tree flat: `treeMembers` lists every session with the root's `owner_id` in creation order, and the chat mixes all of them.

## Confirmed decisions

- **The root is the lead.** Copy names every agent by its persona, as now. Where copy means the root's role, it says "the lead". The message box says "Message" and the root's persona name, for example "Message Lead…".
- **Chat is the lead's conversation.** It draws the root session's thread and nothing else: the reader's messages, the root's text, the root's diff cards and folded tool lines, the root's questions, and notes such as a cleared context. Children's text, tool calls, diff cards and questions to their parents are not in Chat.
- **Orders are cards.** The `spawn` and `message_child` calls in one assistant message of the root are one Orders card, with one row per call: the child's avatar, its persona name, the first line of the instructions, and a state word. Until a `ChildEvent` from that child follows the call in the root's thread, the row shows the child's live state. After it, the row shows the outcome: done, asked or failed. A tap on a row opens that crew member.
- **Reports stay in Chat.** Each `ChildEvent` in the root's thread is one muted line: "Reviewer reported to Lead: …", "Builder asked Lead: …", or a red line for a failure. A long text shows its first three lines and expands on a tap.
- **A surfaced question names who asked it.** The question card names the origin leaf, read from the `origin` of the `ChildEvent` ask that came before the root's `Ask` block, and shows the path from the leaf to the reader through each parent, from the session list's `parent_id`. The answer routes as it does today. The card says that the answer goes to the leaf word for word.
- **A crew bar replaces the crew strip.** Under the header of Chat, one row shows the children's faces and a count of who is working and who asks the reader. A tap opens Crew. Tapping a face no longer filters the chat.
- **Crew is the chain of command, then the flow.** The top of the Crew view is a tree: the reader, the root under it, and each session under its `parent_id`, drawn with connector lines. Each node shows the avatar, the persona name, a state word, what it is doing now, and its newest order or report. Members that reported and stopped stay in the tree while the tree lives. Below the tree, Flow lists every order, report, question, answer and failure between agents in the tree, newest first, with chips for All, Orders, Reports and Questions. Both come from the tree stream and the session list.
- **The leaf of a question shows that it asks the reader.** While the root waits on a surfaced question, its origin leaf shows "asks you" with the needs-you ring, and each session between them shows "waiting". Today only the root shows it.
- **A crew member has a read-only screen.** It opens from Crew, an Orders row, or Flow. Its header shows the path ("Lead › Builder") and two views, Thread and Log. Thread draws the orders the member received, its own text, diff cards and folded tool lines, its questions with their answers, its reports, and its own orders to its children as Orders cards. Below the thread, the member's crew lists its children. Log is today's transcript of that session.
- **No message box for a child.** The member screen ends with a bar: "Builder takes orders from Lead. To change its work, tell Lead." and an **Ask Lead** button. The button opens Chat with "About Builder: " in the message box. This keeps the agent-tree rule that the reader steers a child only through the root.
- **Log moves to the member screen.** The phone tab bar becomes Sessions, Chat, Crew, Tasks and Files. The root's Log opens from the root's node in Crew, which opens its member screen on Log, because the root's thread is Chat. The child panel goes: the member screen's Log replaces it.
- **A child's link opens its tree.** Opening a child's id, from `#s=<id>` or a notification, opens the root's session on that member's screen. The watch banner goes. Opening a member screen adds a history entry, so back returns to where the reader came from.
- **From 900px** the right column shows Crew, Tasks and Files, switched by chips, with Crew first. The header keeps the Chat and Log chips, and Log there is the root's.
- **The terminal client follows the same rules.** See [CLI surface](#cli-surface).

## CLI surface

- `bosun open` views on Alt+1 to Alt+4 and F1 to F4: Chat, Crew, Tasks and Files. Chat follows the pane's rules: the root's thread, Orders as one line per child under the root's text, reports and asks as dim lines, a surfaced question naming its leaf and path.
- Crew draws the tree with box-drawing lines, each member as its tag, state word and caption, with the flow below it. ↑/↓ pick a member, Enter opens its thread, and Alt+5 or F5 opens the picked member's Log. Esc returns to Crew.
- Tab no longer cycles the chat through members.
- `bosun open <child id>` opens the root's tree on that member's thread.
- The crew line under the status line stays.

## Constraints from the code and the ADRs

- **No change to the control plane, its routes or its stream.** Every fact the views need is in `tree-events` and `GET /sessions`: `spawn` and `message_child` calls are orders, `ChildEvent` blocks are reports, asks and failures, the `origin` on an ask event names the leaf, and `parent_id` gives the tree.
- **Children's text arrives a message at a time.** The tree stream carries the root's live deltas only (`2026-10-08-one-stream-per-tree.md`), so a member's Thread does not stream token by token. This is unchanged.
- **The agent-tree ADR stands.** Watch-only children and hierarchical asks are its decisions, and this sprint draws them. Its "revisit when a user must message or steer a child directly" is not met: the Ask Lead button sends the reader to the root.
- **A new ADR replaces parts of three.** `2026-10-08-pane-crew-view.md` (the four views, the chat rules, the crew strip and its filter), `2026-10-08-terminal-crew-view.md` (the chat rules, the view keys, Tab cycling, a child opening on Log) and `2026-09-27-subagent-panel.md` (the panel). The new ADR records the lead-only chat, Crew, the member screen, and why a child has no message box.
- **The pane's principles.** The only text field is still the message box. Ask Lead fills it so the reader starts from a choice. Each view shows a fact once: the Orders card holds a child's state in Chat, and the tree holds it in Crew.
- **The browser tests.** `crates/bosun-control/tests/browser/pane.py` covers the chat today. New checks: a child's text does not appear in Chat, an Orders card row changes from working to done when the child reports, a surfaced question names its leaf and path, Crew draws a grandchild under its parent, a member screen has no message box, and a child's `#s=` link opens its member screen.

## User stories in implementation order

- [ ] **S1 — Chat holds the lead's conversation.** The chat draws the root's thread only. Orders cards with live and final row states, report, ask and failure lines, the surfaced question with its leaf and path, the "Message Lead…" placeholder, and the crew bar in place of the strip.
- [ ] **S2 — Crew shows the chain of command.** The tree from `parent_id` with connector lines, each node's state word, caption and newest order or report, and "asks you" on the origin leaf with "waiting" on the path to it.
- [ ] **S3 — Flow under the tree.** Every order, report, question, answer and failure in the tree, newest first, with the four chips.
- [ ] **S4 — A crew member's screen.** Thread and Log, the member's crew, the Ask Lead bar and its prefilled message. A child's link opens it, a history entry backs out of it, and the child panel and the watch banner go.
- [ ] **S5 — Tabs and the desktop.** The tab bar becomes Sessions, Chat, Crew, Tasks and Files, with the needs-you dot on Chat. From 900px the right column switches Crew, Tasks and Files.
- [ ] **S6 — The terminal client.** Chat on the same rules, Crew with the tree, flow and member picking, Log on Alt+5 and F5, and `bosun open <child id>` on the member's thread.
- [ ] **S7 — The docs that own the current state say so.** CLAUDE.md, the README, and the new ADR with the superseded notes on the three it changes.

## Out of scope

- A message box for a child, or any way for the reader to message a child directly.
- Live text deltas from children.
- Any change to the control plane, the store or the stream.
- Keeping a tree after it is stopped.
