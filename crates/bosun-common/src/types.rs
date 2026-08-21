use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

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
/// result of the previously executed command, if any.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollRequest {
    pub node_name: String,
    pub status: NodeStatus,
    pub sessions: Vec<SessionInfo>,
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
    },
    Dev {
        id: u64,
        session_id: String,
        dir: PathBuf,
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeCloneRequest {
    pub session_id: String,
    pub repo_url: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DevRequest {
    pub node: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeDevRequest {
    pub session_id: String,
    pub dir: PathBuf,
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
