use std::sync::Arc;

use bosun_common::types::CommandResult;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeCommand;
use bosun_common::types::NodeDevRequest;

use crate::manager::NodeManager;

/// Executes one command the control plane queued for this node.
pub async fn execute(manager: &Arc<NodeManager>, command: NodeCommand) -> CommandResult {
    match command {
        NodeCommand::Clone {
            id,
            session_id,
            repo_url,
            git_ref,
        } => {
            let request = NodeCloneRequest {
                session_id,
                repo_url,
                git_ref,
            };
            match manager.run_clone(&request).await {
                Ok(record) => CommandResult::Session {
                    id,
                    session: record.to_info(),
                },
                Err(error) => CommandResult::Error {
                    id,
                    message: error.to_string(),
                },
            }
        }
        NodeCommand::Dev {
            id,
            session_id,
            dir,
        } => {
            let request = NodeDevRequest { session_id, dir };
            match manager.dev(&request).await {
                Ok(record) => CommandResult::Session {
                    id,
                    session: record.to_info(),
                },
                Err(error) => CommandResult::Error {
                    id,
                    message: error.to_string(),
                },
            }
        }
        NodeCommand::Dirs { id, path } => match manager.list_dir(path.as_deref()) {
            Ok(listing) => CommandResult::Dirs { id, listing },
            Err(error) => CommandResult::Error {
                id,
                message: error.to_string(),
            },
        },
        NodeCommand::Stop { id, session_id } => match manager.stop(&session_id).await {
            Ok(()) => CommandResult::Stop { id },
            Err(error) => CommandResult::Error {
                id,
                message: error.to_string(),
            },
        },
    }
}
