# ADR: Canonical tool surface

**Date:** 2026-08-30
**Author:** Raghav

## Context

The MVP had no tools: opencode owned the tool set and the client drove it through a proxy. Sprint 002's agent loop calls tools itself, so Bosun must choose the tool surface, its schemas, and which tools run where. Bosun also runs with an explicit permission mode per session.

## Decision Drivers

- The same tools must serialize to Anthropic and OpenAI-compatible providers, so the loop keeps one canonical list and each provider adapter translates it.
- Mutating tools must be kept out of a read-only model's schema and refused at the executor, so read-only is enforced twice.
- Everything that inspects or edits the working copy must run on the node; everything that talks to the user or the loop must run on the control plane.

## Options Considered

- **Mirror opencode's tool set.** Rejected: it is large, versioned by opencode, and mostly for its client UI. Bosun's agent needs the subset a coding agent actually uses.
- **One small canonical list (chosen).** `shell`, `file/read`, `file/write`, `edit`, `grep`, `glob`, `ask`, `todowrite`, `git`, `webfetch`, `skill`, `spawn_subagent`.

## Decision

The tool surface has one canonical set of JSON Schemas in `bosun-common`. Node-side tools run on the executor: `shell` (streamed, cancellable), `file/read`, `file/write`, `edit` (single-replacement), `grep`, `glob`, `git` (read commands plus `commit` and `add`; no `push`), and `webfetch` (fetches a URL into the session context). Control-plane tools run in the loop: `ask` (ends the turn with options for the user), `todowrite` (maintains the session todo list), `skill` (loads a skill's instructions on demand), and `spawn_subagent` (runs a nested loop with a configured subagent type). `websearch` is out of scope.

Each provider adapter exposes the same list; a read-only session removes `shell`, `file/write`, and `edit` from the schema and the executor refuses them, plus mutating `git` verbs, regardless. `webfetch` runs on the node because the node may be the machine with network reach to a private resource.

## Consequences

- Adding a provider means writing one adapter over a fixed schema, not reshaping the tools.
- The executor is the only component that touches the working copy, so path confinement is enforced in one place.
- The loop's `ask` and `todowrite` keep the user in the loop without a node round-trip.
- Working-copy skills are discovered and read through the executor (`/tool/skills`, `/tool/skill/read`), so they work even when the control plane and node are different machines; injected control-plane skills are read locally.

## Revisit When

A tool must run on the control plane against the working copy, a second node-side executor appears, or `websearch` is worth its cost.
