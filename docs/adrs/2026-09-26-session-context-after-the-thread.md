# ADR: Context that changes during a session follows the thread, not the system prompt

**Date:** 2026-09-26
**Author:** Raghav

## Context

Every request the agent loop sends starts with the system prompt, then the thread. `2026-09-12-layered-system-prompt.md` put all of the session's context in the system prompt: the repo-standards notice, the persona catalog, the skill advertisements, the todo list, and the live-children manifest. `2026-09-06-mid-wake-child-visibility.md` rebuilds the manifest before every turn, so it reports each child's current state and latest authored message.

Providers cache a request's prefix and bill the cached part at a fraction of the price. DeepSeek, which serves Bosun's models through the Vercel AI Gateway, bills a cache read far below a fresh input token: sending the same 4,232-token prompt twice, the second request cost about an eighteenth of the first. A cached request also reaches its first token sooner. The cache holds only while the start of the request stays the same: from the first token that differs, the provider reads the rest at full price.

The todo list and the manifest change often. A todo update, a child changing state, or a child authoring a report changes the system prompt, so the next request misses the cache on everything after the harness contract and the persona role. A supervising session changes the manifest on almost every turn. Measured on one architect session with eleven children over a working day: 95 of 142 requests missed the cache after the first 4,000 or so tokens, and 22.6M of its 29.4M input tokens were billed at full price. Sessions without children stayed at 95 to 97 percent cached.

## Decision Drivers

- The prefix of a request must stay the same from one request to the next while a session runs, so the provider can cache the thread.
- The model must still see the current todo list and the current manifest on every request, as the mid-wake visibility decision requires.
- The turn structure the provider sees must not change. A user message ends the turn in flight, and DeepSeek then stops expecting that turn's reasoning (`2026-09-10-reasoning-replayed-onto-assistant-messages.md`).
- The fixed harness contract stays within its 4,000-character cap.

## Options Considered

- **Keep the context in the system prompt.** Rejected: it is the cause of the misses above, and the cost grows with the length of the thread.
- **Store the context as a message in the thread when it changes.** Rejected: the thread would fill with old copies of the todo list and the manifest, compaction would summarize them, and the model would have to tell the current copy from stale ones.
- **Send the context as a separate user message at the end of the request.** Rejected: a user message after a tool result ends the turn in flight, which changes how the provider treats the turn's reasoning, and a message the user never wrote reads as a new instruction.
- **Send the context as a second system message at the end.** Rejected: some chat templates move every system message to the start of the prompt, which puts the change back at the front.
- **Append the context to the last message of the request (chosen).** The thread before the last message is serialized exactly as before, and no message or turn is added.

## Decision

`system_prompt` in `crates/bosun-agent/src/prompt.rs` holds only what stays the same for the whole session: the harness contract, the persona role, the repo-standards notice, the persona catalog, and the skill advertisements. `session_context` in the same file builds a separate block from the todo list and the live-children manifest, or returns `None` when there is neither. The block opens with a `[session context]` heading that says the harness wrote it for this request, that it is not part of the message it follows, and that it replaces any earlier session context.

The agent loop builds the block for every turn and passes it on `ProviderCall.session_context`. Out-of-band calls, compaction and session summaries, pass none. `append_openai_session_context` in `crates/bosun-agent/src/serialize.rs` joins the block to the content of the last user message or tool result in the request. That is normally the last message. After compaction the session's summary, an assistant message, ends the thread, and the block joins the input message before it; compaction has already changed the start of the thread, so no cached prefix is lost. The block becomes a user message of its own only when the request holds no user message or tool result at all. `append_anthropic_session_context` appends it as a final text block. The block is never stored.

Each model call also records how many of its input tokens the provider read from its cache: `StreamEvent::Stop.cached_input_tokens`, the `cached_input_tokens` column of `model_calls`, and the same field on the `model_call` event, which the web pane and the terminal client show beside the input count. `None` means the provider did not report a count.

## Consequences

- A change to the todo list or the manifest no longer invalidates the cached thread. The cache now ends at the last message of the previous request, because that message carried the previous block and no longer does.
- A request re-reads its last message at full price every time. A large tool result as the last message costs more than a short one.
- The block sits inside a message the harness did not write. A tool result or file can contain text that imitates the heading. The block is data about the session, not policy, so an imitation can mislead the model about todos or children but cannot change the contract.
- Earlier requests of a session that is running when this ships were cached with the old layout, so its first request after the upgrade misses the cache once.
- The cache count is what the provider reports. A provider that reports no count shows none, which is not the same as zero.
- `model_call_cost` prices cached input tokens as ordinary input, so the recorded cost of a mostly cached session is an upper bound. A cached-input price in the model config would fix that; it is not part of this decision.

## Revisit When

- A provider supports explicit cache breakpoints that Bosun uses, which could make the placement of the block irrelevant.
- The last message of a typical request grows large enough that re-reading it costs more than the misses this avoids.
- The model starts to treat the block as part of the message it is joined to, for example by answering a tool result's session context as if the tool had sent it.
