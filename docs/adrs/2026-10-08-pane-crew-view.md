# ADR: The pane shows a session as its crew, in Bosun Signal

**Date:** 2026-10-08
**Author:** Pete

## Context

The web pane showed an open session as one transcript: every block a row, every model call a cost line, every control but Send behind one `⋯` sheet, and a child as `[child 3f2a… report]`. Home was one row per session with children under a toggle. The control plane now serves one stream per session tree (`2026-10-08-one-stream-per-tree.md`), the crew's state with the session list (`2026-10-08-crew-state-for-clients.md`), and persona avatars (`2026-10-08-persona-avatars.md`). Earlier pane ADRs hold one screen in the page at a time (`2026-09-29-one-screen-in-the-document.md`), follow one child in a panel (`2026-09-27-subagent-panel.md`), and keep the session actions in a sheet.

## Decision Drivers

- Show who is working, on what, and what needs the reader, without reading a transcript.
- Keep the full transcript one tap away, unchanged, for the reader who wants it.
- The pane's principles: one hand on a phone, choices over text, each fact once.
- No build step, no framework, nothing loaded from outside the control plane.

## Options Considered

**1. A crew view over the tree stream, with today's transcript as its Log view. (chosen)**

**2. Restyle the transcript only. (rejected)** It leaves the wall of rows in place and still hides who did what among several agents.

**3. A view per child, as tabs. (rejected)** A crew of five would need five tabs on a phone, and the conversation between agents would be split across them.

## Decision

- **Bosun Signal** is the pane's design language: its tokens are the stylesheet's variables (the older names alias them), Night is the default and Day follows a light system setting, and Bricolage Grotesque, Geist and Geist Mono are embedded woff2 served at `/ui/fonts/`. `signal.js` draws each persona's signal flag, the seeded robot avatar and the line icons with `createElementNS`.
- **A session has four views:** Chat, Tasks, Files and Log, switched by a tab bar on a phone and remembered in `localStorage`. Chat, Tasks and Files are drawn by `crew.js` from `tree-events`; Log is the session's own transcript, read and paged as before.
- **Chat rules:** a parent's `spawn` and `message_child` calls are its messages to a child; a child's user-role text is never drawn; edits and writes are diff cards; other tool calls fold into one line per run; the reader's messages are the only bubbles.
- **The crew strip** under the header shows each member's face, ring and caption. A tap filters the chat to that member, and in Log follows a child in the existing panel.
- **Stop** (interrupt) is a header button while the session runs. Fork, clear, copy id and end session stay in the sheet, opened by a button labelled "Session".
- **Home** is one card per tree, grouped Needs you, Working, Idle. A tab bar reaches Machines, Skills, MCP and the new Crew screen, where a persona's avatar is shuffled, uploaded or reset.
- **From 900px** the home column stays beside an open session as a rail, Tasks and Files take a column on the right, and only Chat and Log switch. This revisits `2026-09-29-one-screen-in-the-document.md`, which named two screens side by side as its reason to revisit.
- While a keyboard takes the screen, the view tabs and the crew row step aside. Focus alone does not hide them, so a tap that moves the focus does not move the layout under the finger.

## Consequences

- An open session holds two connections: its own stream for Log and the tree stream for the crew.
- Children's text appears in the chat a message at a time.
- The pane carries about 130 KB of fonts, cached for a day.
- The subagent panel stays, reached from the crew strip in Log, not from a line's "watch" alone.

## Revisit When

- A reader needs a child's own composer.
- The tree stream carries children's live text.
