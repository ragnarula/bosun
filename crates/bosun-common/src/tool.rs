use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::session::Permission;

/// A persona's `allowed_tools` value that allows every canonical tool.
pub const ALL_TOOLS: &str = "*";

/// One canonical tool, as advertised to providers. `schema` is the JSON Schema
/// of the parameters object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// One typed operation the control plane sends to a session's executor on a
/// fresh logical connection. The node relay reads exactly one operation as the
/// first message of the connection, dispatches it to the session's in-process
/// `ExecutorState`, and writes the response frames on the same connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToolOp {
    /// Runs one tool: a JSON tool answers with one `Result`, the `shell` tool
    /// streams `Event` frames and ends with `Done`.
    Call {
        run_id: String,
        tool: String,
        args: Value,
    },
    /// Kills the running `shell` named by `run_id`. Always answered with
    /// `Ack`, even for an unknown run id.
    Cancel { run_id: String },
    /// Replaces the session's executor permission. Answered with `Ack` once
    /// applied.
    SetPermission { permission: Permission },
}

/// One typed response frame the node relay writes back to the control plane on
/// the connection an operation arrived on. `Ack`, `Error`, and `Result` are
/// terminal; `Event` and `Done` belong to one `shell` run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ToolMsg {
    /// A `Cancel` or `SetPermission` operation was applied.
    Ack,
    /// A call was refused or failed; carries the message the executor's tool
    /// error describes.
    Error { message: String },
    /// A non-streaming tool's JSON result.
    Result { content: Value },
    /// One chunk of a running shell's streamed output.
    Event { text: String },
    /// A shell run ended; carries its exit code.
    Done { exit_code: i32 },
}

/// Largest serialized tool frame, in bytes. Both ends enforce it: the writer
/// refuses a payload over the cap instead of emitting a frame the peer will
/// reject, and the reader rejects a length header over it, so neither side can
/// be driven into an unbounded allocation. File reads cap at 1 MiB and grep
/// caps at 500 matches, so every legitimate response stays well under this.
pub const MAX_TOOL_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Serialized payload is written in chunks of this size. A logical tunnel
/// connection rejects any single write above its frame limit, so one frame is
/// never handed over in a single oversized write.
const TOOL_FRAME_WRITE_CHUNK: usize = 32 * 1024;

/// Serializes `message` and writes it as one length-prefixed JSON frame: a
/// little-endian `u32` payload length followed by the payload.
pub async fn write_tool_frame<W>(write: &mut W, message: &impl Serialize) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(message)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if payload.len() > MAX_TOOL_FRAME_BYTES as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "tool frame of {} bytes exceeds the {MAX_TOOL_FRAME_BYTES} byte limit",
                payload.len()
            ),
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame is too large"))?;
    write.write_all(&len.to_le_bytes()).await?;
    for chunk in payload.chunks(TOOL_FRAME_WRITE_CHUNK) {
        write.write_all(chunk).await?;
    }
    Ok(())
}

/// Reads one length-prefixed JSON frame. Returns `Ok(None)` when the peer
/// closed the connection at a frame boundary, and an error on a truncated or
/// oversized frame or invalid JSON.
pub async fn read_tool_frame<R, M>(read: &mut R) -> std::io::Result<Option<M>>
where
    R: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    let mut len_bytes = [0u8; 4];
    let mut read_count = 0;
    while read_count < len_bytes.len() {
        match read.read(&mut len_bytes[read_count..]).await {
            Ok(0) => break,
            Ok(n) => read_count += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    if read_count == 0 {
        return Ok(None);
    }
    if read_count < len_bytes.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated tool frame length",
        ));
    }
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_TOOL_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("tool frame of {len} bytes exceeds the {MAX_TOOL_FRAME_BYTES} byte limit"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    read.read_exact(&mut payload).await?;
    let message = serde_json::from_slice(&payload)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(Some(message))
}

/// Live streaming delta from a running tool (shell output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDelta {
    pub text: String,
}

/// The one canonical tool list every provider adapter exposes. Read-only
/// sessions drop the mutating tools from the model's schema; the executor
/// refuses them regardless.
pub fn canonical_tools(permission: Permission) -> Vec<ToolSpec> {
    let all = vec![
        ToolSpec {
            name: "shell".into(),
            description: "Run a command in the session's shell. Output streams until the command exits.".into(),
            schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        },
        ToolSpec {
            name: "file/read".into(),
            description: "Read a file from the session's working copy.".into(),
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        },
        ToolSpec {
            name: "file/write".into(),
            description: "Write a file in the session's working copy, replacing its content.".into(),
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        },
        ToolSpec {
            name: "edit".into(),
            description: "Replace a single occurrence of `old` with `new` in a file.".into(),
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}),
        },
        ToolSpec {
            name: "grep".into(),
            description: "Search for a regex pattern in the working copy. `path` narrows the search to one file or directory.".into(),
            schema: json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}),
        },
        ToolSpec {
            name: "glob".into(),
            description: "List paths matching a glob pattern.".into(),
            schema: json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        },
        ToolSpec {
            name: "ask".into(),
            description: "Ask a question with optional choices that ends the turn and waits for an answer. At the root the question reaches the user. In a child session it reaches your parent instead: the parent answers, denies with a reason, or passes the question up, and you wait for the parent's message. To pass one of your child sessions' questions upward, call ask with the child's id from your manifest as child_id and the question as message: the surfaced ask names that child — the session you can message to answer or cancel it — and the user's answer is routed automatically to the session that originally asked, so the answer is never paraphrased on its way back. While a question you raised awaits an answer you may ask nothing else: message the child whose question you raised to cancel it first.".into(),
            schema: json!({"type":"object","properties":{"message":{"type":"string"},"options":{"type":"array","items":{"type":"string"}},"child_id":{"type":"string"}},"required":["message"]}),
        },
        ToolSpec {
            name: "todowrite".into(),
            description: "Replace the session todo list.".into(),
            schema: json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string","enum":["todo","in_progress","done"]}},"required":["id","content","status"]}}},"required":["items"]}),
        },
        ToolSpec {
            name: "git".into(),
            description: "Run a git read or commit command in the working copy. Push is refused.".into(),
            schema: json!({"type":"object","properties":{"args":{"type":"array","items":{"type":"string"}}},"required":["args"]}),
        },
        ToolSpec {
            name: "webfetch".into(),
            description: "Fetch a URL and return its content as text.".into(),
            schema: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        },
        ToolSpec {
            name: "skill".into(),
            description: "Load a skill's instructions into context. Skills are discovered from the working repo and the control plane.".into(),
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
        },
        ToolSpec {
            name: "spawn".into(),
            description: "Create a child session under a configured persona and hand it a task. Returns the child session's id; the child runs on its own executor and reports back when done.".into(),
            schema: json!({"type":"object","properties":{"persona":{"type":"string"},"instructions":{"type":"string"}},"required":["persona","instructions"]}),
        },
        ToolSpec {
            name: "message_child".into(),
            description: "Send a message to one of your child sessions, named in your live-children manifest. A child waiting for input has asked you a question: answer it or deny it with a reason here, and the child resumes from its own thread. If you surfaced a child's question to the user and the user redirects instead of answering, message that child to cancel its pending question. You can also use it to ask a working child for detail or to redirect it.".into(),
            schema: json!({"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"}},"required":["id","text"]}),
        },
    ];

    match permission {
        Permission::ReadWrite => all,
        Permission::ReadOnly => all
            .into_iter()
            .filter(|tool| !matches!(tool.name.as_str(), "shell" | "file/write" | "edit"))
            .collect(),
    }
}

/// An `allowed_tools` value named tools that are not canonical. Carries the
/// unknown names so boot validation can report them and a session can refuse
/// to run rather than silently widen its tool set.
#[derive(Debug, thiserror::Error)]
#[error("unknown tool name(s): {}", self.unknown.join(", "))]
pub struct UnknownToolsError {
    pub unknown: Vec<String>,
}

/// Parses a persona's `allowed_tools` value into the tool names it allows.
/// `"*"` allows every canonical tool (`Ok(None)`); anything else is a list of
/// canonical names split on commas and whitespace. Unknown names come back as
/// errors so a typo fails boot validation instead of silently narrowing the
/// tool set; duplicates are dropped.
pub fn parse_allowed_tools(value: &str) -> Result<Option<Vec<String>>, UnknownToolsError> {
    if value.trim() == ALL_TOOLS {
        return Ok(None);
    }
    let canonical: Vec<String> = canonical_tools(Permission::ReadWrite)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let mut names = Vec::new();
    let mut unknown = Vec::new();
    for raw in value.split([',', ' ', '\t', '\n']) {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        if names.iter().any(|n| n == name) || unknown.iter().any(|n| n == name) {
            continue;
        }
        if canonical.iter().any(|n| n == name) {
            names.push(name.to_string());
        } else {
            unknown.push(name.to_string());
        }
    }
    if unknown.is_empty() {
        Ok(Some(names))
    } else {
        Err(UnknownToolsError { unknown })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_returns_all_tools_in_order() {
        let tools = canonical_tools(Permission::ReadWrite);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "shell",
                "file/read",
                "file/write",
                "edit",
                "grep",
                "glob",
                "ask",
                "todowrite",
                "git",
                "webfetch",
                "skill",
                "spawn",
                "message_child",
            ]
        );
        for tool in &tools {
            assert!(!tool.description.is_empty());
            assert_eq!(tool.schema["type"], "object");
        }
    }

    #[test]
    fn read_only_omits_mutating_tools() {
        let tools = canonical_tools(Permission::ReadOnly);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "file/read",
                "grep",
                "glob",
                "ask",
                "todowrite",
                "git",
                "webfetch",
                "skill",
                "spawn",
                "message_child",
            ]
        );
    }

    #[test]
    fn shell_schema_requires_command() {
        let tools = canonical_tools(Permission::ReadWrite);
        let shell = tools.iter().find(|tool| tool.name == "shell").unwrap();
        assert_eq!(shell.schema["required"], json!(["command"]));
    }

    #[test]
    fn message_child_schema_requires_id_and_text() {
        let tools = canonical_tools(Permission::ReadWrite);
        let message_child = tools
            .iter()
            .find(|tool| tool.name == "message_child")
            .unwrap();
        assert_eq!(message_child.schema["required"], json!(["id", "text"]));
    }

    #[test]
    fn ask_schema_requires_message_and_makes_child_id_optional() {
        let tools = canonical_tools(Permission::ReadWrite);
        let ask = tools.iter().find(|tool| tool.name == "ask").unwrap();
        assert_eq!(ask.schema["required"], json!(["message"]));
        assert_eq!(
            ask.schema["properties"]["child_id"],
            json!({"type": "string"})
        );
        assert!(ask.description.contains("child_id"));
    }

    #[test]
    fn star_allowed_tools_parses_to_no_restriction() {
        assert!(matches!(parse_allowed_tools("*"), Ok(None)));
        assert!(matches!(parse_allowed_tools(" * "), Ok(None)));
        assert_eq!(ALL_TOOLS, "*");
    }

    #[test]
    fn allowed_tools_splits_on_commas_and_whitespace() {
        let parsed = parse_allowed_tools("shell, file/read  grep\nglob").unwrap();
        assert_eq!(
            parsed.unwrap(),
            ["shell", "file/read", "grep", "glob"],
            "names keep their given order"
        );
    }

    #[test]
    fn allowed_tools_drops_duplicates() {
        let parsed = parse_allowed_tools("shell shell, shell").unwrap();
        assert_eq!(parsed.unwrap(), ["shell"]);
    }

    #[test]
    fn empty_allowed_tools_allows_nothing() {
        for value in ["", "   ", ","] {
            let parsed = parse_allowed_tools(value).unwrap();
            assert_eq!(parsed.unwrap(), Vec::<String>::new(), "{value:?}");
        }
    }

    #[test]
    fn allowed_tools_rejects_unknown_names_in_order() {
        let err = parse_allowed_tools("shell, websurf, file/read, nope").unwrap_err();
        assert_eq!(err.unknown, ["websurf", "nope"]);
        assert_eq!(err.to_string(), "unknown tool name(s): websurf, nope");
    }

    #[test]
    fn every_canonical_name_is_accepted_as_allowed() {
        let tools = canonical_tools(Permission::ReadWrite);
        let spec: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        let parsed = parse_allowed_tools(&spec.join(" ")).unwrap();
        assert_eq!(parsed.unwrap().len(), tools.len());
    }

    #[test]
    fn tool_ops_and_msgs_round_trip_snake_case() {
        for op in [
            ToolOp::Call {
                run_id: "run-1".into(),
                tool: "file/read".into(),
                args: json!({ "path": "a.txt" }),
            },
            ToolOp::Cancel {
                run_id: "run-2".into(),
            },
            ToolOp::SetPermission {
                permission: Permission::ReadOnly,
            },
        ] {
            let json = serde_json::to_value(&op).unwrap();
            let decoded: ToolOp = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(decoded, op);
        }

        let json = serde_json::to_value(ToolOp::Call {
            run_id: "run-1".into(),
            tool: "shell".into(),
            args: json!({}),
        })
        .unwrap();
        assert_eq!(json["op"], "call");

        let json = serde_json::to_value(ToolOp::SetPermission {
            permission: Permission::ReadOnly,
        })
        .unwrap();
        assert_eq!(json["op"], "set_permission");
        assert_eq!(json["permission"], "read_only");

        for msg in [
            ToolMsg::Ack,
            ToolMsg::Error {
                message: "boom".into(),
            },
            ToolMsg::Result { content: json!({}) },
            ToolMsg::Event { text: "hi".into() },
            ToolMsg::Done { exit_code: 3 },
        ] {
            let json = serde_json::to_value(&msg).unwrap();
            let decoded: ToolMsg = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(decoded, msg);
        }

        let json = serde_json::to_value(ToolMsg::Done { exit_code: 3 }).unwrap();
        assert_eq!(json["msg"], "done");
    }

    #[tokio::test]
    async fn tool_frames_round_trip_over_a_stream() {
        use tokio::io::AsyncWriteExt;
        use tokio::io::duplex;

        let (mut client, mut node) = duplex(64 * 1024);
        let messages = vec![
            ToolMsg::Event {
                text: "building...".into(),
            },
            ToolMsg::Done { exit_code: 0 },
        ];
        for message in &messages {
            write_tool_frame(&mut client, message).await.unwrap();
        }
        client.shutdown().await.unwrap();

        let mut received = Vec::new();
        while let Some(message) = read_tool_frame::<_, ToolMsg>(&mut node).await.unwrap() {
            received.push(message);
        }
        assert_eq!(received, messages);
    }

    #[tokio::test]
    async fn read_tool_frame_reports_eof_and_truncation() {
        use tokio::io::AsyncWriteExt;
        use tokio::io::duplex;

        let (client, mut node) = duplex(1024);
        drop(client);
        assert!(
            read_tool_frame::<_, ToolMsg>(&mut node)
                .await
                .unwrap()
                .is_none(),
            "a clean close at a frame boundary reads as no frame"
        );

        let (mut client, mut node) = duplex(1024);
        client.write_all(&[0x10, 0x00]).await.unwrap();
        drop(client);
        let error = read_tool_frame::<_, ToolMsg>(&mut node)
            .await
            .expect_err("a truncated length must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_tool_frame_rejects_oversized_lengths() {
        use tokio::io::AsyncWriteExt;
        use tokio::io::duplex;

        let (mut client, mut node) = duplex(1024);
        let huge = (MAX_TOOL_FRAME_BYTES + 1).to_le_bytes();
        client.write_all(&huge).await.unwrap();
        let error = read_tool_frame::<_, ToolMsg>(&mut node)
            .await
            .expect_err("an oversized length must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn write_tool_frame_rejects_an_oversized_payload() {
        use tokio::io::duplex;

        let (mut client, _node) = duplex(1024);
        let oversized = "x".repeat(MAX_TOOL_FRAME_BYTES as usize + 1);
        let error = write_tool_frame(&mut client, &ToolMsg::Event { text: oversized })
            .await
            .expect_err("a payload over the cap must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_large_frame_writes_and_reads_across_chunks() {
        use tokio::io::AsyncWriteExt;
        use tokio::io::duplex;

        let (mut client, mut node) = duplex(1024 * 1024);
        let content = "x".repeat(300 * 1024);
        write_tool_frame(
            &mut client,
            &ToolMsg::Result {
                content: json!({ "content": content }),
            },
        )
        .await
        .unwrap();
        client.shutdown().await.unwrap();

        let message: ToolMsg = read_tool_frame(&mut node).await.unwrap().expect("a frame");
        let ToolMsg::Result { content } = message else {
            panic!("expected a result frame");
        };
        assert_eq!(content["content"].as_str().unwrap().len(), 300 * 1024);
    }
}
