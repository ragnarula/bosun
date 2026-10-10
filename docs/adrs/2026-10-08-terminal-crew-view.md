# ADR: The terminal client draws a session tree as a crew from the tree stream

**Date:** 2026-10-08
**Author:** Pete

> Superseded in part by `2026-10-10-reader-talks-to-the-lead.md`: Chat draws the root's thread only, the views are Chat, Crew, Tasks and Files on Alt+1 to Alt+4 and F1 to F4 with a member's Log on Alt+5 and F5, Tab no longer cycles the chat through members, and `bosun open <child id>` opens the root's tree on the child's thread instead of the child on Log. The single tree stream, the flags and tags, `bosun list` and the wide layout stand.

## Context

`bosun open` attaches to one session: it replays and follows `GET /sessions/{id}/events`, draws the session's transcript, and polls `GET /sessions` every two seconds to count the session's live children. `bosun list` prints one table row per session, children indented under their root. Neither says who in a tree is working, on what, which files changed, how far the task list has got, or which tree needs the user.

The control plane now serves what a crew view needs: `GET /sessions/{id}/tree-events` streams every session of a tree with each frame's `session_id`, `GET /sessions` serves each session's cost, newest activity, pending question and task counts, `Event::Todos` carries the root's task list, `ToolStarted` names its target, and `edit`, `file_write` and `shell` results carry the files they changed (`docs/adrs/2026-10-08-one-stream-per-tree.md`, `docs/adrs/2026-10-08-crew-state-for-clients.md`). The web pane draws a crew member as a robot avatar with its persona's signal flag; a terminal cannot draw either (`docs/adrs/2026-10-08-persona-avatars.md`).

## Decision Drivers

- The terminal shows the same crew as the pane: Chat, Tasks, Files and Log, with the same rules for what a chat holds.
- Every existing key and command of `bosun open` keeps working, and so does typing a message that starts with a digit.
- One connection per open tree, with the reconnect and replay behaviour the client already has.
- Bounded memory, like the transcript's 5000-row cap.
- `bosun list` output reads without colour, in a pipe or with `NO_COLOR`.

## Options Considered

**1. Follow the tree stream alone, and draw Log from the opened session's frames. (chosen)**

One connection feeds everything. Every durable frame feeds the crew; a frame whose `session_id` is the opened session's also feeds the transcript, the permission, the state and the live ask, exactly as `/events` did. The reconnect resumes from the last seq of the tree, so a frame of any member is applied once.

**2. Keep `/events` for Log and open `tree-events` beside it. (rejected)**

Two streams need two cursors, two reconnect loops and two outage notices, and they replay the opened session's events twice. The only thing the second stream adds is a watched child's live text deltas, which the tree stream does not carry.

**3. Bare `1` to `4` switch views while the input box is empty. (rejected)**

A message that starts with a digit, such as a numbered list, would lose its first key to a view switch. Alt+1 to Alt+4 and F1 to F4 never collide with typing. A watched child has no input box, so the bare digits switch its views too.

**4. A coloured block or a Unicode flag glyph for a crew member. (rejected)**

A block of colour carries the persona only in colour, which a reader without colour or with colour blindness loses. Regional-indicator flags are country flags, not signal flags, and render at varying widths. Two letters in the flag's two colours read with or without colour and keep a fixed width.

## Decision

- `cmd/bosun/src/crew.rs` holds the crew: the flag of each persona, its two-letter tag, the state word that always follows a tag, the caption verbs, and `Crew`, which applies tree frames into the chat entries, the task list, the changed files, the newest edit and each member's newest activity.
- Flags: Lead P (white on blue, `Ld`), Architect K (blue on yellow, `Ar`), Builder O (yellow on red, `Bu`), Reviewer U (red on white, `Rv`), Researcher G (yellow on blue, `Rs`). Any other persona gets D, T, J, N or X by 32-bit FNV-1a of its lowercase name modulo five, in that order, and a tag of its first two letters. A client that wants the same flag uses the same hash.
- Members come from the session list, root first, then by creation; a second member of one persona is named "Builder 2". The stream's state and message events update a member between polls.
- Chat rules: a child's user-role text is never drawn; a `spawn` call is a post to the persona and a `message_child` call a post to the child; a child's assistant text is its post; a `ChildEvent` report is drawn only when the child's own post with that text is not in the last 200 entries; a failure is a red line; an ask is an amber block, filled with its answer when the reader or a routed answer gives one. Other tool calls between two messages fold into one dim line per member. An `edit` or `file_write` result draws `✎ path +a −r` with up to six diff lines taken from the edit's own `old` and `new`.
- `bosun open` opens a root on Chat and a child on Log. Alt+1 to Alt+4 and F1 to F4 switch Chat, Tasks, Files and Log; Tab cycles the chat through each member's posts and back to everyone; in Files, ↑/↓ pick a path, Enter shows its diff lines and Esc returns. A crew line under the status line shows each member's tag, state word and caption.
- From 140 columns Tasks and Files sit in a column beside the main pane, and the view keys switch only Chat and Log.
- `bosun list` prints one entry per tree under `NEEDS YOU`, `WORKING` and `IDLE`, with ANSI colour only when stdout is a terminal and `NO_COLOR` is unset.
- The chat holds at most 5000 entries, the file list 500 paths, and open edit calls 256.

## Consequences

- A watched child's Log no longer streams its text token by token: the tree stream carries the root's deltas only, so a child's text appears when the store writes it.
- Opening a session replays its whole tree, not only the session, so a large crew costs more to open.
- The chat is assembled in the client from the store's rows. If the store's shapes for `spawn`, `message_child`, results or child events change, the client's rules change with them.
- The report match is by exact text: a report that differs from the child's last post by more than whitespace shows twice.
- The terminal and the pane each carry the flag table and the hash; a change to one must be made in the other.
- The list cannot show a pending question's text, because `GET /sessions` carries only that one is waiting.

## Revisit When

- The tree stream carries children's live deltas: a watched child's Log streams again.
- The pane chooses a different hash for spare flags: the terminal follows it.
- A crew grows large enough that replaying the whole tree on open is slow: open with `tail=` and page back with `tree-history`.
- `GET /sessions` carries the pending question's text: the list shows it.
