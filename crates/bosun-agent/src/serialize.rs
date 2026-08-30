use bosun_common::session::Block;
use bosun_common::session::Message;
use bosun_common::session::Role;
use bosun_common::tool::ToolSpec;
use serde_json::Value;
use serde_json::json;

/// Anthropic keeps the system prompt out of the message list.
pub fn anthropic_messages(system: &str, messages: &[Message]) -> Value {
    let messages: Vec<Value> = messages.iter().map(anthropic_message).collect();
    json!({ "system": system, "messages": messages })
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

/// OpenAI puts the system prompt in the first message.
pub fn openai_messages(system: &str, messages: &[Message]) -> Value {
    let mut out = vec![json!({ "role": "system", "content": system })];
    out.extend(messages.iter().map(openai_message));
    Value::Array(out)
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

fn anthropic_message(message: &Message) -> Value {
    match (&message.role, &message.block) {
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
        (_, Block::Ask { message: ask, .. }) => {
            json!({ "type": "text", "text": format!("[question to user] {ask}") })
        }
        (_, Block::Summary { text }) => json!({ "type": "text", "text": text }),
        (_, Block::Subagent { text, .. }) => json!({ "type": "text", "text": text }),
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

fn openai_message(message: &Message) -> Value {
    match (&message.role, &message.block) {
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
        (_, Block::Ask { message: ask, .. }) => {
            json!({ "role": role_str(&message.role), "content": format!("[question to user] {ask}") })
        }
        (_, Block::Summary { text }) => json!({ "role": role_str(&message.role), "content": text }),
        (_, Block::Subagent { text, .. }) => {
            json!({ "role": role_str(&message.role), "content": text })
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

fn role_str(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
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
                    name: "file/read".into(),
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
                block: Block::Subagent {
                    subagent_type: "coder".into(),
                    status: "done".into(),
                    text: "sub done".into(),
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
                    name: "file/write".into(),
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
                name: "file/read".into(),
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
                { "type": "text", "text": "sub done" },
                { "type": "text", "text": "[tool call git]" },
                { "type": "text", "text": "{\"ok\":true}" },
            ],
        });
        assert_eq!(
            anthropic_messages("You are Bosun.", &sample_messages()),
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
                "name": "file/read",
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
            { "role": "user", "content": "sub done" },
            { "role": "user", "content": "[tool call git]" },
            { "role": "tool", "tool_call_id": "call-5", "content": "{\"ok\":true}" },
        ]);
        assert_eq!(
            openai_messages("You are Bosun.", &sample_messages()),
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
                    "name": "file/read",
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
}
