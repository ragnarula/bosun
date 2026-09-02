use std::fmt;
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

/// The header the control plane sets on every response, carrying its version.
pub const X_BOSUN_VERSION: &str = "x-bosun-version";

/// The node's outbound control request. Carries the heartbeat payload and the
/// result of the previously executed command, if any. Sessions are no longer
/// reported here: the control plane's store is their source of truth.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollRequest {
    pub node_name: String,
    pub status: NodeStatus,
    pub result: Option<CommandResult>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub update_status: UpdateStatus,
}

/// The node's update state as of its last poll. The node computes it from the
/// control plane's version, its own version, its update config, and the
/// outcome of its last update attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStatus {
    #[default]
    UpToDate,
    Updating,
    Failed(String),
    Ahead,
    Disabled,
    /// The node could not update to the announced version because the release
    /// feed serves no archive for it.
    NoRelease,
}

impl fmt::Display for UpdateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateStatus::UpToDate => write!(f, "up-to-date"),
            UpdateStatus::Updating => write!(f, "updating"),
            UpdateStatus::Failed(reason) => write!(f, "failed: {reason}"),
            UpdateStatus::Ahead => write!(f, "ahead"),
            UpdateStatus::Disabled => write!(f, "disabled"),
            UpdateStatus::NoRelease => write!(f, "no-release"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollResponse {
    pub command: Option<NodeCommand>,
    #[serde(default)]
    pub version: String,
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
    /// Apply the control plane's version. The version rides the command so
    /// the node does not depend on poll state when executing; `force` allows
    /// a downgrade.
    Update {
        id: u64,
        version: String,
        #[serde(default)]
        force: bool,
    },
}

impl NodeCommand {
    pub fn id(&self) -> u64 {
        match self {
            NodeCommand::Clone { id, .. }
            | NodeCommand::Dev { id, .. }
            | NodeCommand::Dirs { id, .. }
            | NodeCommand::Stop { id, .. }
            | NodeCommand::Update { id, .. } => *id,
        }
    }
}

/// A demand that a node apply the control plane's version. The control plane
/// fills in its own version; only the force flag travels from the CLI.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeUpdateRequest {
    #[serde(default)]
    pub force: bool,
}

/// The node's answer to a command, delivered in the next poll.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CommandResult {
    Session {
        id: u64,
        session: SessionInfo,
    },
    Dirs {
        id: u64,
        listing: DirListing,
    },
    Stop {
        id: u64,
    },
    /// A demanded update was a no-op: the node already ran the version the
    /// command carried, so nothing was downloaded or restarted.
    #[serde(rename = "up-to-date")]
    UpToDate {
        id: u64,
        message: String,
    },
    Error {
        id: u64,
        message: String,
    },
}

impl CommandResult {
    pub fn id(&self) -> u64 {
        match self {
            CommandResult::Session { id, .. }
            | CommandResult::Dirs { id, .. }
            | CommandResult::Stop { id }
            | CommandResult::UpToDate { id, .. }
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

    #[test]
    fn poll_request_round_trips_the_version() {
        let request = PollRequest {
            node_name: "node-1".into(),
            status: NodeStatus::Up,
            result: None,
            version: "0.9.0".into(),
            update_status: UpdateStatus::UpToDate,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["version"], "0.9.0");
        let decoded: PollRequest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.version, "0.9.0");
    }

    #[test]
    fn poll_request_from_an_old_node_has_no_version() {
        let request: PollRequest =
            serde_json::from_str(r#"{"node_name":"node-1","status":"up","result":null}"#).unwrap();
        assert_eq!(request.version, "");
    }

    #[test]
    fn poll_request_from_an_old_node_defaults_update_status_to_up_to_date() {
        let request: PollRequest =
            serde_json::from_str(r#"{"node_name":"node-1","status":"up","result":null}"#).unwrap();
        assert_eq!(request.update_status, UpdateStatus::UpToDate);
    }

    #[test]
    fn poll_request_round_trips_the_update_status() {
        let request = PollRequest {
            node_name: "node-1".into(),
            status: NodeStatus::Up,
            result: None,
            version: "0.9.0".into(),
            update_status: UpdateStatus::Failed("checksum mismatch".into()),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["update_status"]["failed"], "checksum mismatch");
        let decoded: PollRequest = serde_json::from_value(json).unwrap();
        assert_eq!(
            decoded.update_status,
            UpdateStatus::Failed("checksum mismatch".into())
        );
    }

    #[test]
    fn update_status_round_trips_every_variant() {
        for status in [
            UpdateStatus::UpToDate,
            UpdateStatus::Updating,
            UpdateStatus::Failed("download failed".into()),
            UpdateStatus::Ahead,
            UpdateStatus::Disabled,
            UpdateStatus::NoRelease,
        ] {
            let json = serde_json::to_value(&status).unwrap();
            let decoded: UpdateStatus = serde_json::from_value(json).unwrap();
            assert_eq!(decoded, status);
        }
    }

    #[test]
    fn update_status_displays_the_registry_label() {
        assert_eq!(UpdateStatus::UpToDate.to_string(), "up-to-date");
        assert_eq!(UpdateStatus::Updating.to_string(), "updating");
        assert_eq!(
            UpdateStatus::Failed("checksum mismatch".into()).to_string(),
            "failed: checksum mismatch"
        );
        assert_eq!(UpdateStatus::Ahead.to_string(), "ahead");
        assert_eq!(UpdateStatus::Disabled.to_string(), "disabled");
        assert_eq!(UpdateStatus::NoRelease.to_string(), "no-release");
    }

    #[test]
    fn poll_response_round_trips_the_version() {
        let response = PollResponse {
            command: None,
            version: "0.5.5".into(),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["version"], "0.5.5");
        let decoded: PollResponse = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.version, "0.5.5");
    }

    #[test]
    fn poll_response_from_an_old_control_plane_defaults_the_version() {
        let response: PollResponse = serde_json::from_str(r#"{"command":null}"#).unwrap();
        assert_eq!(response.version, "");
    }

    #[test]
    fn update_command_round_trips_version_and_force() {
        let command = NodeCommand::Update {
            id: 7,
            version: "0.5.5".into(),
            force: true,
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["kind"], "update");
        assert_eq!(json["version"], "0.5.5");
        assert_eq!(json["force"], true);
        let decoded: NodeCommand = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.id(), 7);
        let NodeCommand::Update { version, force, .. } = decoded else {
            panic!("the command must stay an update");
        };
        assert_eq!(version, "0.5.5");
        assert!(force);
    }

    #[test]
    fn update_command_defaults_force_to_false() {
        let command: NodeCommand =
            serde_json::from_str(r#"{"kind":"update","id":1,"version":"0.5.5"}"#).unwrap();
        let NodeCommand::Update { force, .. } = command else {
            panic!("expected the update command");
        };
        assert!(!force);
    }

    #[test]
    fn up_to_date_result_round_trips_the_kind_and_message() {
        let result = CommandResult::UpToDate {
            id: 7,
            message: "already up to date at version 0.5.5".into(),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["kind"], "up-to-date");
        assert_eq!(json["id"], 7);
        assert_eq!(json["message"], "already up to date at version 0.5.5");
        let decoded: CommandResult = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.id(), 7);
        let CommandResult::UpToDate { message, .. } = decoded else {
            panic!("the result must stay an up-to-date");
        };
        assert_eq!(message, "already up to date at version 0.5.5");
    }

    #[test]
    fn update_request_round_trips_force() {
        let request = NodeUpdateRequest { force: true };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["force"], true);
        let decoded: NodeUpdateRequest = serde_json::from_value(json).unwrap();
        assert!(decoded.force);
    }

    #[test]
    fn update_request_defaults_force_to_false() {
        let request: NodeUpdateRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!request.force);
    }
}
