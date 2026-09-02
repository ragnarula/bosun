use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use bosun_common::error::ErrorExt;
use bosun_common::types::NodeCommand;
use bosun_common::types::NodeStatus;
use bosun_common::types::PollRequest;
use bosun_common::types::PollResponse;
use bosun_common::types::UpdateStatus;
use bosun_common::version::compare;
use rustls::ClientConfig;
use tokio::task::JoinHandle;
use tracing::error;
use tracing::warn;

use crate::command::execute;
use crate::command::handle_update_command;
use crate::manager::NodeManager;
use crate::update::apply;
use crate::update::should_update;
use crate::update::status_from_error;
use crate::update::update_status;

/// The control plane holds a poll for `node_timeout_secs / 2`. The client
/// timeout must exceed the longest possible hold, so it is fixed well above
/// the default hold.
const POLL_TIMEOUT: Duration = Duration::from_secs(600);
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// A failed update attempt suppresses the next one for this long, so a bad
/// release cannot turn the poll loop into a retry storm.
pub const UPDATE_RETRY_DELAY: Duration = Duration::from_secs(60);

/// The node's one outbound control loop: heartbeats, command delivery, and
/// command results all ride this request.
pub async fn run_poll_loop(
    cp_url: String,
    node_name: String,
    manager: Arc<NodeManager>,
    tls_config: Option<Arc<ClientConfig>>,
    update_enabled: bool,
    update_base_url: Option<String>,
    update_retry_delay: Duration,
) {
    let client = bosun_common::tls::reqwest_client_with_tls(tls_config.clone())
        .expect("failed to build the polling HTTP client");
    let url = format!("{}/poll", cp_url.trim_end_matches('/'));
    // The release feed the node fetches update archives from: the config's
    // base URL when set, else the BOSUN_UPDATE_BASE_URL mirror, else GitHub.
    let update_base_url = bosun_common::update::resolve_update_base_url(update_base_url.as_deref());
    let mut pending: Option<bosun_common::types::CommandResult> = None;
    let mut last_update_attempt: Option<Instant> = None;
    let mut update_task: Option<JoinHandle<()>> = None;
    let last_outcome: Arc<Mutex<Option<UpdateStatus>>> = Arc::new(Mutex::new(None));
    // Whether the last spawned apply was a demanded downgrade. A failure's
    // outcome is stale when the relationship it was made under reversed, so
    // the staleness rule depends on which direction the attempt went.
    let mut last_attempt_was_downgrade = false;
    // The status a request carries describes the control plane's last answer;
    // the first poll of a fresh loop has none yet.
    let mut last_cp_version = String::new();

    loop {
        let result = pending.take();
        let update_in_flight = update_task.as_ref().is_some_and(|task| !task.is_finished());
        let outcome = last_outcome.lock().unwrap().clone();
        let request = PollRequest {
            node_name: node_name.clone(),
            status: NodeStatus::Up,
            result: result.clone(),
            version: bosun_common::version::VERSION.to_string(),
            update_status: update_status(
                bosun_common::version::VERSION,
                &last_cp_version,
                update_enabled,
                update_in_flight,
                outcome.as_ref(),
            ),
        };

        let response: PollResponse = match client
            .post(&url)
            .json(&request)
            .timeout(POLL_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => match response.json().await {
                Ok(response) => response,
                Err(error) => {
                    warn!(error = %error, "control plane returned an unparsable poll response");
                    pending = result;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
            },
            Err(error) => {
                warn!(error = %error.display_chain(), "poll failed; retrying");
                pending = result;
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        last_cp_version = response.version.clone();

        // A failure's outcome describes the node only while the relationship
        // that produced it holds: an upgrade failure while the control plane
        // is still ahead, a downgrade failure while the node is still ahead.
        // Once the relationship reverses no new attempt will spawn, so the
        // stale failure must not keep winning the reported status.
        let node_behind = matches!(
            compare(&response.version, bosun_common::version::VERSION),
            Some(Ordering::Greater)
        );
        let node_ahead = matches!(
            compare(&response.version, bosun_common::version::VERSION),
            Some(Ordering::Less)
        );
        let outcome_stale = if last_attempt_was_downgrade {
            !node_ahead
        } else {
            !node_behind
        };
        if update_enabled && outcome_stale {
            *last_outcome.lock().unwrap() = None;
        }

        // A download can outlast many polls, so the poll loop must never
        // start a second apply while one is still running.
        let update_free = update_task.as_ref().is_none_or(|task| task.is_finished());
        let update_due = match last_update_attempt {
            Some(attempt) => attempt.elapsed() >= update_retry_delay,
            None => true,
        };
        if update_due
            && update_free
            && should_update(
                bosun_common::version::VERSION,
                &response.version,
                update_enabled,
            )
        {
            last_update_attempt = Some(Instant::now());
            last_attempt_was_downgrade = false;
            // The new attempt supersedes the previous outcome: while it runs
            // the node reports Updating, and its failure replaces the old one.
            *last_outcome.lock().unwrap() = None;
            let client = client.clone();
            let update_base_url = update_base_url.clone();
            let expected_version = response.version.clone();
            let outcome = last_outcome.clone();
            update_task = Some(tokio::spawn(async move {
                if let Err(error) = apply(&client, &update_base_url, &expected_version, false).await
                {
                    let status = status_from_error(&error);
                    error!(error = %error.display_chain(), "node update failed");
                    *outcome.lock().unwrap() = Some(status);
                }
            }));
        }

        if let Some(command) = response.command {
            pending = match command {
                NodeCommand::Update { id, version, force } => {
                    let result = handle_update_command(
                        &client,
                        &update_base_url,
                        id,
                        &version,
                        force,
                        update_enabled,
                        &mut update_task,
                        &last_outcome,
                    );
                    if result.is_none() {
                        last_attempt_was_downgrade = matches!(
                            compare(&version, bosun_common::version::VERSION),
                            Some(Ordering::Less)
                        );
                    }
                    result
                }
                other => Some(execute(&manager, other).await),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use bosun_common::types::CommandResult;
    use bosun_common::types::NodeCommand;
    use bosun_common::types::PollRequest;
    use bosun_common::types::PollResponse;
    use bosun_common::types::UpdateStatus;
    use tempfile::tempdir;

    use super::*;
    use crate::test_feed::ArchiveBehavior;
    use crate::test_feed::serve as serve_feed;

    /// Waits until `condition` holds, polling every 10ms. The poll loop's
    /// progress is real time, so a tight fixed deadline flakes under heavy
    /// parallel load; the generous bound only catches a regression that
    /// stalls the loop.
    async fn wait_until<F>(what: &str, mut condition: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if condition() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A control plane that answers polls with a version (strictly newer than
    /// the workspace version by default) and one queued command, so a test can
    /// revert the control plane under a running node or demand an update from
    /// it. It serves no update assets: the node under test fetches them from
    /// the release-feed stub instead.
    struct FakeControlPlane {
        addr: SocketAddr,
        polls: Arc<AtomicUsize>,
        last_status: Arc<Mutex<Option<UpdateStatus>>>,
        last_result: Arc<Mutex<Option<CommandResult>>>,
        version: Arc<Mutex<String>>,
        command: Arc<Mutex<Option<NodeCommand>>>,
    }

    impl FakeControlPlane {
        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.addr.port())
        }

        fn set_version(&self, version: &str) {
            *self.version.lock().unwrap() = version.to_string();
        }

        fn set_command(&self, command: NodeCommand) {
            *self.command.lock().unwrap() = Some(command);
        }
    }

    async fn fake_control_plane() -> FakeControlPlane {
        let polls = Arc::new(AtomicUsize::new(0));
        let last_status = Arc::new(Mutex::new(None));
        let last_result = Arc::new(Mutex::new(None));
        let version = Arc::new(Mutex::new(bosun_test_support::newer_than(
            bosun_common::version::VERSION,
        )));
        let command = Arc::new(Mutex::new(None));

        #[derive(Clone)]
        struct ServerState {
            polls: Arc<AtomicUsize>,
            last_status: Arc<Mutex<Option<UpdateStatus>>>,
            last_result: Arc<Mutex<Option<CommandResult>>>,
            version: Arc<Mutex<String>>,
            command: Arc<Mutex<Option<NodeCommand>>>,
        }

        async fn serve_poll(
            State(state): State<ServerState>,
            Json(poll): Json<PollRequest>,
        ) -> Json<PollResponse> {
            state.polls.fetch_add(1, Ordering::Relaxed);
            *state.last_status.lock().unwrap() = Some(poll.update_status);
            // A command result is delivered once; later polls carry None, so
            // the first result must be latched for the test to see.
            if let Some(result) = poll.result {
                *state.last_result.lock().unwrap() = Some(result);
            }
            let version = state.version.lock().unwrap().clone();
            let command = state.command.lock().unwrap().take();
            Json(PollResponse { command, version })
        }

        let app = Router::new()
            .route("/poll", post(serve_poll))
            .with_state(ServerState {
                polls: polls.clone(),
                last_status: last_status.clone(),
                last_result: last_result.clone(),
                version: version.clone(),
                command: command.clone(),
            });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeControlPlane {
            addr,
            polls,
            last_status,
            last_result,
            version,
            command,
        }
    }

    #[tokio::test]
    async fn a_second_update_does_not_start_while_one_is_in_flight() {
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        let feed = serve_feed(&version, ArchiveBehavior::Hang).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_millis(100),
        ));

        wait_until("the first update to reach the archive download", || {
            feed.requests() == 2
        })
        .await;
        let polls_when_download_started = cp.polls.load(Ordering::Relaxed);

        wait_until("the polls to report the in-flight update", || {
            cp.last_status.lock().unwrap().as_ref() == Some(&UpdateStatus::Updating)
        })
        .await;

        // The archive download hangs, so several retry cooldowns pass while
        // it is still in flight; the single-flight gate must keep a second
        // update from starting.
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            assert_eq!(
                feed.requests(),
                2,
                "a second update task started while the first was in flight"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wait_until("the poll loop to keep polling", || {
            cp.polls.load(Ordering::Relaxed) > polls_when_download_started
        })
        .await;

        poll_loop.abort();
    }

    #[tokio::test]
    async fn a_failed_update_is_reported_in_later_polls() {
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        let feed = serve_feed(&version, ArchiveBehavior::Mismatch).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_millis(100),
        ));

        wait_until("the polls to report the failed update", || {
            cp.last_status.lock().unwrap().as_ref()
                == Some(&UpdateStatus::Failed("checksum mismatch".into()))
        })
        .await;

        poll_loop.abort();
    }

    #[tokio::test]
    async fn a_failed_update_stops_being_reported_once_the_cp_is_no_longer_ahead() {
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        let feed = serve_feed(&version, ArchiveBehavior::Mismatch).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        // The retry cooldown is short, so attempts may supersede the stale
        // outcome while the control plane is ahead; once it is no longer
        // ahead no new attempt can spawn, and the transition to up-to-date
        // must come from dropping the stale outcome.
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_millis(100),
        ));

        wait_until("the polls to report the failed update", || {
            cp.last_status.lock().unwrap().as_ref()
                == Some(&UpdateStatus::Failed("checksum mismatch".into()))
        })
        .await;

        cp.set_version(bosun_common::version::VERSION);

        wait_until(
            "the polls to report up-to-date after the cp was reverted",
            || cp.last_status.lock().unwrap().as_ref() == Some(&UpdateStatus::UpToDate),
        )
        .await;

        poll_loop.abort();
    }

    #[tokio::test]
    async fn a_disabled_node_reports_disabled_and_never_downloads() {
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        let feed = serve_feed(&version, ArchiveBehavior::Hang).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            false,
            Some(feed.base_url()),
            Duration::from_secs(5),
        ));

        wait_until("a disabled node to report disabled", || {
            cp.last_status.lock().unwrap().as_ref() == Some(&UpdateStatus::Disabled)
        })
        .await;
        assert_eq!(
            feed.requests(),
            0,
            "a disabled node must never fetch the release feed"
        );

        poll_loop.abort();
    }

    #[tokio::test]
    async fn a_forced_downgrade_is_attempted_and_its_failure_is_reported() {
        let version = bosun_test_support::older_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        cp.set_command(NodeCommand::Update {
            id: 1,
            version: version.clone(),
            force: true,
        });
        let feed = serve_feed(&version, ArchiveBehavior::Mismatch).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_secs(5),
        ));

        wait_until("the polls to report the failed forced downgrade", || {
            cp.last_status.lock().unwrap().as_ref()
                == Some(&UpdateStatus::Failed("checksum mismatch".into()))
        })
        .await;
        assert!(
            feed.requests() >= 2,
            "the forced downgrade must get past the gate and fetch the release assets"
        );

        poll_loop.abort();
    }

    #[tokio::test]
    async fn an_update_command_for_an_ahead_node_without_force_is_refused_and_reports_ahead() {
        let version = bosun_test_support::older_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        cp.set_command(NodeCommand::Update {
            id: 1,
            version: version.clone(),
            force: false,
        });
        let feed = serve_feed(&version, ArchiveBehavior::Hang).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_secs(5),
        ));

        wait_until("the polls to report the refused update", || {
            matches!(
                cp.last_result.lock().unwrap().as_ref(),
                Some(CommandResult::Error { id: 1, .. })
            )
        })
        .await;
        assert_eq!(
            cp.last_status.lock().unwrap().as_ref(),
            Some(&UpdateStatus::Ahead),
            "the refusal must leave the node reporting ahead"
        );
        assert_eq!(
            feed.requests(),
            0,
            "a refused downgrade must never fetch the release feed"
        );

        poll_loop.abort();
    }

    #[tokio::test]
    async fn a_demanded_update_during_an_in_flight_auto_update_is_refused() {
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        cp.set_command(NodeCommand::Update {
            id: 1,
            version: version.clone(),
            force: false,
        });
        let feed = serve_feed(&version, ArchiveBehavior::Hang).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_secs(5),
        ));

        wait_until("the polls to report the refused update", || {
            matches!(
                cp.last_result.lock().unwrap().as_ref(),
                Some(CommandResult::Error { id: 1, .. })
            )
        })
        .await;
        // The refusal can be reported before the auto-update's download
        // starts, so wait for it instead of racing it.
        wait_until("the auto-update's download to start", || {
            feed.requests() == 2
        })
        .await;
        // The download hangs, so the single-flight gate has no reason to
        // release; the refused demand must never start a second one.
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            assert_eq!(
                feed.requests(),
                2,
                "the refused demand must not start a second download"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        poll_loop.abort();
    }

    #[tokio::test]
    async fn a_missing_release_is_reported_as_no_release() {
        // The feed serves only the workspace's own release, while the control
        // plane advertises a newer version no feed has, so every fetch 404s.
        let version = bosun_test_support::newer_than(bosun_common::version::VERSION);
        let cp = fake_control_plane().await;
        cp.set_version(&version);
        let feed = serve_feed(bosun_common::version::VERSION, ArchiveBehavior::Mismatch).await;
        let dir = tempdir().unwrap();
        let manager = Arc::new(NodeManager::new(
            dir.path().to_path_buf(),
            vec![],
            cp.base_url(),
            None,
        ));
        let poll_loop = tokio::spawn(run_poll_loop(
            cp.base_url(),
            "test-node".into(),
            manager,
            None,
            true,
            Some(feed.base_url()),
            Duration::from_millis(100),
        ));

        wait_until("the polls to report no release", || {
            cp.last_status.lock().unwrap().as_ref() == Some(&UpdateStatus::NoRelease)
        })
        .await;
        assert!(
            feed.requests() >= 1,
            "the node must have fetched the missing release before reporting no release"
        );

        poll_loop.abort();
    }
}
