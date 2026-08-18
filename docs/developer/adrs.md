# Writing ADRs

An ADR records why a decision was made, when that decision constrains later work. Write one when a choice creates a long-term constraint, or when alternatives were meaningfully considered and a later reader will need to know why one won. Bug fixes, refactors and minor enhancements do not warrant one.

An ADR must be readable without surrounding context. It describes the architecture it decides about, not the circumstances that prompted it.

There is no status field. A merged ADR is an accepted one.

## Structure

```markdown
# ADR: <what was decided>

**Date:** YYYY-MM-DD
**Author:** <name>

## Context
## Decision Drivers
## Options Considered
## Decision
## Consequences
## Revisit When
```

- **Context** — what the decision is about, and the constraints fixed going in.
- **Decision Drivers** — what the choice had to satisfy.
- **Options Considered** — each option, the chosen one marked, and for every rejected one the reason it lost. Often the longest section: it is the only record of what was ruled out, and the part a later reader needs most.
- **Decision** — what is now true, in enough detail to recognise in the code.
- **Consequences** — what this buys and what it costs. State the bad ones.
- **Revisit When** — the conditions that would reopen this. Without them an ADR reads as permanent.

## Language

Write as though the decision is already in force, because once it merges it is.

- **Name the real thing.** `lcs_auth_vessel_assignment_properties`, not "the properties table". The reader has to find it in the code.
- **Give every rejected option a real reason.** A strawman teaches nothing, and invites the same option back.
- **State the costs.** An ADR with no downside was not a decision.
- **Give conditions, not dates.** "When a vessel can hold access transitively", not "revisit in six months".
- **Leave the process out.** Who proposed it, what was discussed, which ticket or PR carried it — none of that is architecture. Git and the issue tracker hold it.

Short, active voice, literal. No metaphors, idioms or slang. Do not dramatise.

## Filing

`docs/adrs/YYYY-MM-DD-kebab-case-title.md`. The date prefix sorts them, so there is no index to maintain.
