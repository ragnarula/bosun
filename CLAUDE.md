# CLAUDE.md

## Vision

Bosun sends software work to AI agents and runs them on machines you own. Work arrives from an issue tracker, from a change waiting for review, from a schedule, or from a person typing a request. Bosun finds a machine, starts an agent there, gives it the standards you have set, and reports what happened. Each agent works on its own machine, so no agent affects another.

This repository builds Bosun in Rust. The current sprint targets a single user, with no security and no scalability: Bosun runs one agent loop per session on the control plane, executes tools on the node the session works on, and the user drives sessions from a terminal client.

Current state: Remote skills shipped. Sessions run on the control plane: a per-session agent loop drives the provider API, tool calls execute on the node through the executor, and the user drives sessions from the terminal client or the web pane. Skill packages come from GitHub repositories that the operator manages in the web pane, are stored and served from the SQLite store, and load through the `skill` tool. The web pane follows the mobile-first principles in `docs/developer/web-ui-principles.md`. The sprint plan is in `docs/sprints/007-skill-packages.md`; the web pane's redesign plan is in `docs/sprints/007-web-pane-mobile.md`.

## Development

Use the `bosun-development` skill before designing a solution or writing any code. It points at the engineering principles and coding standards that govern the change.

## Communication

Use simple, direct language. Use the active voice. Avoid metaphors. Keep the English simple enough for non-native speakers. Do not dramatise.

This applies everywhere: written docs, code, comments, commit messages, and replies to the user.
