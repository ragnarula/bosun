use std::sync::Arc;
use std::time::Duration;

use bosun_common::error::ErrorExt;
use bosun_common::types::NodeStatus;
use bosun_common::types::PollRequest;
use bosun_common::types::PollResponse;
use rustls::ClientConfig;
use tracing::warn;

use crate::command::execute;
use crate::manager::NodeManager;

/// The control plane holds a poll for `node_timeout_secs / 2`. The client
/// timeout must exceed the longest possible hold, so it is fixed well above
/// the default hold.
const POLL_TIMEOUT: Duration = Duration::from_secs(600);
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// The node's one outbound control loop: heartbeats, command delivery, and
/// command results all ride this request.
pub async fn run_poll_loop(
    cp_url: String,
    node_name: String,
    manager: Arc<NodeManager>,
    tls_config: Option<Arc<ClientConfig>>,
) {
    let client = bosun_common::tls::reqwest_client_with_tls(tls_config.clone())
        .expect("failed to build the polling HTTP client");
    let url = format!("{}/poll", cp_url.trim_end_matches('/'));
    let mut pending: Option<bosun_common::types::CommandResult> = None;

    loop {
        let result = pending.take();
        let request = PollRequest {
            node_name: node_name.clone(),
            status: NodeStatus::Up,
            result: result.clone(),
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

        if let Some(command) = response.command {
            pending = Some(execute(&manager, command).await);
        }
    }
}
