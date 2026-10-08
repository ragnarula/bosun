# Sprint 012 — Crew View

An open session is one scrolling transcript today, on the phone and in the terminal: every row has a clock time, every model call a cost line, every tool call and result a row of its own. The session's controls sit behind one menu, a child appears as `[child 3f2a… report]`, and nothing says at a glance who is working or what changed. This sprint shows the crew instead. Each agent in a session tree is a crew member with a robot avatar and its persona's signal flag; the session opens on a group chat of the whole tree, with views for the task list, the files the crew changed, and the full log. Home becomes one card per tree, saying who is doing what to which file. The pane and the terminal client both move to Bosun Signal, the design language this sprint adds.

Status: **in progress**.

## Confirmed decisions

- **The crew is the tree.** A session view draws the root and every child under it: one chat, one task list, one file list. A child is not opened on its own; tapping a crew member filters the chat and the log to that member.
- **Four views: Chat, Tasks, Files, Log.** Chat opens first and the pane remembers the last view. Log is today's transcript for the selected crew member.
- **One stream per tree.** `GET /sessions/{id}/tree-events` replays and follows every session that `owner_id` names as `id`, each frame carrying its `session_id`. The pane opens one connection for a whole crew.
- **The chat is built from what the store holds.** A parent's `spawn` and `message_child` calls are its messages to a child; a child's own text is its posts. A child's user-role text is never drawn in the chat, because it is always its parent's instructions or a routed answer, which the parent's call or the answered ask already shows. No sender field is added to messages.
- **Tasks are durable.** The session row holds its task list, `todowrite` writes it with an `Event::Todos`, and a restarted loop reads it back. A task may carry its kind of work and its owner.
- **Activity names its target.** `ToolStarted` carries the file, command, pattern, URL, skill or child the call works on, so a crew caption reads "editing winsw.ts" from the event alone.
- **File results carry facts.** `file_write` says whether it created the file, and `file_write` and `edit` return the lines added and removed. A shell run in a git working copy reports the paths whose status it changed.
- **The session list carries the crew's state.** Each session in `GET /sessions` adds its cost so far, its newest activity, whether a question waits, and its task counts.
- **Identity is a flag and a robot.** A persona's signal flag comes from its name; a robot avatar is drawn in the browser from a seed: the persona's stored seed for a root, the session id for a child. An uploaded picture replaces the robot for every session of that persona. AI-drawn avatars are out of scope.
- **Bosun Signal is the one design language.** Tokens, three typefaces served by the control plane, flags, avatars and icons drawn by the pane's own module, Night and Day themes. Nothing loads from a font host or a CDN.
- **Three columns from 900px.** Sessions, the open session, and its tasks and files, side by side.

## CLI surface

- `bosun list` groups sessions under Needs you, Working and Idle, shows each tree's crew as flag tags, its summary, what is happening now and its task progress.
- `bosun open` adds a crew line under the status line and four views on the keys `1` to `4`: Chat, Tasks, Files, Log. Log is today's view. From about 140 columns, Tasks and Files take a right-hand column.

## User stories in implementation order

- [ ] **S1 — One stream for a tree.** `tree-events` with replay, tail paging, polling and the root's live deltas; `tree-history` for read-back.
- [ ] **S2 — Tasks survive a restart.** `sessions.todos`, `Store::set_todos`, `Event::Todos`, the loop rehydrates; `todowrite` items take an optional `kind` and `owner`.
- [ ] **S3 — Activity names its target.** `ToolStarted.target`, filled at every call site from the call's arguments.
- [ ] **S4 — File results carry facts.** Created and line counts from `file_write` and `edit`; shell runs report the paths whose git status changed.
- [ ] **S5 — The session list carries the crew's state.** Cost, newest activity, pending question and task counts on each listed session.
- [ ] **S6 — Personas have avatars.** A stored seed and an optional uploaded picture per persona; `PUT`/`DELETE /personas/{name}/avatar`, `GET` to serve it, `POST /personas/{name}/avatar/shuffle`.
- [ ] **S7 — Bosun Signal in the pane.** Tokens, the three typefaces as embedded woff2, the flag, avatar and icon module, Night and Day.
- [ ] **S8 — The session view.** Header with Stop in sight, crew strip, Chat, Tasks, Files, Log, the Session sheet; the subagent panel goes.
- [ ] **S9 — Home and the desktop layout.** Session cards grouped by Needs you, Working, Idle; three columns from 900px; Machines, Skills, MCP and a Crew screen for avatars in the same language.
- [x] **S10 — The terminal client.** `bosun list` groups and flag tags; `bosun open` crew line and views 1 to 4.
- [ ] **S11 — The docs that own the current state say so.** CLAUDE.md, README, the ADRs this sprint changes.

## Out of scope

- AI-drawn avatars.
- Keeping a tree after it is stopped: the store still removes it, so Home's Idle group holds sessions that are waiting or interrupted.
- Push notifications when a session needs you.
- A sender field on messages.
