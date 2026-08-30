use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Creating,
    Running,
    WaitingForInput,
    Interrupted,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub node: String,
    pub repo_url: Option<String>,
    pub git_ref: Option<String>,
    pub dir: String,
    pub model: String,
    pub permission: Permission,
    pub state: SessionState,
    pub created_at_secs: i64,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a turn's transcript. A message carries exactly one block, so
/// tool calls, results, questions and text all append as separate messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        id: String,
        name: String,
        is_error: bool,
        content: Value,
    },
    Ask {
        message: String,
        options: Vec<String>,
        answer: Option<String>,
    },
    Summary {
        text: String,
    },
    Subagent {
        subagent_type: String,
        status: String,
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub block: Block,
}

/// Durable events replayed over SSE. `Delta` is not part of this enum; text
/// streaming is delivered live only and is not stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Message {
        message: Message,
    },
    State {
        state: SessionState,
    },
    Permission {
        permission: Permission,
    },
    ModelCall {
        model: String,
        provider: String,
        // "completion" or "compaction"; renamed away from the enum's own
        // "kind" tag, which serde forbids on a field.
        #[serde(rename = "call_kind")]
        kind: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost: Option<f64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_parses_and_serializes_snake_case() {
        let read_only: Permission = serde_json::from_str("\"read_only\"").unwrap();
        let read_write: Permission = serde_json::from_str("\"read_write\"").unwrap();
        assert_eq!(read_only, Permission::ReadOnly);
        assert_eq!(read_write, Permission::ReadWrite);

        assert_eq!(serde_json::to_string(&read_only).unwrap(), "\"read_only\"");
        assert_eq!(
            serde_json::to_string(&read_write).unwrap(),
            "\"read_write\""
        );
    }

    #[test]
    fn session_state_parses_and_serializes_snake_case() {
        let cases = [
            ("creating", SessionState::Creating),
            ("running", SessionState::Running),
            ("waiting_for_input", SessionState::WaitingForInput),
            ("interrupted", SessionState::Interrupted),
            ("stopped", SessionState::Stopped),
        ];
        for (name, state) in cases {
            let parsed: SessionState = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(parsed, state);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn role_parses_and_serializes_snake_case() {
        let user: Role = serde_json::from_str("\"user\"").unwrap();
        let assistant: Role = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(user, Role::User);
        assert_eq!(assistant, Role::Assistant);
        assert_eq!(serde_json::to_string(&assistant).unwrap(), "\"assistant\"");
    }

    fn assert_round_trips<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_value(value).unwrap();
        let decoded: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), json);
    }

    fn message(block: Block) -> Message {
        Message {
            role: Role::Assistant,
            block,
        }
    }

    #[test]
    fn block_round_trips_every_variant() {
        for block in [
            Block::Text {
                text: "hello".into(),
            },
            Block::ToolCall {
                id: "call-1".into(),
                name: "file/read".into(),
                args: serde_json::json!({"path": "src/main.rs"}),
            },
            Block::ToolResult {
                id: "call-1".into(),
                name: "file/read".into(),
                is_error: false,
                content: serde_json::json!({"text": "fn main() {}"}),
            },
            Block::Ask {
                message: "continue?".into(),
                options: vec!["yes".into(), "no".into()],
                answer: None,
            },
            Block::Summary {
                text: "summarized".into(),
            },
            Block::Subagent {
                subagent_type: "coder".into(),
                status: "done".into(),
                text: "finished the change".into(),
            },
        ] {
            assert_round_trips(&block);
        }
    }

    #[test]
    fn ask_block_round_trips_with_an_answer() {
        let block = Block::Ask {
            message: "continue?".into(),
            options: vec!["yes".into()],
            answer: Some("yes".into()),
        };
        assert_round_trips(&block);
    }

    #[test]
    fn block_uses_snake_case_kind_tags() {
        let json = serde_json::to_value(Block::ToolCall {
            id: "call-1".into(),
            name: "shell".into(),
            args: serde_json::json!({"command": "ls"}),
        })
        .unwrap();
        assert_eq!(json["kind"], "tool_call");

        let json = serde_json::to_value(Block::Ask {
            message: "continue?".into(),
            options: vec![],
            answer: None,
        })
        .unwrap();
        assert_eq!(json["kind"], "ask");
        assert_eq!(json["answer"], serde_json::Value::Null);
    }

    #[test]
    fn event_round_trips_every_variant() {
        for event in [
            Event::Message {
                message: message(Block::Text {
                    text: "hello".into(),
                }),
            },
            Event::State {
                state: SessionState::WaitingForInput,
            },
            Event::Permission {
                permission: Permission::ReadWrite,
            },
            Event::ModelCall {
                model: "claude".into(),
                provider: "anthropic".into(),
                kind: "completion".into(),
                input_tokens: Some(100),
                output_tokens: Some(50),
                cost: Some(0.001),
            },
        ] {
            assert_round_trips(&event);
        }
    }

    #[test]
    fn event_uses_snake_case_kind_tags() {
        let json = serde_json::to_value(Event::State {
            state: SessionState::Running,
        })
        .unwrap();
        assert_eq!(json["kind"], "state");
        assert_eq!(json["state"], "running");

        let json = serde_json::to_value(Event::Permission {
            permission: Permission::ReadWrite,
        })
        .unwrap();
        assert_eq!(json["kind"], "permission");
        assert_eq!(json["permission"], "read_write");

        let json = serde_json::to_value(Event::ModelCall {
            model: "claude".into(),
            provider: "anthropic".into(),
            kind: "completion".into(),
            input_tokens: None,
            output_tokens: None,
            cost: None,
        })
        .unwrap();
        assert_eq!(json["kind"], "model_call");
        assert_eq!(json["call_kind"], "completion");
        assert_eq!(json["input_tokens"], serde_json::Value::Null);
    }

    #[test]
    fn message_round_trips() {
        assert_round_trips(&message(Block::Summary {
            text: "done".into(),
        }));
    }
}
