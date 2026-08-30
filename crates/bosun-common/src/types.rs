use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::session::Permission;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Up,
    Down,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub repo_url: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub dir: Option<PathBuf>,
    pub status: String,
}

/// The node's outbound control request. Carries the heartbeat payload and the
/// result of the previously executed command, if any. Sessions are no longer
/// reported here: the control plane's store is their source of truth.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollRequest {
    pub node_name: String,
    pub status: NodeStatus,
    pub result: Option<CommandResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollResponse {
    pub command: Option<NodeCommand>,
}

/// A command the control plane hands a node to execute. The `id` is echoed
/// back in the `CommandResult`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NodeCommand {
    Clone {
        id: u64,
        session_id: String,
        repo_url: String,
        #[serde(rename = "ref")]
        git_ref: Option<String>,
        permission: Permission,
    },
    Dev {
        id: u64,
        session_id: String,
        dir: PathBuf,
        permission: Permission,
    },
    Dirs {
        id: u64,
        path: Option<PathBuf>,
    },
    Stop {
        id: u64,
        session_id: String,
    },
}

impl NodeCommand {
    pub fn id(&self) -> u64 {
        match self {
            NodeCommand::Clone { id, .. }
            | NodeCommand::Dev { id, .. }
            | NodeCommand::Dirs { id, .. }
            | NodeCommand::Stop { id, .. } => *id,
        }
    }
}

/// The node's answer to a command, delivered in the next poll.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CommandResult {
    Session { id: u64, session: SessionInfo },
    Dirs { id: u64, listing: DirListing },
    Stop { id: u64 },
    Error { id: u64, message: String },
}

impl CommandResult {
    pub fn id(&self) -> u64 {
        match self {
            CommandResult::Session { id, .. }
            | CommandResult::Dirs { id, .. }
            | CommandResult::Stop { id }
            | CommandResult::Error { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CloneRequest {
    pub node: String,
    pub repo_url: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<Permission>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeCloneRequest {
    pub session_id: String,
    pub repo_url: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub permission: Permission,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DevRequest {
    pub node: String,
    pub dir: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<Permission>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeDevRequest {
    pub session_id: String,
    pub dir: PathBuf,
    pub permission: Permission,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StopRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_repo: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DirListing {
    pub path: Option<PathBuf>,
    pub parent: Option<PathBuf>,
    pub entries: Vec<DirEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_request_defaults_model_and_permission() {
        let request: CloneRequest =
            serde_json::from_str(r#"{"node":"node-1","repo_url":"https://example.com/repo"}"#)
                .unwrap();
        assert_eq!(request.model, None);
        assert_eq!(request.permission, None);
        assert_eq!(request.prompt, None);
    }

    #[test]
    fn clone_request_parses_an_explicit_permission() {
        let request: CloneRequest = serde_json::from_str(
            r#"{"node":"node-1","repo_url":"https://example.com/repo","permission":"read_write"}"#,
        )
        .unwrap();
        assert_eq!(request.permission, Some(Permission::ReadWrite));
    }

    #[test]
    fn dev_request_defaults_model_and_permission() {
        let request: DevRequest =
            serde_json::from_str(r#"{"node":"node-1","dir":"/work/repo"}"#).unwrap();
        assert_eq!(request.model, None);
        assert_eq!(request.permission, None);
        assert_eq!(request.prompt, None);
    }

    #[test]
    fn clone_request_parses_an_optional_prompt() {
        let request: CloneRequest = serde_json::from_str(
            r#"{"node":"node-1","repo_url":"https://example.com/repo","prompt":"fix the bug"}"#,
        )
        .unwrap();
        assert_eq!(request.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn node_clone_request_round_trips_permission() {
        let request = NodeCloneRequest {
            session_id: "s1".into(),
            repo_url: "https://example.com/repo".into(),
            git_ref: Some("main".into()),
            permission: Permission::ReadOnly,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["permission"], "read_only");
        let decoded: NodeCloneRequest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.permission, Permission::ReadOnly);
    }

    #[test]
    fn poll_request_has_no_sessions_field() {
        let request: PollRequest =
            serde_json::from_str(r#"{"node_name":"node-1","status":"up","result":null}"#).unwrap();
        assert_eq!(request.node_name, "node-1");
        assert!(request.result.is_none());
    }
}
