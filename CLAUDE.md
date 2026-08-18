# CLAUDE.md

## Vision

Bosun sends software work to AI agents and runs them on machines you own. Work arrives from an issue tracker, from a change waiting for review, from a schedule, or from a person typing a request. Bosun finds a machine, starts an agent there, gives it the standards you have set, and reports what happened. Each agent works on its own machine, so no agent affects another.

This repository builds Bosun in Rust. The current sprint targets a single user, with no security and no scalability: Bosun spawns `opencode serve` sessions on machines the user names, and the user drives them with the opencode client through a control-plane proxy.

Current state: setup, heading to a running MVP. The sprint plan is in `docs/sprints/001-setup.md`.

## Development

Use the `bosun-development` skill before designing a solution or writing any code. It points at the engineering principles and coding standards that govern the change.

## Communication

Use simple, direct language. Use the active voice. Avoid metaphors. Keep the English simple enough for non-native speakers. Do not dramatise.

This applies everywhere: written docs, code, comments, commit messages, and replies to the user.
