use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bosun_common::error::ErrorExt;
use bosun_common::types::CommandResult;
use bosun_common::types::NodeCloneRequest;
use bosun_common::types::NodeCommand;
use bosun_common::types::NodeDevRequest;
use bosun_common::types::NodeStartRequest;
use bosun_common::types::UpdateStatus;
use bosun_common::version::VERSION;
use bosun_common::version::compare;
use tokio::task::JoinHandle;
use tracing::error;

use crate::manager::NodeManager;
use crate::update::apply;
use crate::update::status_from_error;

/// Why a demanded update is refused before any apply runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateRefusal {
    /// `[update] enabled = false`: the opt-out is a hard override.
    Disabled,
    /// The node is ahead of the control plane and no `--force` was given.
    Ahead,
    /// The command's version is not a parsable semver.
    UnparsableVersion,
}

impl UpdateRefusal {
    fn message(self) -> &'static str {
        match self {
            UpdateRefusal::Disabled => "update disabled",
            UpdateRefusal::Ahead => "node is ahead of the control plane; pass --force to downgrade",
            UpdateRefusal::UnparsableVersion => "the control plane version is unparsable",
        }
    }
}

/// What a demanded update must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateAction {
    /// Download, verify, swap, and restart into the commanded version.
    Apply,
    /// The node already runs the commanded version; nothing to do.
    UpToDate,
}

/// Decides what a demanded update does: refuse (disabled, downgrade without
/// force, unparsable version), apply, or report up to date when the versions
/// already match. Equal versions are a no-op: applying them would restart an
/// already-current node for nothing.
pub(crate) fn update_command_action(
    node_version: &str,
    cp_version: &str,
    enabled: bool,
    force: bool,
) -> Result<UpdateAction, UpdateRefusal> {
    if !enabled {
        return Err(UpdateRefusal::Disabled);
    }
    match compare(cp_version, node_version) {
        Some(Ordering::Less) if !force => Err(UpdateRefusal::Ahead),
        Some(Ordering::Equal) => Ok(UpdateAction::UpToDate),
        Some(_) => Ok(UpdateAction::Apply),
        None => Err(UpdateRefusal::UnparsableVersion),
    }
}

/// Executes a demanded `Update` command: refuses what the decision matrix
/// refuses, otherwise spawns the apply without blocking the poll loop and
/// reports the outcome through the poll's update status. `None` means the
/// apply is in flight and no command result is due.
#[allow(clippy::too_many_arguments)] // the demanded update needs the poll loop's whole update machinery
pub(crate) fn handle_update_command(
    client: &reqwest::Client,
    update_base_url: &str,
    id: u64,
    version: &str,
    force: bool,
    update_enabled: bool,
    update_task: &mut Option<JoinHandle<()>>,
    last_outcome: &Arc<Mutex<Option<UpdateStatus>>>,
) -> Option<CommandResult> {
    match update_command_action(VERSION, version, update_enabled, force) {
        Err(refusal) => {
            return Some(CommandResult::Error {
                id,
                message: refusal.message().to_string(),
            });
        }
        Ok(UpdateAction::UpToDate) => {
            return Some(CommandResult::UpToDate {
                id,
                message: format!("already up to date at version {version}"),
            });
        }
        Ok(UpdateAction::Apply) => {}
    }
    if update_task.as_ref().is_some_and(|task| !task.is_finished()) {
        return Some(CommandResult::Error {
            id,
            message: "an update is already in progress".to_string(),
        });
    }
    *last_outcome.lock().unwrap() = None;
    let client = client.clone();
    let update_base_url = update_base_url.to_string();
    let expected_version = version.to_string();
    let outcome = last_outcome.clone();
    *update_task = Some(tokio::spawn(async move {
        if let Err(error) = apply(&client, &update_base_url, &expected_version, force).await {
            let status = status_from_error(&error);
            error!(error = %error.display_chain(), "demanded node update failed");
            *outcome.lock().unwrap() = Some(status);
        }
    }));
    None
}

/// Executes one command the control plane queued for this node. The poll loop
/// intercepts `Update` commands, which need the update machinery rather than
/// the manager; an `Update` that still reaches this match is a caller
/// regression, so it is reported as an error instead of panicking.
pub async fn execute(manager: &Arc<NodeManager>, command: NodeCommand) -> CommandResult {
    match command {
        NodeCommand::Clone {
            id,
            session_id,
            repo_url,
            git_ref,
            permission,
        } => {
            let request = NodeCloneRequest {
                session_id,
                repo_url,
                git_ref,
                permission,
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
            permission,
        } => {
            let request = NodeDevRequest {
                session_id,
                dir,
                permission,
            };
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
        NodeCommand::Start {
            id,
            session_id,
            dir,
            permission,
        } => {
            let request = NodeStartRequest {
                session_id,
                dir,
                permission,
            };
            match manager.start(&request).await {
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
        NodeCommand::Dirs { id, path } => match manager.list_dir(path.as_deref()).await {
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
        NodeCommand::Update { id, .. } => CommandResult::Error {
            id,
            message: "update commands are handled by the poll loop, not by execute".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn older_than(version: &str) -> String {
        bosun_test_support::older_than(version)
    }

    fn newer_than(version: &str) -> String {
        bosun_test_support::newer_than(version)
    }

    #[test]
    fn disabled_refuses_every_version_and_force_combination() {
        for (version, force) in [
            (&newer_than(VERSION), false),
            (&newer_than(VERSION), true),
            (&older_than(VERSION), false),
            (&older_than(VERSION), true),
            (&VERSION.to_string(), true),
        ] {
            assert_eq!(
                update_command_action(VERSION, version, false, force),
                Err(UpdateRefusal::Disabled),
                "disabled with {version} force={force}"
            );
        }
    }

    #[test]
    fn ahead_refuses_without_force_and_applies_with_it() {
        let version = older_than(VERSION);
        assert_eq!(
            update_command_action(VERSION, &version, true, false),
            Err(UpdateRefusal::Ahead)
        );
        assert_eq!(
            update_command_action(VERSION, &version, true, true),
            Ok(UpdateAction::Apply)
        );
    }

    #[test]
    fn behind_applies_with_or_without_force() {
        let version = newer_than(VERSION);
        assert_eq!(
            update_command_action(VERSION, &version, true, false),
            Ok(UpdateAction::Apply)
        );
        assert_eq!(
            update_command_action(VERSION, &version, true, true),
            Ok(UpdateAction::Apply)
        );
    }

    #[test]
    fn equal_is_a_no_op_with_or_without_force() {
        assert_eq!(
            update_command_action(VERSION, VERSION, true, false),
            Ok(UpdateAction::UpToDate)
        );
        assert_eq!(
            update_command_action(VERSION, VERSION, true, true),
            Ok(UpdateAction::UpToDate)
        );
    }

    #[test]
    fn unparsable_version_is_refused() {
        assert_eq!(
            update_command_action(VERSION, "banana", true, true),
            Err(UpdateRefusal::UnparsableVersion)
        );
        assert_eq!(
            update_command_action(VERSION, "", true, false),
            Err(UpdateRefusal::UnparsableVersion)
        );
    }

    #[tokio::test]
    async fn refused_update_reports_an_error_result_with_the_command_id() {
        let outcome = Arc::new(Mutex::new(None));
        let mut update_task = None;
        let result = handle_update_command(
            &reqwest::Client::new(),
            "http://feed:8090",
            42,
            &older_than(VERSION),
            false,
            true,
            &mut update_task,
            &outcome,
        );
        let Some(CommandResult::Error { id, message }) = result else {
            panic!("the refusal must report an error result");
        };
        assert_eq!(id, 42);
        assert!(!message.is_empty());
    }

    #[tokio::test]
    async fn disabled_update_reports_an_error_result() {
        let outcome = Arc::new(Mutex::new(None));
        let mut update_task = None;
        let result = handle_update_command(
            &reqwest::Client::new(),
            "http://feed:8090",
            7,
            &older_than(VERSION),
            true,
            false,
            &mut update_task,
            &outcome,
        );
        assert!(
            matches!(result, Some(CommandResult::Error { id: 7, .. })),
            "the disabled opt-out must refuse even a forced downgrade"
        );
    }

    #[tokio::test]
    async fn update_during_an_in_flight_apply_is_refused() {
        let outcome = Arc::new(Mutex::new(None));
        let mut update_task = Some(tokio::spawn(std::future::pending::<()>()));
        let result = handle_update_command(
            &reqwest::Client::new(),
            "http://feed:8090",
            7,
            &older_than(VERSION),
            true,
            true,
            &mut update_task,
            &outcome,
        );
        assert!(
            matches!(result, Some(CommandResult::Error { id: 7, .. })),
            "a second apply must not start while one is in flight"
        );
    }

    #[tokio::test]
    async fn equal_version_update_reports_up_to_date_without_spawning_an_apply() {
        let outcome = Arc::new(Mutex::new(None));
        let mut update_task = None;
        let result = handle_update_command(
            &reqwest::Client::new(),
            "http://feed:8090",
            7,
            VERSION,
            false,
            true,
            &mut update_task,
            &outcome,
        );
        let Some(CommandResult::UpToDate { id, message }) = result else {
            panic!("an equal version must report an up-to-date result");
        };
        assert_eq!(id, 7);
        assert!(
            message.contains("already up to date"),
            "the no-op message must say the node is up to date: {message}"
        );
        assert!(
            update_task.is_none(),
            "an equal version must not download, swap, or restart"
        );
    }

    #[tokio::test]
    async fn equal_version_update_is_a_no_op_even_with_force() {
        let outcome = Arc::new(Mutex::new(None));
        let mut update_task = None;
        let result = handle_update_command(
            &reqwest::Client::new(),
            "http://feed:8090",
            7,
            VERSION,
            true,
            true,
            &mut update_task,
            &outcome,
        );
        assert!(
            matches!(result, Some(CommandResult::UpToDate { id: 7, .. })),
            "--force must not make an equal version apply"
        );
        assert!(update_task.is_none());
    }

    #[tokio::test]
    async fn execute_reports_an_update_command_as_an_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            "http://cp:8090".into(),
            None,
        ));

        let result = execute(
            &manager,
            NodeCommand::Update {
                id: 7,
                version: VERSION.into(),
                force: false,
            },
        )
        .await;

        let CommandResult::Error { id, message } = result else {
            panic!("a stray update command must report an error, not panic");
        };
        assert_eq!(id, 7);
        assert!(!message.is_empty());
    }

    #[tokio::test]
    async fn start_command_without_browse_roots_refuses_an_existing_dir() {
        // The roots gate applies to the child-session spawner's start command
        // exactly as it does to `dev`: with no browse roots the node refuses
        // the executor before the directory is even looked at.
        let work = tempfile::tempdir().unwrap();
        let session_dir = work.path().join("repo");
        std::fs::create_dir_all(&session_dir).unwrap();
        let manager = Arc::new(NodeManager::new(
            work.path().to_path_buf(),
            vec![],
            "http://cp:8090".into(),
            None,
        ));

        let result = execute(
            &manager,
            NodeCommand::Start {
                id: 7,
                session_id: "child-1".into(),
                dir: session_dir,
                permission: bosun_common::session::Permission::ReadOnly,
            },
        )
        .await;

        let CommandResult::Error { id, message } = result else {
            panic!("a start without browse roots must report an error, not a session");
        };
        assert_eq!(id, 7);
        assert_eq!(message, "no browse roots configured on this node");
    }

    #[tokio::test]
    async fn dev_command_on_a_node_without_roots_is_refused_by_the_browse_root_gate() {
        // The dev command keeps its browse-root gate at the handler: with no
        // browse roots the gate refuses before the directory is even looked
        // at, so the two commands stay distinct end to end.
        let work = tempfile::tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            work.path().to_path_buf(),
            vec![],
            "http://cp:8090".into(),
            None,
        ));

        let result = execute(
            &manager,
            NodeCommand::Dev {
                id: 7,
                session_id: "s1".into(),
                dir: work.path().join("missing"),
                permission: bosun_common::session::Permission::ReadOnly,
            },
        )
        .await;

        let CommandResult::Error { id, message } = result else {
            panic!("dev must report an error on a node without browse roots");
        };
        assert_eq!(id, 7);
        assert_eq!(message, "no browse roots configured on this node");
    }

    #[tokio::test]
    async fn start_command_refuses_an_existing_dir_outside_browse_roots() {
        // The escape browse roots close: the control plane asked the node to
        // run a child executor in a directory that exists but sits outside
        // every configured root, and the node refuses it.
        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://cp:8090".into(),
            None,
        ));

        let result = execute(
            &manager,
            NodeCommand::Start {
                id: 7,
                session_id: "child-1".into(),
                dir: outside.path().to_path_buf(),
                permission: bosun_common::session::Permission::ReadOnly,
            },
        )
        .await;

        let CommandResult::Error { id, message } = result else {
            panic!("a start outside the browse roots must report an error, not a session");
        };
        assert_eq!(id, 7);
        assert_eq!(
            message,
            format!(
                "directory {} is outside the configured browse roots",
                outside.path().display()
            )
        );
    }

    #[tokio::test]
    async fn start_command_refuses_a_missing_dir_as_a_dir_error() {
        // The absent-directory refusal still applies after the roots check:
        // with browse roots configured, a start at a directory that does not
        // exist is refused before any executor starts.
        let work = tempfile::tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://cp:8090".into(),
            None,
        ));

        let result = execute(
            &manager,
            NodeCommand::Start {
                id: 7,
                session_id: "child-1".into(),
                dir: work.path().join("missing"),
                permission: bosun_common::session::Permission::ReadOnly,
            },
        )
        .await;

        let CommandResult::Error { id, message } = result else {
            panic!("an absent dir must report an error, not a session");
        };
        assert_eq!(id, 7);
        assert_eq!(
            message,
            format!(
                "directory {} does not exist",
                work.path().join("missing").display()
            )
        );
    }

    #[tokio::test]
    async fn start_command_refuses_a_file_path_as_a_dir_error() {
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let manager = Arc::new(NodeManager::new(
            work.path().to_path_buf(),
            vec![work.path().to_path_buf()],
            "http://cp:8090".into(),
            None,
        ));

        let result = execute(
            &manager,
            NodeCommand::Start {
                id: 7,
                session_id: "child-1".into(),
                dir: file.clone(),
                permission: bosun_common::session::Permission::ReadOnly,
            },
        )
        .await;

        let CommandResult::Error { id, message } = result else {
            panic!("a file path must report an error, not a session");
        };
        assert_eq!(id, 7);
        assert_eq!(
            message,
            format!("path {} is not a directory", file.display())
        );
    }
}
