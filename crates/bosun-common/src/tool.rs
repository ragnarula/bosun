use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::session::Permission;

/// One canonical tool, as advertised to providers. `schema` is the JSON Schema
/// of the parameters object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Uniform envelope the control plane sends the executor for every tool call.
/// `run_id` lets a later POST /tool/{run_id}/cancel target the running tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub run_id: String,
    pub args: Value,
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
            description: "Ask the user a question with optional choices. Ends the turn; the session waits for an answer.".into(),
            schema: json!({"type":"object","properties":{"message":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}},"required":["message"]}),
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
            name: "spawn_subagent".into(),
            description: "Hand work to a subagent of a configured type, synchronously. Its activity appears in the transcript and its summary is returned.".into(),
            schema: json!({"type":"object","properties":{"subagent_type":{"type":"string"},"instructions":{"type":"string"}},"required":["subagent_type","instructions"]}),
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
                "spawn_subagent",
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
                "spawn_subagent",
            ]
        );
    }

    #[test]
    fn shell_schema_requires_command() {
        let tools = canonical_tools(Permission::ReadWrite);
        let shell = tools.iter().find(|tool| tool.name == "shell").unwrap();
        assert_eq!(shell.schema["required"], json!(["command"]));
    }
}
