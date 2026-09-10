# ADR: Reasoning is a transcript block replayed onto the turn's assistant messages

**Date:** 2026-09-10
**Author:** Raghav

## Context

Some models stream their thinking apart from their reply, and some then
require it back. DeepSeek's thinking models reject a request whose in-flight
turn contains an assistant message without a non-empty `reasoning_content`,
and reject an assistant message that carries `reasoning_content` and nothing
else. A turn that has ended needs none: once a user message follows, the
thinking behind the earlier assistant messages may be absent.

`Block` had no variant for thinking, so the agent loop discarded it. Every
session on such a model completed its first turn and failed the second.

## Decision Drivers

- The provider contract above must be satisfied for a turn of any length,
  including one completion that emits several tool calls.
- The web pane and the terminal client render the same transcript, so
  thinking must reach both through the existing message stream.
- Thinking is large. It must not be stored once per assistant message it
  covers, and must not be shown more than once for the completion that
  produced it.
- Adapters for providers with a different thinking format must not be
  forced into this one.

## Options Considered

- **A `reasoning: Option<String>` field on `Block::Text` and
  `Block::ToolCall`.** Rejected: one completion producing N tool calls
  becomes N blocks, so the same thinking is stored N times and the pane
  renders N identical panels, against `web-ui-principles.md`'s rule to show
  each piece of information once.
- **A `Block::Reasoning` serialized as its own assistant message.** Rejected:
  DeepSeek rejects a reasoning-only assistant message outright.
- **A `Block::Reasoning` replayed onto the assistant messages that follow it
  (chosen).** One block per completion, stored and rendered once, and
  attached at serialization to each assistant message of the turn.
- **Failing the turn when the model returned no thinking.** Rejected: measured
  at four crashes in five runs of a two-completion session, and the transcript
  the provider refuses is one the provider itself produced.

## Decision

`Block::Reasoning { text }` holds one completion's thinking. The agent loop
appends it before the `Block::Text` and `Block::ToolCall` blocks that
completion produced, so position carries the association.

A model does not always produce thinking it is then required to return.
deepseek-v4.1-flash returns completions with no `reasoning` at all and rejects
the next completion of the same turn for not carrying any, so replaying what
the model produced is not enough on its own. The field is validated for
presence, not content: every assistant message in the turn in flight that has
no thinking to replay is given a single space.

`openai_messages` never emits it as a message. It holds the most recent
`Reasoning` text and writes it as `reasoning_content` on each message it
serializes with role `assistant`; a message with role `user` clears it, and
role `tool` leaves it standing, because a tool result sits inside a turn
rather than ending it. `anthropic_messages` drops `Reasoning` blocks:
Anthropic accepts thinking back only in a signed `thinking` block and the
transcript does not carry the signature.

The wire field is not symmetric. DeepSeek's own API streams
`reasoning_content`; the Vercel AI Gateway streams `reasoning` for the same
model. The adapter reads either and always sends `reasoning_content`.

Thinking does not reach the delta sink, and compaction skips `Reasoning`
blocks: it is working-out, not something the session said.

## Consequences

- A thinking model completes turns of any length. Measured over eight runs of
  a tool-calling session that crashed in four of five runs beforehand: eight
  completed, none crashed, and no request was refused for the missing field.
- A filled assistant message tells the model it thought a single space. This
  is the cost of the provider validating presence rather than content, and it
  applies only where the model returned nothing to replay.
- A completion that produces no thinking, in a turn where an earlier one did,
  inherits the earlier text. The provider accepts this, and a session's model
  is fixed, so a session does not mix thinking and non-thinking completions.
- Any future `Block` consumer must handle `Reasoning`. `render_block` and both
  provider serializers reject it with `unreachable!` because their callers
  filter it; a new caller that does not filter will panic rather than send a
  shape the provider refuses.
- A provider whose thinking must be replayed with a signature or an opaque
  token is not served by this: `Block::Reasoning` holds text only.

## Revisit When

An adapter must replay thinking that carries a signature or provider token,
or a provider requires thinking from turns that have already completed.
