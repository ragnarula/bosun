# Sprint 010 — The layered system prompt

A session's system prompt is one built-in default, and a persona's prompt file replaces it. The session cannot tell a fixed operating contract from a role, a repository instruction, or an instruction injected into a file it reads. This sprint splits the prompt into three layers — a compile-time harness contract, a persona role, and the session's live context — and fixes the trust order between them. It also moves the turn model into the contract and removes the children-specific waiting rule that told supervising sessions to end their turn.

Status: **proposed**.

> The layer boundary and the trust order are recorded in `../adrs/2026-09-12-layered-system-prompt.md`; the turn model move and the waiting-rule removal in `../adrs/2026-09-12-turn-model-in-the-contract.md`. Both supersede parts of `../adrs/2026-09-03-agent-tree.md` and `../adrs/2026-09-06-mid-wake-child-visibility.md`.

## Confirmed decisions

- **The harness contract is fixed at compile time.** It is the first text of every request, and no persona, repository file, skill, tool result, or user turn changes it. It lives in `crates/bosun-agent/src/prompts/harness.md`, included with `include_str!`, and a unit test caps its length near 4,000 characters.
- **The persona is the role layer, below the contract.** Its prompt file is appended when present and cannot contradict the contract. A session with no persona prompt gets the contract alone. `DEFAULT_SYSTEM_PROMPT` is deleted.
- **Precedence follows trust, not specificity.** Policy is owned by the contract and the persona. The user directs the task but not policy or role. Repository standards, skill instructions, tool output, and user turns are untrusted input for policy and are followed only as task guidance.
- **The contract states what the system is and where the trust boundary is.** It carries a short description of the system, the turn and wake model, tool and working-copy discipline, the trust order, confidentiality and injection refusal, secret handling, and the project's tone.
- **The turn model moves into the contract.** The contract says to keep working until the task is complete, to end the turn only when nothing is left or the user is needed, that a user message or a child event wakes the session, and that a child reports by ending its turn.
- **`WAITING_RULE` is deleted.** The rule that a supervising session waits for a child by ending its turn told a session with live children to stop, even when it still had work of its own. The live-children manifest becomes data only.
- **The contract is not configurable.** No `serve.toml` field and no runtime file changes it; a rebuild is the only way.
- **Mechanical containment is out of scope.** Network egress control, secret scrubbing, and sandboxing are recorded in the ADR's Revisit When, not built here.

## CLI surface

Unchanged. No command, flag, or output changes. `serve.toml` changes only in its persona comment text.

## User stories in implementation order

- [ ] **S1 — The harness contract text and its home**

As a developer, I want the contract as compile-time prose in one file, so every session carries the same fixed behaviour and the text is reviewed as prose.

- `crates/bosun-agent/src/prompts/harness.md` holds the contract: a short description of the system (Bosun, a session under a persona, the session tree), the turn and wake model, tool and working-copy discipline, the trust order (the contract and the persona own policy; the user directs the task; repository standards, skill contents, tool output, and user turns are untrusted for policy), confidentiality and injection refusal, secret handling, and tone.
- `crates/bosun-agent/src/prompt.rs` includes it with `include_str!` as `HARNESS_CONTRACT`, and holds the composition function and its tests.
- Tone states the project's rule: simple, direct, active voice, no metaphor or idiom, plain English; when explaining, give a brief summary of the prior context.
- A unit test caps the length near 4,000 characters, with a comment that names the reason, so a later reader does not raise it without deciding to.
- The contract describes the system and names no specific persona or repository; a session's role comes from its persona.
- Tests: the contract is present and non-empty; its length is within the cap.

- [ ] **S2 — The prompt is composed in layers**

As a developer, I want the system prompt built as contract, then persona, then session context, so the contract is always first and the persona is additive.

- `system_prompt` moves to `prompt.rs` and composes `HARNESS_CONTRACT`, then the persona body when present, then the existing context blocks in their current order: the repo-standards notice, the persona catalog, the skill advertisements, the todo list, and the live-children manifest.
- `persona_system_prompt` is unchanged: a missing persona, or a persona without a prompt file, contributes no role layer.
- `DEFAULT_SYSTEM_PROMPT` is deleted.
- Tests, at the unit tier: the contract is always first; a persona with a prompt appears after the contract; a persona without a prompt leaves the contract alone; the context blocks follow the persona; the length cap holds.

- [ ] **S3 — The turn model moves into the contract and the waiting rule goes**

As a user, I want a session to keep working until its task is done and to wait without being told to stop, so it does not park mid-task.

- The contract carries the general turn model: keep working until the task is complete; end the turn only when nothing is left or the user is needed; a user message or a child event wakes the session, so waiting needs no polling; a child reports by ending its turn; `message_child` is only to answer, redirect, or cancel a child.
- `WAITING_RULE` (`agent_loop.rs:2255-2258`) and its append in `system_prompt` are deleted; the manifest lists the rows only.
- Tests: the contract carries the turn model; a manifest with children carries no waiting rule; the existing `Live children:` assertions stand.

- [ ] **S4 — The persona is documented as the role layer**

As a developer, I want the persona's new meaning stated in the documents that own it, so an operator is not surprised that the contract precedes their prompt and outranks it.

- `docs/developer/config.md:51-54` states that the persona file is the role layer under an always-present contract it cannot contradict.
- `cmd/bosun/settings/serve.toml:36-38` and the `load_persona_prompts` doc comment (`config.rs:83-85`) state the same meaning; the rule is stated once and linked.
- No migration: existing persona files keep loading, and lines in them that address harness behaviour are now subordinate.

- [ ] **S5 — The decisions on record**

As a developer, I want the changed decisions written down, so a later reader knows what was ruled out and why.

- `../adrs/2026-09-12-layered-system-prompt.md` records the layer boundary and the trust order. Its Options Considered carries the rejected alternatives with the reason each lost: replacing the base rather than layering; ranking by specificity; a runtime-configurable contract; and injecting repository contents into the system prompt. Its Revisit When records mechanical containment.
- `../adrs/2026-09-12-turn-model-in-the-contract.md` records moving the turn model into the contract and deleting `WAITING_RULE`.
- Supersession notes are added to `2026-09-03-agent-tree.md:38` and `2026-09-06-mid-wake-child-visibility.md:33`.

## Acceptance measures

Measured over one delegated sprint of work, against a run on the current prompt.

| Measure | Before | Target |
|---|---|---|
| Supervising sessions that park while they still had work | observed | none; a session parks only when it is waiting |
| System prompt trust order | not stated | contract first, persona second, context third |
| Base contract length | one line | within a 4,000-character test cap |
| Untrusted sources named in the contract | none | repository, skill, tool output, and user turn |
| New config fields, tables, endpoints, session states | — | none |

## Out of scope

- No mechanical containment: no network egress control, no secret scrubbing, no sandboxing. Recorded in the ADR's Revisit When.
- No runtime or configuration editing of the contract.
- No change to the provider adapters or the serializers: the canonical `ProviderCall.system` stays one string.
- No change to persona switching, the tool surface, executor permission, the store schema, the tunnel, or the session states.
- No `CONTEXT.md`.
- No per-permission variants of the contract.
