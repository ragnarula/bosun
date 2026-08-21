# ADR: Dev sessions browse a node's directories instead of naming a path

**Date:** 2026-08-20
**Author:** Raghav

## Context

A dev session runs `opencode serve` in an existing directory on a node, with no clone. The person who starts it sits at a machine that is not the node and cannot see the node's filesystem. Naming a raw path to spawn into requires knowing the node's disk layout, which the person does not.

## Decision Drivers

- The person never names a directory path on a node they cannot see.
- Cloning onto a node stays a separate operation from running in an existing directory.
- A dev session preserves uncommitted work, so stopping it must not delete the directory.
- The session record must distinguish a clone from a dev session, and survive node restarts.
- Bosun injects no opencode config, so nothing it writes can land in the person's checkout.

## Options Considered

- **An optional `dir` field on the clone request.** One operation with two meanings, and the inputs can be invalid together (`dir` plus a repo URL). Rejected.
- **A separate dev operation, with the directory discovered by browsing.** The node exposes a read-only directory listing under configured roots, surfaced through the control plane; the client walks it with an interactive single-level selector. Chosen.
- **Browsing the node over SSH.** Rejected: Bosun exists so the person never needs direct machine access, and nodes are reachable only through the control plane.

## Decision

`bosun clone` clones a repository into `work_dir/<session_id>` and the directory is deleted on stop. `bosun dev` starts a session in an existing directory and the directory survives stop.

The node holds `browse_roots` in its config. It serves `GET /dirs` (no path lists the roots; a path lists directories below it), refusing paths outside the roots. The control plane relays this as `GET /nodes/<name>/dirs`. The client walks the tree with a single-level fuzzy selector (`dialoguer`), with a `..` entry to ascend and a "spawn here" entry once a directory is open.

A dev session starts `opencode serve` in the chosen directory, writes nothing into it, and records `reapable: false`. `PersistedSession` stores `dir` and `reapable` with serde defaults, so a node's existing `state.json` still parses. `bosun list` shows the repository URL for clones and the directory for dev sessions.

## Consequences

- Uncommitted changes in a dev directory survive the session and are preserved.
- A node with no `browse_roots` configured disables `bosun dev` on that node, with a clear error.
- A dev directory outside every root is refused, and a symlink pointing out of a root is refused after canonicalisation.
- Dev sessions cannot be started non-interactively; there is no `--dir` form yet.

## Revisit When

Multi-user access needs per-user browse roots, or scripts need to start a dev session non-interactively by path.
