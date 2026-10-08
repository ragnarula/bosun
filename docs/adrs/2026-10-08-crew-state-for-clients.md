# ADR: The store records what a crew is doing in a form clients can draw

**Date:** 2026-10-08
**Author:** Pete

## Context

The web pane and the terminal client show a session tree as a crew: who is working, on what, which files changed, how far the task list has got, and which sessions need the user. Before this decision the facts behind those views were missing or scattered:

- The task list lived only in the loop's `LoopState.todos`. It was lost on a control-plane restart, and no API returned it; the only durable trace was the `todowrite` call's arguments.
- `ActivityPhase::ToolStarted` named the tool but not what it worked on, so a caption could say "running tool edit" but not which file.
- `file_write` returned `{}` and `edit` returned `{replaced: true}`. Neither said whether a file was new or how much changed, and nothing reported what a shell command changed.
- `GET /sessions` returned bare session rows. Cost was only available per session from `/model-calls`, and the current activity, a pending question and task progress were not available at all.

## Decision Drivers

- A client draws a crew from the stream and the list it already reads, without parsing tool arguments or opening each session.
- Old stored events and old clients keep working: every new field has a default, and a field with no value is not serialized.
- Bounded work per list poll and per tool call.
- Facts, not guesses: a count or a path in a result is what the executor measured.

## Options Considered

**1. Record each fact where it happens, and serve a per-session overview with the list. (chosen)**

The task list is a column on the session row with an `Event::Todos` on each change. `ToolStarted` carries a `target`. The executor's write and edit results carry counts. The loop reads the working copy's git status around a shell command. The list joins each row with an overview read from the store.

**2. Derive everything in the client from tool calls. (rejected)**

The pane and the terminal client would each parse `todowrite`, `edit` and `file_write` arguments, and a client that joined after compaction or a context clear would not find the call that wrote the current task list. Created-versus-overwritten and a shell command's effect cannot be derived from arguments at all.

**3. Watch the file system on the node. (rejected)**

An inotify watcher per working copy would see every change, including ones no agent made, would need a new protocol message from node to control plane, and could not say which call made a change when several run at once. The git status before and after a command answers the question that matters with the existing `history_read` operation.

**4. Change the shell's `Done` frame to carry the changed paths. (rejected)**

The executor, the node tunnel and the control plane's tool client each build or read `Done` and `Spilled` frames in several places. Reading git status from the loop through `history_read` needs no protocol change and works with nodes that predate it.

**5. A diff library for line counts. (rejected)**

An exact diff of a 1 MiB rewrite is quadratic in the worst case. Counting the lines after the shared start and end is one pass, exact for an edit, and an upper bound for a rewrite that changes several places.

## Decision

- `sessions.todos` holds the task list as JSON. `Store::set_todos` writes it with an `Event::Todos { at_ms, items }` in one transaction, and `Store::todos` reads it. The loop writes on each `todowrite` and reads the list back when it starts. A task item may carry `kind` (`build`, `test`, `review`, `design`, `research`, `other`) and `owner` (a child's id).
- `ActivityPhase::ToolStarted { name, target }`: `target` is the path, the first non-empty line of a command, the pattern, the URL, the skill, the persona or the child id, cut at 120 characters; None for other tools.
- `file_write` returns `{created, added, removed}`; `edit` returns `{replaced: true, added, removed}`.
- Around a `shell` call the loop runs `history_read {op: status}` before and after. The result gains `files: [{path, op}]` for each path whose porcelain status the command changed: `deleted` when the status holds `D`, `created` for `??` or `A`, `edited` otherwise. A path that left the status list is not reported, because a commit and a revert both remove one.
- `GET /sessions` and `GET /sessions/{id}` serve a `SessionView`: the session's fields, then `cost` (summed model-call cost), `activity` (the newest `Event::Activity` with its stamp), `asking` (the newest message is an `Ask` with no answer) and `tasks` (`total`, `done`, `in_progress`).

## Consequences

- The task list survives a restart, and a client sees each change on the stream.
- A crew caption and a file list come from events and results alone.
- Every shell call costs two more executor round trips, and a working copy that is not a git repository reports no files.
- A path a command changed while it was already dirty with the same status is not reported.
- A list poll reads four small queries per session. The newest-activity query walks the `(session_id, seq)` index back to the first activity row.
- Models see the new fields in tool results and in the `todowrite` schema.

## Revisit When

- A session's file changes need exact attribution between agents that write to the same working copy at the same time.
- The session list grows large enough that its poll shows in the control plane's load: serve the overview from one aggregate query.
