# Documentation

**The code is the source of truth for what the system does.** Documentation exists for what the code cannot state: the rules to follow while writing it, the procedures around it, and why a decision was made.

File a document by which of those it is:

- `docs/developer/` — a rule to follow while writing code.
- `docs/developer/workflows/` — a procedure a contributor runs.
- `docs/adrs/` — why a decision was made, when it constrains later work. See [adrs.md](./adrs.md).

## Do not describe how a subsystem works

A document that explains the mechanics of a subsystem competes with the code and loses. The code moves; the description does not, and nothing signals the gap. It also misses the part that has no other home — the reasoning. Knowing that a queue coalesces by key is available from the code. Knowing why it coalesces rather than queues, and what was given up, is not.

Write an ADR instead. It captures the decision, it is dated, and a reader treats it as a record of that moment rather than as current truth.

This is the same rule [commenting.md](./commenting.md) applies to comments, one level up: explain the why, because the what is already written.

## Do not document what only CI performs

Nobody runs those procedures by hand, and a second description drifts from the workflow file that actually does the work.

## State a rule once

Put it in the document that owns it, and link from anywhere else that needs it. Two copies of a rule become two different rules.
