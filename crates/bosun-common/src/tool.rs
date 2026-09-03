use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

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
            name: "spawn".into(),
            description: "Create a child session under a configured persona and hand it a task. Returns the child session's id; the child runs on its own executor and reports back when done.".into(),
            schema: json!({"type":"object","properties":{"persona":{"type":"string"},"instructions":{"type":"string"}},"required":["persona","instructions"]}),
        },
        ToolSpec {
            name: "message_child".into(),
            description: "Send a message to one of your child sessions, named in your live-children manifest, to ask for detail or redirect it. The child resumes from its own thread and reports again.".into(),
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
}
