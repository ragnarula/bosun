# Sprint 013 — Project Map

Crew View shows one session tree. A project often has several trees at once: one crew on a branch, another in a worktree of the same repository, a builder on another machine. Nothing shows them together, so two crews can change the same file for an hour before anyone sees it, and a branch's pull request state lives only on GitHub. This sprint adds the Project Map. It shows every branch that Bosun sessions work on in one repository: where each one is checked out, which crew works there, what is not committed yet, which commits are pushed, how far each branch is from main, the state of its pull request, and which files two branches both change. It updates while the crews work.

Status: **in progress**. S1 to S4 are implemented and tested.

## Confirmed decisions

- **Session folders only.** A lane is the working copy of a session that is running, waiting or interrupted. Folders with no session are not on the map. A session's child that works in its parent's folder belongs to the same lane.
- **A project is one origin.** Copies whose `remote.origin.url` normalises to the same `host/path` are one project, so the SSH and HTTPS forms match. A copy with no `origin` is a project of its own repository.
- **The control plane reads each copy through the executor.** The executor's `git_state` operation reads the branch, the commits since the fork point, what is not committed, and the main branch's newest commits. The control plane reads every copy every 5 seconds, and a copy again 0.7 seconds after a `file_write`, `edit` or `shell` call in it. There is no file watcher on the node. The model is never offered `git_state`.
- **Changes are credited by timing.** A change found while one of a copy's sessions has a call running belongs to that session and its persona. Any other change was made outside Bosun.
- **Pull requests come from GitHub.** The control plane reads each branch's newest pull request, its head's checks and its reviews once a minute, with `github_token`. Without a token, or for a repository not on GitHub, the view says why it has no pull requests.
- **Merged branches stay a day.** A branch whose head reaches the main branch, or whose pull request merges, is recorded in `merged_lanes` and shown for `merged_branch_hours`, 24 by default.
- **Branches only on GitHub are one quiet line.** The view counts the branches on `origin` that no lane has checked out.
- **A Projects tab, and a shortcut from the session.** The pane gets a Projects tab beside Sessions; an open session's header links to its project's map.

## HTTP surface

- `GET /projects`: one summary per project, with the root sessions working in it, so a session can link to its project.
- `GET /projects/{id}`: the project's view: main's newest commits, the lanes, merged branches, overlaps, pull request source and the activity feed.
- `GET /projects/{id}/events`: the view now and again on each change, as `project` frames; a `gone` frame, `{"id": …}`, ends the stream when the project's last session stops. A client that falls behind gets the newest view, never a backlog.

## CLI surface

- `bosun map [project]` draws the lanes and the newest activity and keeps them up to date. With no argument it picks the only project, or lists them.

## User stories in implementation order

- [x] **S1 — The node reads a working copy.** The executor's `git_state`: root, common directory, origin, main ref and its commits, branch, head, fork point, ahead and behind, commits with pushed or not, changed paths, files not committed with line counts, branches on `origin`.
- [x] **S2 — Copies form projects.** `ProjectHub` groups session folders by origin, names each copy as a clone, folder or worktree, finds overlaps, writes the activity feed and records merges in `merged_lanes`.
- [x] **S3 — The project routes.** `/projects`, `/projects/{id}` and `/projects/{id}/events`.
- [x] **S4 — Pull requests.** GitHub polling, checks and reviews, a merged pull request recorded once.
- [ ] **S5 — The map in the pane.** A Projects tab; on a desktop, the lanes drawn from main with their cards, and Live beside them.
- [ ] **S6 — The map on a phone.** Branches, Where and the lane screen; the session header's link to its project.
- [ ] **S7 — The terminal client.** `bosun map`.
- [ ] **S8 — The docs that own the current state say so.** CLAUDE.md, the README and an ADR for how the map reads working copies.

## Out of scope

- Folders with no Bosun session, such as a developer's own checkout.
- A file watcher on the node.
- GitLab, Forgejo and GitHub webhooks.
- Bosun creating worktrees for sessions on the same machine instead of full clones.
