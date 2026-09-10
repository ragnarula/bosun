use bosun_common::session::Block;
use bosun_common::session::ChildEventKind;
use bosun_common::session::Message;
use bosun_common::session::Role;
use bosun_common::tool::ToolSpec;
use serde_json::Value;
use serde_json::json;

use crate::provider::AskRecipient;

/// Anthropic keeps the system prompt out of the message list. An empty system
/// prompt is omitted: some hosted APIs reject an empty system field.
pub fn anthropic_messages(
    system: &str,
    messages: &[Message],
    ask_recipient: AskRecipient,
) -> Value {
    // Anthropic accepts thinking back only as a signed `thinking` block, and
    // the signature is not carried on the transcript, so a Reasoning block is
    // dropped here rather than sent in a form the API rejects.
    let messages: Vec<Value> = messages
        .iter()
        .filter(|message| !matches!(message.block, Block::Reasoning { .. }))
        .map(|message| anthropic_message(message, ask_recipient))
        .collect();
    if system.is_empty() {
        json!({ "messages": messages })
    } else {
        json!({ "system": system, "messages": messages })
    }
}

/// Anthropic names the parameters object `input_schema`.
pub fn anthropic_tools(tools: &[ToolSpec]) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.schema,
            })
        })
        .collect();
    Value::Array(tools)
}

/// OpenAI puts the system prompt in the first message. An empty system prompt
/// is omitted: hosted APIs reject a system message without content.
/// A thinking model may require its own reasoning back. DeepSeek rejects a
/// request whose in-flight turn carries an assistant message without
/// `reasoning_content`, and rejects a reasoning-only assistant message, so a
/// [`Block::Reasoning`] is not serialized on its own: it rides on each
/// assistant message the same completion produced. A completed turn needs
/// none, which is why a user message clears it. Tool results sit inside a
/// turn and serialize as role `tool`, so they leave it standing.
pub fn openai_messages(system: &str, messages: &[Message], ask_recipient: AskRecipient) -> Value {
    let mut out: Vec<Value> = Vec::new();
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    let mut thinking: Option<&str> = None;
    for message in messages {
        if let Block::Reasoning { text } = &message.block {
            thinking = Some(text);
            continue;
        }
        let mut value = openai_message(message, ask_recipient);
        match value["role"].as_str() {
            Some("assistant") => {
                if let Some(text) = thinking {
                    value["reasoning_content"] = json!(text);
                }
            }
            Some("user") => thinking = None,
            _ => {}
        }
        out.push(value);
    }
    fill_missing_thinking(&mut out);
    Value::Array(out)
}

/// The thinking a model owes back is not always thinking it produced:
/// deepseek-v4.1-flash returns completions with no `reasoning` at all, then
/// rejects the next completion of the same turn for not carrying any. The
/// field is validated for presence, not content, so a tool call left without
/// one is given a single space rather than losing the turn.
///
/// Every assistant message in the turn in flight is filled, not just the tool
/// calls: the provider rejects the request when any of them lacks the field,
/// and one completion becomes several assistant messages here — its reply and
/// one per tool call. The turn in flight is the run after the last message
/// addressed to the model as `user`; a slice with no such message has no turn
/// boundary to work from and is left alone.
fn fill_missing_thinking(out: &mut [Value]) {
    let Some(last_user) = out.iter().rposition(|value| value["role"] == "user") else {
        return;
    };
    for value in &mut out[last_user + 1..] {
        if value["role"] == "assistant" && value["reasoning_content"].is_null() {
            value["reasoning_content"] = json!(" ");
        }
    }
}

/// OpenAI wraps each function in a `function` object.
pub fn openai_tools(tools: &[ToolSpec]) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.schema,
                },
            })
        })
        .collect();
    Value::Array(tools)
}

fn anthropic_message(message: &Message, ask_recipient: AskRecipient) -> Value {
    match (&message.role, &message.block) {
        (_, Block::Reasoning { .. }) => unreachable!("a filtered reasoning block"),
        (Role::User, Block::Text { text }) => json!({ "type": "text", "text": text }),
        (
            Role::User,
            Block::ToolResult {
                id,
                is_error,
                content,
                ..
            },
        ) => json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": tool_result_text(*is_error, content),
        }),
        (Role::Assistant, Block::Text { text }) => json!({ "type": "text", "text": text }),
        (Role::Assistant, Block::ToolCall { id, name, args }) => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": args,
        }),
        (
            _,
            Block::Ask {
                message: ask,
                child_id,
                answer,
                ..
            },
        ) => json!({
            "type": "text",
            "text": ask_text(ask_recipient, child_id.as_deref(), ask, answer.as_deref())
        }),
        (_, Block::Summary { text }) => json!({ "type": "text", "text": text }),
        (
            _,
            Block::ChildEvent {
                child_id,
                kind,
                text,
                ..
            },
        ) => {
            json!({ "type": "text", "text": authored_event_text(child_id, *kind, text) })
        }
        (Role::User, Block::ToolCall { name, .. }) => {
            json!({ "type": "text", "text": format!("[tool call {name}]") })
        }
        (
            Role::Assistant,
            Block::ToolResult {
                is_error, content, ..
            },
        ) => json!({ "type": "text", "text": tool_result_text(*is_error, content) }),
    }
}

fn openai_message(message: &Message, ask_recipient: AskRecipient) -> Value {
    match (&message.role, &message.block) {
        (_, Block::Reasoning { .. }) => unreachable!("a filtered reasoning block"),
        (Role::User, Block::Text { text }) => json!({ "role": "user", "content": text }),
        (
            Role::User,
            Block::ToolResult {
                id,
                is_error,
                content,
                ..
            },
        ) => json!({
            "role": "tool",
            "tool_call_id": id,
            "content": tool_result_text(*is_error, content),
        }),
        (Role::Assistant, Block::Text { text }) => json!({ "role": "assistant", "content": text }),
        (Role::Assistant, Block::ToolCall { id, name, args }) => json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() },
            }],
        }),
        (
            _,
            Block::Ask {
                message: ask,
                child_id,
                answer,
                ..
            },
        ) => json!({
            "role": message.role.as_str(),
            "content": ask_text(ask_recipient, child_id.as_deref(), ask, answer.as_deref())
        }),
        (_, Block::Summary { text }) => {
            json!({ "role": message.role.as_str(), "content": text })
        }
        (
            _,
            Block::ChildEvent {
                child_id,
                kind,
                text,
                ..
            },
        ) => {
            json!({ "role": message.role.as_str(), "content": authored_event_text(child_id, *kind, text) })
        }
        (Role::User, Block::ToolCall { name, .. }) => {
            json!({ "role": "user", "content": format!("[tool call {name}]") })
        }
        (
            Role::Assistant,
            Block::ToolResult {
                id,
                is_error,
                content,
                ..
            },
        ) => json!({
            "role": "tool",
            "tool_call_id": id,
            "content": tool_result_text(*is_error, content),
        }),
    }
}

/// Tool result content as a provider string; failed calls carry an `Error: `
/// prefix so the model sees the failure.
fn tool_result_text(is_error: bool, content: &Value) -> String {
    let text = match content {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap(),
    };
    if is_error {
        format!("Error: {text}")
    } else {
        text
    }
}

/// A child's authored event as provider text: attributed to the child by
/// session id and kind so the parent does not read the words as its own.
fn authored_event_text(child_id: &str, kind: ChildEventKind, text: &str) -> String {
    let attribution = format!("[{} from child {child_id}]", kind.as_str());
    if text.is_empty() {
        attribution
    } else {
        format!("{attribution}\n{text}")
    }
}

/// An ask as provider text. The recipient is who the question is for: the
/// user when the thread belongs to a root session, the session's parent when
/// it belongs to a child session. A raised ask names the direct child whose
/// question it carries, so the session reading it knows which child it can
/// message to answer, deny, or cancel the question; a recorded answer is
/// included, so a later wake sees the question was resolved.
fn ask_text(
    recipient: AskRecipient,
    child_id: Option<&str>,
    ask: &str,
    answer: Option<&str>,
) -> String {
    let mut text = match child_id {
        Some(child_id) => format!(
            "[question to {}, from child {child_id}] {ask}",
            recipient.as_str()
        ),
        None => format!("[question to {}] {ask}", recipient.as_str()),
    };
    if let Some(answer) = answer {
        text.push_str(&format!("\n[user answered: {answer}]"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                block: Block::Text {
                    text: "hello".into(),
                },
            },
            Message {
                role: Role::Assistant,
                block: Block::Text { text: "hi".into() },
            },
            Message {
                role: Role::User,
                block: Block::ToolResult {
                    id: "call-1".into(),
                    name: "file_read".into(),
                    is_error: false,
                    content: json!("file content"),
                },
            },
            Message {
                role: Role::User,
                block: Block::ToolResult {
                    id: "call-2".into(),
                    name: "shell".into(),
                    is_error: true,
                    content: json!({ "stderr": "boom" }),
                },
            },
            Message {
                role: Role::Assistant,
                block: Block::ToolCall {
                    id: "call-3".into(),
                    name: "shell".into(),
                    args: json!({ "command": "ls" }),
                },
            },
            Message {
                role: Role::User,
                block: Block::Ask {
                    message: "continue?".into(),
                    options: vec!["yes".into()],
                    child_id: None,
                    answer: None,
                },
            },
            Message {
                role: Role::Assistant,
                block: Block::Summary {
                    text: "did things".into(),
                },
            },
            Message {
                role: Role::User,
                block: Block::ChildEvent {
                    child_id: "child-1".into(),
                    kind: ChildEventKind::Report,
                    text: "sub done".into(),
                    origin: None,
                },
            },
            Message {
                role: Role::User,
                block: Block::ToolCall {
                    id: "call-4".into(),
                    name: "git".into(),
                    args: json!({ "args": ["status"] }),
                },
            },
            Message {
                role: Role::Assistant,
                block: Block::ToolResult {
                    id: "call-5".into(),
                    name: "file_write".into(),
                    is_error: false,
                    content: json!({ "ok": true }),
                },
            },
        ]
    }

    fn sample_tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "shell".into(),
                description: "Run a command.".into(),
                schema: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                }),
            },
            ToolSpec {
                name: "file_read".into(),
                description: "Read a file.".into(),
                schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            },
        ]
    }

    #[test]
    fn anthropic_messages_match_the_provider_shape() {
        let expected = json!({
            "system": "You are Bosun.",
            "messages": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "hi" },
                { "type": "tool_result", "tool_use_id": "call-1", "content": "file content" },
                { "type": "tool_result", "tool_use_id": "call-2", "content": "Error: {\"stderr\":\"boom\"}" },
                { "type": "tool_use", "id": "call-3", "name": "shell", "input": { "command": "ls" } },
                { "type": "text", "text": "[question to user] continue?" },
                { "type": "text", "text": "did things" },
                { "type": "text", "text": "[report from child child-1]\nsub done" },
                { "type": "text", "text": "[tool call git]" },
                { "type": "text", "text": "{\"ok\":true}" },
            ],
        });
        assert_eq!(
            anthropic_messages("You are Bosun.", &sample_messages(), AskRecipient::User),
            expected
        );
    }

    #[test]
    fn anthropic_tools_match_the_provider_shape() {
        let expected = json!([
            {
                "name": "shell",
                "description": "Run a command.",
                "input_schema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                },
            },
            {
                "name": "file_read",
                "description": "Read a file.",
                "input_schema": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                },
            },
        ]);
        assert_eq!(anthropic_tools(&sample_tools()), expected);
    }

    #[test]
    fn openai_messages_match_the_provider_shape() {
        let expected = json!([
            { "role": "system", "content": "You are Bosun." },
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": "hi" },
            { "role": "tool", "tool_call_id": "call-1", "content": "file content" },
            { "role": "tool", "tool_call_id": "call-2", "content": "Error: {\"stderr\":\"boom\"}" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-3",
                    "type": "function",
                    "function": { "name": "shell", "arguments": "{\"command\":\"ls\"}" },
                }],
            },
            { "role": "user", "content": "[question to user] continue?" },
            { "role": "assistant", "content": "did things" },
            { "role": "user", "content": "[report from child child-1]\nsub done" },
            { "role": "user", "content": "[tool call git]" },
            { "role": "tool", "tool_call_id": "call-5", "content": "{\"ok\":true}" },
        ]);
        assert_eq!(
            openai_messages("You are Bosun.", &sample_messages(), AskRecipient::User),
            expected
        );
    }

    #[test]
    fn openai_tools_match_the_provider_shape() {
        let expected = json!([
            {
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "Run a command.",
                    "parameters": {
                        "type": "object",
                        "properties": { "command": { "type": "string" } },
                        "required": ["command"],
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "file_read",
                    "description": "Read a file.",
                    "parameters": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"],
                    },
                },
            },
        ]);
        assert_eq!(openai_tools(&sample_tools()), expected);
    }

    #[test]
    fn authored_events_render_their_kind_and_attribution() {
        let message = |kind: ChildEventKind, text: &str| Message {
            role: Role::User,
            block: Block::ChildEvent {
                child_id: "child-1".into(),
                kind,
                text: text.into(),
                origin: None,
            },
        };

        let anthropic = |message: &Message| {
            anthropic_messages("", std::slice::from_ref(message), AskRecipient::User)
        };
        assert_eq!(
            anthropic(&message(ChildEventKind::Report, "done")),
            json!({ "messages": [
                { "type": "text", "text": "[report from child child-1]\ndone" }
            ] })
        );
        assert_eq!(
            anthropic(&message(ChildEventKind::Ask, "may I push?")),
            json!({ "messages": [
                { "type": "text", "text": "[ask from child child-1]\nmay I push?" }
            ] })
        );
        assert_eq!(
            anthropic(&message(ChildEventKind::Failure, "")),
            json!({ "messages": [
                { "type": "text", "text": "[failure from child child-1]" }
            ] })
        );

        let openai = |message: &Message| {
            openai_messages("", std::slice::from_ref(message), AskRecipient::User)
        };
        assert_eq!(
            openai(&message(ChildEventKind::Report, "done")),
            json!([{
                "role": "user",
                "content": "[report from child child-1]\ndone"
            }])
        );
    }

    #[test]
    fn an_empty_system_prompt_is_omitted() {
        // A summarizer call sends no system prompt; hosted APIs reject a
        // system message without content, so the empty prompt must not appear
        // in the serialized request.
        let message = Message {
            role: Role::User,
            block: Block::Text {
                text: "hello".into(),
            },
        };
        let messages = std::slice::from_ref(&message);
        assert_eq!(
            anthropic_messages("", messages, AskRecipient::User),
            json!({ "messages": [{ "type": "text", "text": "hello" }] })
        );
        assert_eq!(
            openai_messages("", messages, AskRecipient::User),
            json!([{ "role": "user", "content": "hello" }])
        );
    }

    #[test]
    fn a_bound_ask_names_its_child_and_a_plain_ask_does_not() {
        let ask = |child_id: Option<&str>| Message {
            role: Role::Assistant,
            block: Block::Ask {
                message: "may I push?".into(),
                options: vec!["yes".into(), "no".into()],
                child_id: child_id.map(String::from),
                answer: None,
            },
        };

        let anthropic = |message: &Message| {
            anthropic_messages("", std::slice::from_ref(message), AskRecipient::User)
        };
        assert_eq!(
            anthropic(&ask(Some("child-1"))),
            json!({ "messages": [
                { "type": "text", "text": "[question to user, from child child-1] may I push?" }
            ] })
        );
        assert_eq!(
            anthropic(&ask(None)),
            json!({ "messages": [
                { "type": "text", "text": "[question to user] may I push?" }
            ] })
        );

        let openai = |message: &Message| {
            openai_messages("", std::slice::from_ref(message), AskRecipient::User)
        };
        assert_eq!(
            openai(&ask(Some("child-1"))),
            json!([{
                "role": "assistant",
                "content": "[question to user, from child child-1] may I push?"
            }])
        );
    }

    #[test]
    fn a_recorded_answer_is_visible_in_later_serialized_asks() {
        // The mechanical answer route records the user's words on the
        // surfaced Ask block, so a later wake reads the question as resolved
        // instead of dangling.
        let message = Message {
            role: Role::Assistant,
            block: Block::Ask {
                message: "may I push?".into(),
                options: vec!["yes".into()],
                child_id: Some("child-1".into()),
                answer: Some("yes, push to main".into()),
            },
        };

        let anthropic = anthropic_messages("", std::slice::from_ref(&message), AskRecipient::User);
        assert_eq!(
            anthropic,
            json!({ "messages": [{
                "type": "text",
                "text": "[question to user, from child child-1] may I push?\n[user answered: yes, push to main]"
            }] })
        );
        let openai = openai_messages("", std::slice::from_ref(&message), AskRecipient::User);
        assert_eq!(
            openai,
            json!([{
                "role": "assistant",
                "content": "[question to user, from child child-1] may I push?\n[user answered: yes, push to main]"
            }])
        );
    }

    #[test]
    fn a_childs_own_ask_is_rendered_as_a_question_to_its_parent() {
        // A child session's ask goes to its parent, never to the user, so the
        // resumed child reads its own question as addressed to the parent.
        let ask = Message {
            role: Role::Assistant,
            block: Block::Ask {
                message: "may I push?".into(),
                options: vec!["yes".into(), "no".into()],
                child_id: None,
                answer: None,
            },
        };

        let anthropic = anthropic_messages("", std::slice::from_ref(&ask), AskRecipient::Parent);
        assert_eq!(
            anthropic,
            json!({ "messages": [
                { "type": "text", "text": "[question to parent] may I push?" }
            ] })
        );
        let openai = openai_messages("", std::slice::from_ref(&ask), AskRecipient::Parent);
        assert_eq!(
            openai,
            json!([{
                "role": "assistant",
                "content": "[question to parent] may I push?"
            }])
        );
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            block: Block::Text { text: text.into() },
        }
    }

    fn thinking(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            block: Block::Reasoning { text: text.into() },
        }
    }

    fn tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            block: Block::ToolCall {
                id: id.into(),
                name: "shell".into(),
                args: json!({ "command": "ls" }),
            },
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            block: Block::ToolResult {
                id: id.into(),
                name: "shell".into(),
                is_error: false,
                content: json!("out"),
            },
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            block: Block::Text { text: text.into() },
        }
    }

    #[test]
    fn thinking_rides_on_the_assistant_messages_of_its_turn() {
        // DeepSeek rejects an in-flight turn whose assistant messages do not
        // carry the reasoning back, and rejects a reasoning-only assistant
        // message, so the block must not serialize as a message of its own.
        // A tool result is not an assistant message and carries none.
        let messages = vec![
            user("go"),
            thinking("plan it"),
            tool_call("c1"),
            tool_result("c1"),
            assistant("done"),
        ];
        let value = openai_messages("", &messages, AskRecipient::User);
        let out = value.as_array().expect("messages serialize as an array");

        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["reasoning_content"], Value::Null);
        assert_eq!(out[1]["reasoning_content"], json!("plan it"));
        assert_eq!(out[2]["role"], json!("tool"));
        assert_eq!(out[2]["reasoning_content"], Value::Null);
        // Still the same turn: the reply after the tool result needs it too.
        assert_eq!(out[3]["reasoning_content"], json!("plan it"));
    }

    #[test]
    fn an_assistant_message_in_flight_without_thinking_is_filled() {
        // deepseek-v4.1-flash returns some completions with no reasoning and
        // then rejects the next one for not carrying any, so the turn in
        // flight must always present the field.
        // One completion becomes an assistant reply and an assistant tool
        // call, and the provider rejects the turn if either lacks the field.
        let messages = vec![
            user("go"),
            assistant("reading it"),
            tool_call("c1"),
            tool_result("c1"),
        ];
        let value = openai_messages("", &messages, AskRecipient::User);
        let out = value.as_array().expect("messages serialize as an array");

        assert_eq!(out[1]["reasoning_content"], json!(" "));
        assert_eq!(out[2]["reasoning_content"], json!(" "));
        assert_eq!(
            out[3]["reasoning_content"],
            Value::Null,
            "a tool result carries none"
        );
    }

    #[test]
    fn a_completed_turn_is_not_filled() {
        let messages = vec![user("go"), assistant("done"), user("again")];
        let value = openai_messages("", &messages, AskRecipient::User);
        let out = value.as_array().expect("messages serialize as an array");

        assert_eq!(out[1]["reasoning_content"], Value::Null);
        assert_eq!(out[2]["role"], json!("user"));
    }

    #[test]
    fn a_completed_turn_carries_no_thinking() {
        // The requirement covers the turn in flight. Once the user speaks
        // again the model has discarded its thinking, and an earlier turn
        // that never had a Reasoning block must not borrow a later one.
        let messages = vec![
            user("go"),
            thinking("plan it"),
            assistant("done"),
            user("again"),
            assistant("sure"),
        ];
        let value = openai_messages("", &messages, AskRecipient::User);
        let out = value.as_array().expect("messages serialize as an array");

        assert_eq!(out.len(), 4);
        assert_eq!(out[1]["reasoning_content"], json!("plan it"));
        // out[3] closes the new turn, so it is filled rather than left bare.
        assert_eq!(out[3]["reasoning_content"], json!(" "));
    }

    #[test]
    fn a_later_completions_thinking_replaces_an_earlier_one() {
        let messages = vec![
            user("go"),
            thinking("first"),
            tool_call("c1"),
            tool_result("c1"),
            thinking("second"),
            assistant("done"),
        ];
        let value = openai_messages("", &messages, AskRecipient::User);
        let out = value.as_array().expect("messages serialize as an array");

        assert_eq!(out[1]["reasoning_content"], json!("first"));
        assert_eq!(out[3]["reasoning_content"], json!("second"));
    }

    #[test]
    fn anthropic_drops_a_reasoning_block() {
        // Anthropic accepts thinking back only as a signed `thinking` block,
        // and the transcript does not carry the signature.
        let messages = vec![user("go"), thinking("plan it"), assistant("done")];
        assert_eq!(
            anthropic_messages("", &messages, AskRecipient::User),
            json!({ "messages": [
                { "type": "text", "text": "go" },
                { "type": "text", "text": "done" },
            ] })
        );
    }
}
