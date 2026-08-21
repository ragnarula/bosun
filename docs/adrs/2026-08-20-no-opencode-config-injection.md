# ADR: No opencode config injection

**Date:** 2026-08-20
**Author:** Raghav

## Context

The ADR `2026-08-18-opencode-template-injected-at-spawn` had the control plane read a single `opencode.json` template and the node write it into each session's clone directory before starting `opencode serve`. The template carried the provider key.

Dev sessions changed the picture. A dev session runs in an existing directory that survives `stop`, so an injected `opencode.json` would stay in the person's checkout with its provider key, and could be committed. Keeping the key only in throwaway clones left two behaviors for the same mechanism.

## Decision Drivers

- Bosun must not write a provider key into a directory it does not own and does not delete.
- A spawned session must work without per-session setup.
- The key is a provisioning concern, like the `opencode` binary itself.

## Options Considered

- **Inject for clones, not dev sessions.** Two behaviors for one mechanism, and the key still travels from control plane to node and lands on disk. Rejected.
- **Write the template to a session-scoped file and point opencode at it.** Depends on opencode honoring an explicit config override, which was never verified, and adds machinery no session needs when the node owns its own config. Rejected.
- **Strip injection entirely.** `opencode serve` resolves its own config: a project `opencode.json` if one exists, then the node user's global config. Chosen.

## Decision

Bosun does not read, carry, or write an opencode config. The `template_path` field is gone from `ControlConfig`, `opencode_config` is gone from the clone request, and the node no longer writes `opencode.json`. This supersedes `2026-08-18-opencode-template-injected-at-spawn`.

## Consequences

- Setting up a node means installing `opencode` and configuring its provider in the node's own opencode config. Both are provisioning.
- Changing provider is a change to each node or to the node image, not one edit on the control plane.
- Bosun handles no provider credential anywhere.

## Revisit When

Bosun runs on machines the operator does not provision, or a hosted Bosun must supply provider configuration to nodes it does not own.
