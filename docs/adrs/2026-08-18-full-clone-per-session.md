# ADR: Full clone per session, node's own git credentials

**Date:** 2026-08-18
**Author:** Raghav

## Context

The architecture says a workspace is a git worktree created from a shared bare clone that the node maintains. The MVP needs the node to fetch a repo before it can start an agent in it.

## Decision Drivers

- Least node-side machinery for the first slice.
- A fresh checkout must not share mutable state with another session.
- No object-store or ref maintenance on the node.

## Options Considered

- **Bare clone plus git worktrees (architecture).** Shared object store, cheap checkouts. Rejected: the node must manage a repo cache, its refs, and worktree bookkeeping — real code for a benefit the MVP does not need.
- **Full clone per session (chosen).** The node runs `git clone <url> <work_dir>/<session_id>`, with `--branch <ref>` when a ref is given, using whatever git credentials the node already has. Deleting the directory removes the session's files.

## Decision

Each session gets a full clone into `work_dir/<session_id>`. The node uses its own git credentials (SSH agent, credential helper, deploy keys). Bosun never holds a git secret.

## Consequences

- Clone cost repeats per session, and disk use grows with session count.
- Sessions never share objects, so one session's checkout cannot corrupt another's.

## Revisit When

Clone time or disk use on a node becomes the constraint; a bare-clone cache can then be added under the same spawn interface.
