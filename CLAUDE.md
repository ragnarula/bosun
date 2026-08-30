# CLAUDE.md

## Vision

Bosun sends software work to AI agents and runs them on machines you own. Work arrives from an issue tracker, from a change waiting for review, from a schedule, or from a person typing a request. Bosun finds a machine, starts an agent there, gives it the standards you have set, and reports what happened. Each agent works on its own machine, so no agent affects another.

This repository builds Bosun in Rust. The current sprint targets a single user, with no security and no scalability: Bosun runs one agent loop per session on the control plane, executes tools on the node the session works on, and the user drives sessions from a terminal client.

Current state: MVP complete. Sessions run on the control plane: a per-session agent loop drives the provider API, tool calls execute on the node through the executor, and the user drives sessions from the terminal client or the web pane. The sprint plan is in `docs/sprints/002-agent-executor.md`.

## Development

Use the `bosun-development` skill before designing a solution or writing any code. It points at the engineering principles and coding standards that govern the change.

## Communication

Use simple, direct language. Use the active voice. Avoid metaphors. Keep the English simple enough for non-native speakers. Do not dramatise.

This applies everywhere: written docs, code, comments, commit messages, and replies to the user.
