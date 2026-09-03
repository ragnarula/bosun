use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::tool::ALL_TOOLS;

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

/// Why a session was interrupted, recorded on the session so stop semantics
/// can tell a user-requested halt from a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptCause {
    /// The user interrupted the session.
    User,
    /// A crash interrupted the session: a control-plane restart, or a turn
    /// that failed on its own.
    Crash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub node: String,
    pub repo_url: Option<String>,
    pub git_ref: Option<String>,
    pub dir: String,
    pub model: String,
    /// The persona this session runs under, resolved by name at creation.
    /// None for sessions created before personas existed; they keep the
    /// default system prompt.
    #[serde(default)]
    pub persona: Option<String>,
    /// The session that spawned this one. None for a root session; a child
    /// runs on its parent's node and working copy as a session of its own.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// The tree root's session id; a root session owns itself. Every session
    /// in one tree shares this id, which is how list and metering views group
    /// them. A missing owner on the wire means the session predates the tree.
    #[serde(default)]
    pub owner_id: String,
    pub permission: Permission,
    /// The persona's allowed-tools spec, resolved onto the session at
    /// creation: `"*"` for every canonical tool, or a list of tool names.
    #[serde(default = "default_allowed_tools")]
    pub allowed_tools: String,
    pub state: SessionState,
    /// Why the session was last interrupted: by the user, or by a crash.
    /// Recorded when the session becomes interrupted and kept when it later
    /// resumes, so stop semantics can still tell a user-requested halt from
    /// a crash. The next interruption replaces it. None for a session that
    /// has never been interrupted.
    #[serde(default)]
    pub interrupt_cause: Option<InterruptCause>,
    pub created_at_secs: i64,
    pub prompt: Option<String>,
}

fn default_allowed_tools() -> String {
    ALL_TOOLS.into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    /// The role's wire-format name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
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
        /// The child whose question this ask carries: when a root surfaces a
        /// child's question instead of answering it, the ask binds to that
        /// child so the user's answer is attributed to it and routes back to
        /// it. None for a question the session asks on its own behalf. Skipped
        /// when None, so an unbound ask serializes byte-identically to the
        /// pre-binding transcript format.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_id: Option<String>,
        answer: Option<String>,
    },
    Summary {
        text: String,
    },
    /// One authored event a child session wrote into its parent's thread: a
    /// completion report, a question, or a failure notice, attributed by
    /// session id. The child's own transcript stays on the child session.
    ChildEvent {
        child_id: String,
        /// Report, ask, or failure. The outer enum already uses `kind` as its
        /// serde tag, so this field is renamed on the wire.
        #[serde(rename = "event_kind")]
        kind: ChildEventKind,
        text: String,
    },
}

/// What a child session authored to its parent: a completion report, a
/// question the parent answers, denies, or passes up (ask gating, S6), or a
/// failure notice (crash reporting, S8). The event channel carries all three
/// from the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildEventKind {
    /// The child ended its turn without an ask: its final words, reporting
    /// what it did.
    Report,
    /// The child asked its parent a question.
    Ask,
    /// The child's turn failed; the parent decides what to do with it.
    Failure,
}

impl ChildEventKind {
    /// The kind's wire-format name, which reads as the verb in transcript
    /// renderings.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChildEventKind::Report => "report",
            ChildEventKind::Ask => "ask",
            ChildEventKind::Failure => "failure",
        }
    }
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
    Persona {
        persona: String,
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
    fn interrupt_cause_parses_and_serializes_snake_case() {
        let user: InterruptCause = serde_json::from_str("\"user\"").unwrap();
        let crash: InterruptCause = serde_json::from_str("\"crash\"").unwrap();
        assert_eq!(user, InterruptCause::User);
        assert_eq!(crash, InterruptCause::Crash);
        assert_eq!(serde_json::to_string(&user).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&crash).unwrap(), "\"crash\"");
    }

    #[test]
    fn role_parses_and_serializes_snake_case() {
        let user: Role = serde_json::from_str("\"user\"").unwrap();
        let assistant: Role = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(user, Role::User);
        assert_eq!(assistant, Role::Assistant);
        assert_eq!(serde_json::to_string(&assistant).unwrap(), "\"assistant\"");
        assert_eq!(assistant.as_str(), "assistant");
        assert_eq!(user.as_str(), "user");
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
                child_id: None,
                answer: None,
            },
            Block::Summary {
                text: "summarized".into(),
            },
            Block::ChildEvent {
                child_id: "child-1".into(),
                kind: ChildEventKind::Report,
                text: "finished the change".into(),
            },
        ] {
            assert_round_trips(&block);
        }
    }

    #[test]
    fn child_event_kind_round_trips_and_parses_snake_case() {
        for kind in [
            ChildEventKind::Report,
            ChildEventKind::Ask,
            ChildEventKind::Failure,
        ] {
            assert_round_trips(&kind);
        }
        let report: ChildEventKind = serde_json::from_str("\"report\"").unwrap();
        let ask: ChildEventKind = serde_json::from_str("\"ask\"").unwrap();
        let failure: ChildEventKind = serde_json::from_str("\"failure\"").unwrap();
        assert_eq!(report, ChildEventKind::Report);
        assert_eq!(ask, ChildEventKind::Ask);
        assert_eq!(failure, ChildEventKind::Failure);
        assert_eq!(
            serde_json::to_string(&ChildEventKind::Ask).unwrap(),
            "\"ask\""
        );
    }

    #[test]
    fn ask_block_round_trips_with_an_answer() {
        let block = Block::Ask {
            message: "continue?".into(),
            options: vec!["yes".into()],
            child_id: Some("child-1".into()),
            answer: Some("yes".into()),
        };
        assert_round_trips(&block);
    }

    #[test]
    fn an_ask_without_a_child_id_field_parses_as_unbound() {
        // Asks written before the binding existed carry no child_id; they
        // parse as an unbound question, so stored transcripts stay readable.
        let ask: Block = serde_json::from_str(
            r#"{"kind":"ask","message":"continue?","options":[],"answer":null}"#,
        )
        .unwrap();
        let Block::Ask { child_id, .. } = &ask else {
            panic!("expected an ask block");
        };
        assert!(child_id.is_none());
    }

    #[test]
    fn an_unbound_ask_serializes_without_a_child_id_field() {
        // Asks written before the binding existed carry no child_id; an
        // unbound ask must serialize byte-identically, so a transcript the
        // current build writes stays indistinguishable from a pre-binding one.
        let json = serde_json::to_value(Block::Ask {
            message: "continue?".into(),
            options: vec![],
            child_id: None,
            answer: None,
        })
        .unwrap();
        assert_eq!(json["kind"], "ask");
        assert_eq!(json["answer"], serde_json::Value::Null);
        assert!(
            json.get("child_id").is_none(),
            "an unbound ask carries no child_id field: {json}"
        );

        let json = serde_json::to_value(Block::Ask {
            message: "may I push?".into(),
            options: vec![],
            child_id: Some("child-1".into()),
            answer: None,
        })
        .unwrap();
        assert_eq!(json["kind"], "ask");
        assert_eq!(json["child_id"], "child-1");
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
            child_id: Some("child-1".into()),
            answer: None,
        })
        .unwrap();
        assert_eq!(json["kind"], "ask");
        assert_eq!(json["child_id"], "child-1");

        let json = serde_json::to_value(Block::ChildEvent {
            child_id: "child-1".into(),
            kind: ChildEventKind::Report,
            text: "done".into(),
        })
        .unwrap();
        assert_eq!(json["kind"], "child_event");
        assert_eq!(json["child_id"], "child-1");
        assert_eq!(json["event_kind"], "report");
        assert_eq!(json["text"], "done");
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
            Event::Persona {
                persona: "reviewer".into(),
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

        let json = serde_json::to_value(Event::Persona {
            persona: "reviewer".into(),
        })
        .unwrap();
        assert_eq!(json["kind"], "persona");
        assert_eq!(json["persona"], "reviewer");

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

    #[test]
    fn session_round_trips_persona_and_allowed_tools() {
        let session = Session {
            id: "s1".into(),
            node: "n1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/work".into(),
            model: "main".into(),
            persona: Some("reviewer".into()),
            parent_id: Some("root-1".into()),
            owner_id: "root-1".into(),
            permission: Permission::ReadOnly,
            allowed_tools: "file/read, git".into(),
            state: SessionState::Interrupted,
            interrupt_cause: Some(InterruptCause::User),
            created_at_secs: 1_700_000_000,
            prompt: None,
        };
        assert_round_trips(&session);
    }

    #[test]
    fn session_without_persona_or_allowed_tools_defaults() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "id": "s1",
            "node": "n1",
            "repo_url": null,
            "git_ref": null,
            "dir": "/work",
            "model": "main",
            "permission": "read_write",
            "state": "waiting_for_input",
            "created_at_secs": 1_700_000_000,
            "prompt": null,
        }))
        .unwrap();
        assert_eq!(session.persona, None);
        assert_eq!(session.allowed_tools, "*");
        // A session without tree fields predates children, so it is a root.
        assert_eq!(session.parent_id, None);
        assert_eq!(session.owner_id, "");
        assert_eq!(session.interrupt_cause, None);
    }

    #[test]
    fn a_child_session_round_trips_its_tree_fields() {
        let session = Session {
            id: "child-1".into(),
            node: "n1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/work".into(),
            model: "cheap".into(),
            persona: Some("reviewer".into()),
            parent_id: Some("root-1".into()),
            owner_id: "root-1".into(),
            permission: Permission::ReadOnly,
            allowed_tools: "file/read, grep".into(),
            state: SessionState::Running,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: Some("review the change".into()),
        };
        assert_round_trips(&session);
    }
}
