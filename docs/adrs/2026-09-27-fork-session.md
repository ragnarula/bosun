# ADR: A fork copies a session's conversation into a new root session

**Date:** 2026-09-27
**Author:** Raghav

## Context

A session's thread is its work: the task, the tools it ran, what they answered, the questions and the summaries. Until now a user could continue a session or start a fresh one with no history, and there was no way to try a second direction from the same point. The store holds every session's messages and tool-call rows, and a session's window is its active rows, so a second thread could start wherever the first one stands.

What a session needs to be is already decided: `2026-08-18-full-clone-per-session.md` gives every session its own clone of the recorded repository, `2026-08-30-sqlite-session-store.md` keeps one store behind one connection, `2026-08-30-session-states.md` says a session created without a prompt waits for input, `2026-09-03-agent-tree.md` says a child is supervised by its parent, and `2026-09-13-session-summaries.md` says the session list carries a summary rather than the thread.

The operator fixed the open questions on 2026-09-27: the fork is a new root session the user drives beside the original; it carries the original's persona, model, MCP servers and permission; it gets its own working copy, a fresh clone of the recorded repository and ref on the same node; the transcript's rows up to the fork are copied into it; the original is untouched and keeps no note of it; a session without a recorded repository is refused; children are not forkable, and only a session waiting for input can be forked.

## Decision Drivers

- Two lines of work must not collide. Two sessions editing one directory is exactly the collision every session's own clone exists to prevent.
- The fork's model must start where the original's stood: the same thread, in the same order, with the same results in it.
- The original must be exactly as it was. Its user may be mid-thought; a fork is a second thought, not a change to the first.
- The fork is the user's session, not the original's child: no supervision, no parent deciding, no report to author.
- A fork that cannot be a faithful copy must say so rather than approximate: an arbitrary directory cannot be copied, and a session that is still working has no fork point yet.

## Options Considered

**1. A new root session with its own clone and a copy of the thread. (chosen)**

One transaction writes the fork's row and copies the original's active messages up to the fork point, plus the tool-call rows those messages name. The node clones the recorded repository on the recorded ref. The fork waits for input, like any session created without a prompt, and the pane opens it.

**2. Share the original's working copy. (rejected)**

It is the cheaper copy — no clone, no node work — and it is what the issue's first open question asked. Two agents editing one directory collide: one writes while the other reads, a git operation moves under a running turn, and nothing in the store says which session owns a file. Every other session already starts from its own clone, so a fork that shared one would be the only exception.

**3. Make the fork a child of the original. (rejected)**

A child is supervised: it reports to its parent, its parent re-decides it, and the pane watches it rather than driving it. A fork is the user's own second line of work, run by the user, so the tree's supervision has nothing to add and would get in the way — the parent would have to be told about a session it has no interest in.

**4. Leave a note in the original's transcript. (rejected)**

It would be truthful — an original's thread could say it was forked — and the operator decided against it: the original keeps no note. Its user did nothing, and a line in its transcript would enter its model's window and its summary, describing a session it neither started nor controls. The pane is where a reader finds the fork, because the fork is a session in the list.

**5. Copy every row, archived ones included. (rejected)**

The original may have compacted: its old rows are archived, and its window is the rows that survived, closed by the summary the compaction wrote. Copying the archived rows would give the fork a longer thread than the original's model has, with a summary in the middle of it, so the fork's window would not be the original's. What a fork copies is what the original would read next.

**6. Copy a dev session's directory. (rejected)**

A dev session runs in a directory the user chose, with no recorded repository, and nothing copies an arbitrary directory: the clone path takes a repository and a ref, and the node confines a copied directory to its browse roots. The fork is refused with a message instead, which is the operator's answer for now.

## Decision

`POST /sessions/<id>/fork` returns the new session's JSON, and the pane's session actions sheet carries a `Fork session` control that opens it.

The refusals come before the node sees a command: an unknown id is 404; a child is 400, "fork a root session instead"; a session whose state is not `waiting_for_input` is 409, naming the state; a session with no recorded repository is 400, because a fork clones one. The fork's model must have a provider, and the node must be up, before the clone starts.

`Store::fork_session` writes the fork's row and copies the original's thread in one transaction: the original's active messages up to and including the fork point, each inserted as the fork's own message with its own event, and the tool-call rows those messages name. The fork point is the newest row in the original's window, so the fork's model starts from what the original's model would have read next. Nothing is written to the original.

The fork's row carries the original's node, repository, ref, model, persona, permission, allowed tools and MCP selection, and it is a root: no parent, and it owns itself. Its directory is the node's clone for the new session id, so the two sessions never share a working copy. It is created `creating`, then the control plane starts its loop and moves it to `waiting_for_input`: it carries no prompt, so nothing runs until the user speaks, exactly as a session created without one.

## Consequences

- A fork is a faithful copy of what the original could read, and the two are independent from the moment it exists: the original's rows, events and state are untouched, and it does not learn that it was forked.
- The copy's stamps are the copy's, not the original's. A message row in the store carries no timestamp — the events do — so the fork's replay shows every copied line at the time of the fork. A reader can tell the fork's own lines from the copied ones by the time only loosely; the tests compare bodies.
- A fork of a compacted session carries the rows that survived its compaction and the summary that closed the retired tail, because that is the original's window. It is faithful to the original's model and surprising to a reader who scrolls the original and sees rows the fork does not have.
- Children and dev sessions are not forkable, and nothing links a fork to its original: no parent, no note, no id on either row. Finding the fork is the session list, which is where every session is found.
- The copy is a transaction, so a fork either exists whole or not at all: no half-copied thread, and no fork row pointing at a clone that was never made.
- The node work is the clone every session does, so a fork costs one clone and one loop, and the fork's first turn is the user's own message.

## Revisit When

- A user needs to find a fork from its original, or the other way round: an id or a note, and the decision above to leave the original silent, would both have to change.
- The copy's timestamps matter: carrying the original's would mean copying its events, which the store keys by sequence rather than by message.
- Forking should work from an earlier point than now — a chosen message, not the newest row — which needs a way to pick one in the pane.
- A dev session should be forkable, which needs a way to copy an arbitrary directory or to clone one from a recorded repository.
- A child should be forkable, which is the tree's business rather than the fork's, and needs an owner for the new session.
