# ADR: The control plane reads each session's working copy for the project map

**Date:** 2026-10-08
**Author:** Pete

## Context

The project map shows every branch that Bosun sessions work on in one repository: where each is checked out, what is not committed, which commits are pushed, how far each branch is from main, its pull request, and the files two branches both change. The facts live in each session's working copy on its node. A session's folder is `sessions.node` and `sessions.dir`; the control plane reaches the node only through the tool tunnel, by session id, and the executor already runs read-only commands in the session's folder for tools such as `repo_standards`.

Copies of one repository show up under different `origin` URLs: `git@github.com:o/r.git` on one machine, `https://github.com/o/r` on another, and a local path for a bare repository on the homelab.

Changes reach a working copy from the session's own tool calls, from a person editing the folder, and from other tools on the machine.

## Decision Drivers

- A change made outside Bosun shows on the map, not only an agent's edits.
- No new channel between the node and the control plane, and no new process on the node.
- An agent's edit shows within about a second.
- A copy that cannot be read, because its node is down or slow, does not hold up the others.
- The same repository is one project whatever form its `origin` takes.

## Options Considered

**1. The control plane polls each copy through the executor's `git_state`. (chosen)**

A loop in `bosun_control::projects` lists the sessions, reads every copy every 5 seconds, and reads a copy again 0.7 seconds after a `file_write`, `edit` or `shell` call in it finishes. The executor's `git_state` runs `git` read commands in the session's folder and answers a `GitState`. The tunnel and the executor already exist, so the node gains one operation.

**2. A file watcher on the node that pushes a git state event. (rejected)**

It shows an outside edit at once rather than within 5 seconds, and reads only when something changes. It needs a watcher per copy that respects `.gitignore`, a push channel from the node that the tunnel does not have, and a way to tell the node which folders to watch as sessions come and go. The 5-second delay for an outside edit is acceptable for a map read by a person.

**3. Read only what the session's tool results already report. (rejected)**

`file_write`, `edit` and `shell` results already say what they changed. They miss every change made outside Bosun, and they say nothing about commits, pushes, the fork point or the remote's branches.

## Decision

- The executor answers `git_state` with a `bosun_common::project::GitState`, or `null` outside a git working copy. It is not a model tool: the control plane calls it through `tools::call_executor`, and no model is offered it.
- `ProjectHub` keeps one copy per `node:dir` of every session that is not `creating` or `stopped`. A copy whose sessions all stop leaves the map, and a project with no copies left ends its stream with a `gone` frame.
- A project is one normalised origin: `host/path` without user, port, scheme or `.git`, so the SSH and HTTPS forms match. A copy with no `origin` is a project of its own repository. The project id is the FNV-1a hash of that string.
- Two copies with the same git common directory are worktrees of one repository; the lane says so.
- The feed is built from the difference between two reads of a copy: files created, edited or deleted, commits, pushes and branch switches. A change found while one of the copy's sessions has a tool call running is credited to that session; any other change is credited to nobody, and the clients call it yours.
- Overlaps are the paths both lanes changed since their fork point, committed or not.
- A copy that does not answer within 15 seconds is skipped until the next read.

## Consequences

- An edit made outside Bosun shows within 5 seconds; an agent's edit within about a second.
- The control plane runs a set of `git` commands per copy every 5 seconds, even when nothing changed. On a large repository `git status --untracked-files=all` is the costly one.
- Credit by timing can be wrong: an outside edit made while an agent's call runs in the same folder is credited to the agent.
- The feed lives in memory. A restart of the control plane starts it empty; merged branches survive because `merged_lanes` records them.
- Folders that no session uses are not on the map.

## Revisit When

- The map should show folders with no session, such as a developer's own checkout: that needs the node to know which folders to read, which a watcher could share.
- Polling cost shows in the control plane's or the node's load, for example with many sessions or a very large repository.
- Credit needs to be exact, for example to bill or audit who changed what.
