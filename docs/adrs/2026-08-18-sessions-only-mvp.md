# ADR: Sessions-only MVP, no task machinery

**Date:** 2026-08-18
**Author:** Raghav

## Context

The product vision describes a task dispatch system: work arrives from a tracker, a review, a schedule, or a phone; a queue routes it to nodes; tasks carry done-criteria, budgets, classes, and a state machine. The architecture names the task contract as the fixed interface that everything else sits behind.

Development starts with one person and a small set of machines they own. The first goal is a working slice of the control-plane/node split, not the whole vision.

## Decision Drivers

- The first slice must prove that a control plane can start an agent on a named machine and that the person can drive it from one place.
- One person must be able to run everything with config files and no extra services.
- No authentication, multi-tenancy, cost ceilings, or capacity scheduling in this phase.

## Options Considered

- **Full task dispatch (architecture).** Task contract, queue, state machine, budgets, done-criteria, events. Rejected: it forces decisions about done-criteria and budgets before anyone has driven a remote agent end to end, and it doubles the first-slice surface.
- **Sessions-only (chosen).** The unit of work is a session: a clone of a repo on a named node plus a running `opencode serve`. The person creates opencode sessions inside the opencode client and drives them through a control-plane proxy.

## Decision

For the current phase, Bosun spawns and stops opencode server sessions on named nodes. The person creates and drives opencode sessions inside the client. There is no task queue, no task state machine, no budget, and no done-criteria.

## Consequences

- Bosun does not yet deliver the vision's automated value (triage overnight, review on demand). A person still starts every piece of work.
- The node/control split, heartbeat registration, and the proxy path survive into the full system.
- The task contract can be added later on top of sessions without reworking the split.

## Revisit When

A person has driven sessions end to end, and automated work becomes the next thing worth building.
