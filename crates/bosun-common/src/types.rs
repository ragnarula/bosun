use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Heartbeat {
    pub node_name: String,
    pub status: NodeStatus,
    pub control_addr: String,
    pub sessions: Vec<SessionInfo>,
}

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
    pub opencode_port: Option<u16>,
    pub forwarder_addr: Option<String>,
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
