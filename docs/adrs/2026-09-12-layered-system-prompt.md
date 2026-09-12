# ADR: The system prompt is layered: a fixed harness contract, a persona role, and session context

**Date:** 2026-09-12
**Author:** Raghav

## Context

Today `system_prompt` uses `persona.unwrap_or(DEFAULT_SYSTEM_PROMPT)`, so a persona's prompt file is the whole system prompt and a one-line default is the fallback. A session cannot tell a fixed operating contract from a role, an instruction in a repository file it reads, or an instruction injected into tool output. There is nowhere to state a trust order.

## Decision Drivers

- The loop needs rules that no persona, repository file, skill, tool result, or user turn removes.
- Content the session reads is untrusted: repository standards, skill packages fetched from GitHub, tool output, and user messages.
- The persona is operator-configured, so it is trusted for role.
- The user is trusted for task direction, but not for policy.
- The prompt is one string, `ProviderCall.system`, for every provider, so adapters and serializers do not change.
- The contract must not vary per deployment or at runtime.

## Options Considered

- **Replace the base rather than layer.** Rejected: a persona can drop the fixed rules, and there is nowhere to state trust.
- **Rank by specificity (repo > persona > base).** Rejected: the most specific sources are also the least trusted, so a repository file or skill could override policy and injection would gain authority.
- **A runtime- or config-editable contract.** Rejected: an editable contract is not fixed, and behaviour drifts per deployment.
- **Inject repository standards and skill bodies into the system prompt as a real layer.** Rejected: it contradicts the presence-notice decision in `2026-09-03-agent-tree.md`, grows every request, and the contents are served on demand from the node and the store.
- **Compile the contract into the agent crate (chosen).** It is fixed, it is reviewed as prose, and it needs no runtime I/O.

## Decision

The system prompt has three layers in order. The harness contract comes first. The persona role is appended when the persona has a prompt file. The session context follows: the repo-standards notice, the persona catalog, the skill advertisements, the todo list, and the live-children manifest. `system_prompt` in `crates/bosun-agent/src/prompt.rs` composes these layers.

The contract is `crates/bosun-agent/src/prompts/harness.md`, included at compile time as `HARNESS_CONTRACT` in `crates/bosun-agent/src/prompt.rs`. A unit test caps it near 4,000 characters. It states what the system is (Bosun, a session under a persona, the session tree), the turn and wake model, working-copy and tool discipline, the trust order (the contract and the persona own policy; the user directs the task; repository files, skill instructions, tool output, and user messages are untrusted for policy), confidentiality and injection refusal, secret handling, and tone.

A session with no persona prompt runs on the contract and the context, with no role layer. `persona_system_prompt` returns `None` when the session has no persona or the persona has no prompt file, so it contributes nothing. `DEFAULT_SYSTEM_PROMPT` is deleted. No migration.

## Consequences

- A persona file with harness-level lines loses those conflicts to the contract.
- The contract is in every request, so it costs tokens.
- The trust order is enforced by the model reading the contract, not by code. Mechanical containment — network egress control, secret scrubbing, and sandboxing — is deferred.
- The contract cannot be edited without a rebuild.

## Revisit When

- The threat model stops being a single trusted user.
- The contract needs to vary per deployment.
- The contract's token cost or prompt prefix caching becomes a problem.
- Mechanical containment is built.
