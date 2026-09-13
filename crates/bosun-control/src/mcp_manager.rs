//! The control plane's MCP connection manager: one shared connection per
//! enabled server.
//!
//! One task per server owns that server's connection. The task connects with
//! the stored credential, lists the tools, and keeps the list fresh on a
//! `tools/list_changed` notification or at the server's own TTL; a failed
//! connect retries with backoff, and the failure is recorded on the server
//! row. Nothing a server does touches another server, because each one has
//! its own task and its own slot.
//!
//! Sessions read the cached tools and call through the same connection, so
//! one connection serves every session that chose the server. A call that
//! finds an expired or rejected OAuth token, or a session the server has
//! ended, is retried once on a fresh connection. A challenge for scopes the
//! token does not carry starts one re-authorization, and its URL is offered
//! from here: a challenge a connect or a re-list meets leaves the server down
//! until the operator authorizes it, and a challenge a tool call meets serves
//! the tool list again while every call that needs the missing scope keeps
//! failing.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context;
use bosun_agent::agent_loop::McpAvailability;
use bosun_agent::agent_loop::McpCallError;
use bosun_agent::agent_loop::McpConnections;
use bosun_agent::config::resolve_api_key;
use bosun_common::error::ErrorExt;
use bosun_common::mcp::McpAuth;
use bosun_common::mcp::McpCallOutcome;
use bosun_common::mcp::McpServerSecret;
use bosun_common::mcp::McpTool;
use bosun_common::tool::ToolDelta;
use bosun_store::store::Store;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use crate::mcp_client::McpClient;
use crate::mcp_client::McpClientError;
use crate::mcp_client::McpToolList;
use crate::mcp_client::ServerEra;
use crate::mcp_oauth::McpOAuthContext;
use crate::mcp_oauth::union_scopes;

/// How long the manager waits after a failed connect before trying again; the
/// wait doubles up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// The longest wait between two connect attempts, so a server that stays down
/// is retried at a steady rate instead of ever more slowly.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The shortest gap between two TTL refreshes. A server that always answers
/// `ttlMs: 0` would otherwise make the manager list in a tight loop.
const MIN_LIST_REFRESH: Duration = Duration::from_secs(1);

/// How long a call waits for its server's task to connect again after a token
/// refresh or an expired session.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait before opening the tool-list change stream again after it
/// ends. Ending the stream is routine, so the wait is short enough that a
/// change is noticed soon, and long enough that a server which refuses the
/// stream is not asked in a tight loop.
const LISTEN_RETRY_DELAY: Duration = Duration::from_secs(5);

/// How long starting a step-up re-authorization may take. The flow start is a
/// few metadata requests the session's tool call waits behind, so a server
/// that stops answering must fail the call with a reason rather than hold it.
const STEP_UP_TIMEOUT: Duration = Duration::from_secs(30);

/// One connection per enabled server, plus the manual retry. Cloning shares
/// the same connections.
#[derive(Clone)]
pub struct McpManager {
    store: Store,
    oauth: McpOAuthContext,
    client: reqwest::Client,
    slots: Arc<RwLock<HashMap<String, Arc<ServerSlot>>>>,
}

impl McpManager {
    pub fn new(store: Store, oauth: McpOAuthContext, client: reqwest::Client) -> Self {
        Self {
            store,
            oauth,
            client,
            slots: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Starts one task per enabled server, each connecting at once. Returns
    /// without waiting: a server that is down must not hold up the control
    /// plane's start, and its failure is recorded on its own row.
    pub async fn start(&self) {
        let servers = match self.store.enabled_mcp_servers().await {
            Ok(servers) => servers,
            Err(error) => {
                error!(
                    error = %error.display_chain(),
                    "failed to load the enabled MCP servers"
                );
                return;
            }
        };
        for server in &servers {
            self.slot_for(&server.name);
        }
        info!(count = servers.len(), "MCP servers connecting");
    }

    /// Connects one named server now: its task starts when the manager has
    /// not seen the server before, and a server that is already connected is
    /// reconnected, so the attempt is real rather than a status read. A
    /// server the store does not hold, or holds disabled, is left alone.
    pub async fn connect(&self, name: &str) {
        if !self.enabled(name).await {
            return;
        }
        let (slot, started) = self.slot_for(name);
        // The row is read again now the slot exists. A server disabled or
        // deleted while this call was in flight is stopped again here, rather
        // than left serving sessions for want of a task to stop.
        if started && !self.enabled(name).await {
            self.stop_slot(name, &slot);
            return;
        }
        if !started {
            slot.wake.notify_one();
        }
    }

    /// Stops one named server: its task ends and its connection is dropped,
    /// so a disabled or deleted server is served to no session. A later
    /// connect starts it again on a fresh connection.
    pub fn stop(&self, name: &str) {
        let slot = self.slots.write().unwrap().remove(name);
        if let Some(slot) = slot {
            slot.stop();
        }
    }

    /// Stops one server's task when it is still the slot the manager holds,
    /// leaving a slot that a racing connect has already replaced.
    fn stop_slot(&self, name: &str, slot: &Arc<ServerSlot>) {
        let mut slots = self.slots.write().unwrap();
        let current = slots
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, slot));
        if current {
            slots.remove(name);
        }
        drop(slots);
        if current {
            slot.stop();
        }
    }

    /// The step-up authorization URL the operator must visit to give one
    /// server's token the scopes it lacks, or None when the pane is offered
    /// none. The offering goes away once the server connects again, and a URL
    /// whose flow is gone is dropped rather than offered, because the callback
    /// would only refuse it.
    pub async fn reauthorize_url(&self, name: &str) -> Option<String> {
        let slot = self.slot(name)?;
        let offered = slot.step_up()?;
        match self.oauth.pending_authorize(name).await {
            // Another flow replaced the one this URL names, which a fresh
            // authorize request does. The live flow's URL is the one the
            // operator must visit.
            Some(live) if live.url != offered.url => {
                slot.remember_step_up(live.url.clone(), offered.from_call);
                Some(live.url)
            }
            Some(_) => Some(offered.url),
            None => {
                // No flow is left to complete, so the offering goes with it,
                // and so does the step-up reason the row records: a reason
                // with no URL to answer it leaves the pane showing a step-up
                // the operator cannot act on.
                slot.clear_step_up();
                self.clear_step_up_failure(name).await;
                None
            }
        }
    }

    /// True when the store holds this server with its enabled flag set.
    async fn enabled(&self, name: &str) -> bool {
        match self.store.get_mcp_server(name).await {
            Ok(Some(server)) => server.enabled,
            Ok(None) => false,
            Err(error) => {
                error!(
                    server = %name,
                    error = %error.display_chain(),
                    "failed to read the MCP server"
                );
                false
            }
        }
    }

    /// The server's live connection, or None when it is down or unknown.
    fn connection(&self, name: &str) -> Option<Arc<Connection>> {
        self.slot(name)?.current()
    }

    /// The server's slot, or None when the manager has never seen the server.
    fn slot(&self, name: &str) -> Option<Arc<ServerSlot>> {
        self.slots.read().unwrap().get(name).cloned()
    }

    /// The server's slot, creating it and starting its task on first use.
    /// The flag is true when this call created the slot: its task connects on
    /// its own, so waking it would only make it reconnect.
    fn slot_for(&self, name: &str) -> (Arc<ServerSlot>, bool) {
        let mut slots = self.slots.write().unwrap();
        if let Some(slot) = slots.get(name) {
            return (slot.clone(), false);
        }
        let slot = Arc::new(ServerSlot::new());
        slot.attach(tokio::spawn(serve(
            self.clone(),
            name.to_string(),
            slot.clone(),
        )));
        slots.insert(name.to_string(), slot.clone());
        (slot, true)
    }

    /// Connects one stored server and lists its tools, with the credential
    /// its row holds and a refreshed token when the stored one has expired.
    /// A credential the server rejects is refreshed once and the connect and
    /// list are retried with it, because the row can hold a token the
    /// authorization server has since replaced. A challenge for scopes the
    /// token lacks starts a step-up re-authorization, whose URL is remembered
    /// on the slot. The failure is recorded only when the refresh fails too.
    async fn connect_and_list(
        &self,
        name: &str,
        slot: &Arc<ServerSlot>,
    ) -> anyhow::Result<Arc<Connection>> {
        let result = match self.connect_and_list_once(name).await {
            Err(error) if token_rejected(&error) => {
                self.refresh_token(name).await?;
                self.connect_and_list_once(name).await
            }
            result => result,
        };
        match result {
            Ok(connection) => Ok(connection),
            Err(error) => Err(self.step_up_error(name, slot, error).await),
        }
    }

    /// One connect and list, with no retry.
    async fn connect_and_list_once(&self, name: &str) -> anyhow::Result<Arc<Connection>> {
        let (client, expires_at_secs) = self.connect_client(name).await?;
        let listed = client
            .tools_list()
            .await
            .context("failed to list the server's tools")?;
        Ok(Arc::new(Connection {
            client,
            tools: listed.tools,
            listed_at: Instant::now(),
            ttl: listed.ttl_ms.map(Duration::from_millis),
            expires_at_secs,
        }))
    }

    /// Builds one server's client with its stored credential, refreshing an
    /// expired OAuth token first. Returns the client and the OAuth token's
    /// expiry, so a later call can tell when the token is due.
    async fn connect_client(&self, name: &str) -> anyhow::Result<(Arc<McpClient>, Option<i64>)> {
        let mut server = self.row(name).await?;
        // Only an OAuth connection carries a token whose expiry a call must
        // watch: a row switched away from OAuth keeps the old token columns.
        let mut expires_at_secs = match server.auth {
            McpAuth::OAuth => server.oauth_expires_at_secs,
            McpAuth::None | McpAuth::Bearer => None,
        };
        let token = match server.auth {
            McpAuth::None => None,
            McpAuth::Bearer => match server.bearer_token.as_deref().filter(|t| !t.is_empty()) {
                Some(token) => Some(resolve_api_key(token)?),
                None => None,
            },
            McpAuth::OAuth => {
                if token_expired(expires_at_secs) {
                    self.oauth
                        .refresh_mcp_token(name)
                        .await
                        .context("failed to refresh the OAuth token")?;
                    server = self.row(name).await?;
                    expires_at_secs = server.oauth_expires_at_secs;
                }
                server.oauth_access_token.clone()
            }
        };
        let mut client = McpClient::new(self.client.clone(), &server.url, token);
        client.connect().await.context("failed to connect")?;
        Ok((Arc::new(client), expires_at_secs))
    }

    /// Refreshes one server's OAuth token after the server rejected the
    /// stored one. A server that does not use OAuth has no token to refresh,
    /// so the rejection stands as the failure.
    async fn refresh_token(&self, name: &str) -> anyhow::Result<()> {
        if self.row(name).await?.auth != McpAuth::OAuth {
            anyhow::bail!("the server rejected the stored credential");
        }
        self.oauth
            .refresh_mcp_token(name)
            .await
            .context("failed to refresh the OAuth token")?;
        Ok(())
    }

    /// Lists the tools again on a live connection, returning the connection
    /// with the new list. A list the server refuses because the stored token
    /// was rejected is reconnected and listed again, because this
    /// connection's client holds the rejected token. A challenge for more
    /// scopes starts a step-up re-authorization and leaves the server down.
    async fn relist(
        &self,
        name: &str,
        slot: &Arc<ServerSlot>,
        connection: &Connection,
    ) -> anyhow::Result<Arc<Connection>> {
        match connection.client.tools_list().await {
            Ok(listed) => Ok(Arc::new(connection.with_tools(listed))),
            Err(McpClientError::Unauthorized) => self.connect_and_list(name, slot).await,
            Err(McpClientError::InsufficientScope { scope }) => Err(anyhow::Error::msg(
                self.start_step_up(name, slot, scope.as_deref(), false)
                    .await,
            )),
            Err(error) => Err(error).context("failed to list the server's tools again"),
        }
    }

    /// The error a failed connect or list reports, with an
    /// `insufficient_scope` challenge turned into one the operator can act
    /// on: the step-up flow starts and its URL is offered on the slot.
    async fn step_up_error(
        &self,
        name: &str,
        slot: &Arc<ServerSlot>,
        error: impl Into<anyhow::Error>,
    ) -> anyhow::Error {
        let error = error.into();
        let Some(scope) = challenge_scope(&error) else {
            return error;
        };
        anyhow::Error::msg(
            self.start_step_up(name, slot, scope.as_deref(), false)
                .await,
        )
    }

    /// Handles one `insufficient_scope` challenge: starts a re-authorization
    /// for the scopes the stored token was granted and the scope the challenge
    /// named, and offers the operator the URL it returns. Returns the line the
    /// row records, which names the scopes the live flow asks for. `from_call`
    /// is true when a tool call met the challenge, which is what makes the
    /// offering outlive the connect that follows it.
    async fn start_step_up(
        &self,
        name: &str,
        slot: &Arc<ServerSlot>,
        challenge_scope: Option<&str>,
        from_call: bool,
    ) -> String {
        let server = match self.row(name).await {
            Ok(server) => server,
            Err(error) => {
                return format!(
                    "the server requires more OAuth scopes, and its row could not be read: {}",
                    error.display_chain()
                );
            }
        };
        let union = union_scopes(
            server.oauth_scope.as_deref().unwrap_or(""),
            challenge_scope.unwrap_or(""),
        );
        if let Some(live) = self.oauth.pending_authorize(name).await {
            // A live flow wins over the newer challenge's scope: dropping a
            // flow the operator is part-way through in the browser is worse
            // than answering the newer challenge with the older flow's
            // scopes. It self-corrects, because the next challenge after the
            // callback completes starts a fresh flow.
            let from_call = from_call || slot.step_up().is_some_and(|step_up| step_up.from_call);
            slot.remember_step_up(live.url, from_call);
            return step_up_reason(&live.scope);
        }
        // A challenge that named no scope takes the resource's advertised
        // scopes too, inside the re-authorization's own discovery. With
        // neither a stored scope nor a challenged one there is nothing to ask
        // for, so the first-authorization request asks for what the resource
        // advertises.
        let started = tokio::time::timeout(STEP_UP_TIMEOUT, async {
            match union.is_empty() {
                true => self.oauth.start_authorization(&server).await,
                false => {
                    self.oauth
                        .reauthorize_with_scopes(&server, challenge_scope)
                        .await
                }
            }
        })
        .await;
        match started {
            Ok(Ok(url)) => {
                slot.remember_step_up(url, from_call);
                // The line names the scopes of the flow just started, which
                // the advertised scopes can widen past the union computed here.
                let scope = match self.oauth.pending_authorize(name).await {
                    Some(live) => live.scope,
                    None => union,
                };
                step_up_reason(&scope)
            }
            Ok(Err(error)) => format!(
                "the server requires more OAuth scopes, and the re-authorization could not start: {}",
                error.display_chain()
            ),
            Err(_elapsed) => "the server requires more OAuth scopes, and the re-authorization did not start in time".to_string(),
        }
    }

    /// Marks one server down for a step-up: the re-authorization starts, its
    /// URL is offered, the row records why, and the call reports an
    /// unavailable server.
    async fn step_up_unavailable(
        &self,
        server: &str,
        slot: &Arc<ServerSlot>,
        challenge_scope: Option<&str>,
    ) -> McpCallError {
        let reason = self
            .start_step_up(server, slot, challenge_scope, true)
            .await;
        self.mark_down(server, slot, &reason).await
    }

    /// The stored row, or an error when the server is gone.
    async fn row(&self, name: &str) -> anyhow::Result<McpServerSecret> {
        self.store
            .load_mcp_server(name)
            .await?
            .with_context(|| format!("MCP server {name} was not found"))
    }

    /// Refreshes one server's stored OAuth token, asks its task for a new
    /// connection built with it, and waits for that connection. A server with
    /// no token to refresh, a failed refresh, or a reconnect that does not
    /// arrive leaves the server down and reports why.
    async fn refresh_connection(
        &self,
        name: &str,
        slot: &Arc<ServerSlot>,
        rejected: &Arc<Connection>,
    ) -> Result<Arc<Connection>, McpCallError> {
        // A server the manager no longer holds is not refreshed or waited
        // for: its row may be gone, and no task connects again for it.
        self.ensure_live_slot(name, slot)?;
        let Some(server) = self
            .store
            .load_mcp_server(name)
            .await
            .map_err(|error| McpCallError::Internal(anyhow::Error::new(error)))?
        else {
            // A row the operator deleted is a server that is gone rather than
            // a store failure, and the caller can act on that.
            return Err(unavailable(name, "the server was deleted"));
        };
        if server.auth != McpAuth::OAuth {
            return Err(self
                .mark_down(name, slot, "the server rejected the stored credential")
                .await);
        }
        if let Err(error) = self.oauth.refresh_mcp_token(name).await {
            return Err(self.mark_down(name, slot, &error.display_chain()).await);
        }
        self.reconnect(name, slot, rejected).await
    }

    /// Replaces one connection the caller found unusable: drops it, asks the
    /// server's task to connect again, and waits for the new connection. A
    /// server the manager no longer holds is not waited for, and a reconnect
    /// that does not arrive leaves the server down and reports why.
    async fn reconnect(
        &self,
        name: &str,
        slot: &Arc<ServerSlot>,
        rejected: &Arc<Connection>,
    ) -> Result<Arc<Connection>, McpCallError> {
        self.ensure_live_slot(name, slot)?;
        slot.drop_connection();
        slot.wake.notify_one();
        match slot.wait_for_connection(rejected, RECONNECT_TIMEOUT).await {
            Some(connection) => Ok(connection),
            None => {
                // A slot the operator stopped is not a failure, and nothing
                // is recorded for it.
                self.ensure_live_slot(name, slot)?;
                Err(self
                    .mark_down(name, slot, "the server did not connect again")
                    .await)
            }
        }
    }

    /// `Ok` while `slot` is still the slot the manager holds for `name`. A
    /// deleted or disabled server has had its slot stopped, so no task
    /// connects again for it and its caller must not wait for one.
    fn ensure_live_slot(&self, name: &str, slot: &Arc<ServerSlot>) -> Result<(), McpCallError> {
        let live = self
            .slot(name)
            .is_some_and(|current| Arc::ptr_eq(&current, slot));
        match live {
            true => Ok(()),
            false => Err(unavailable(name, "the server is no longer connected")),
        }
    }

    /// Marks one server down: the connection is dropped, the reason is
    /// recorded on its row, and its task is asked to connect again. Returns
    /// the error the failed operation reports.
    async fn mark_down(&self, name: &str, slot: &Arc<ServerSlot>, reason: &str) -> McpCallError {
        slot.drop_connection();
        self.record_failure(name, reason).await;
        slot.wake.notify_one();
        unavailable(name, reason)
    }

    /// Records a connection failure on the server row. A store failure is
    /// logged: the caller can only report the connection failure anyway.
    async fn record_failure(&self, name: &str, reason: &str) {
        if let Err(error) = self.store.set_mcp_server_error(name, reason).await {
            error!(
                server = %name,
                error = %error.display_chain(),
                "failed to record the MCP server error"
            );
        }
    }

    /// Clears the row's recorded error once the server is connected.
    async fn clear_failure(&self, name: &str) {
        if let Err(error) = self.store.clear_mcp_server_error(name).await {
            error!(
                server = %name,
                error = %error.display_chain(),
                "failed to clear the MCP server error"
            );
        }
    }

    /// Clears the row's recorded error when it is a step-up reason, leaving an
    /// ordinary connection failure alone. Called when the offering a reason
    /// belongs to is dropped, so the reason cannot outlive the URL that
    /// answers it. The store checks the reason in the same statement that
    /// clears it, so a failure recorded in between survives.
    async fn clear_step_up_failure(&self, name: &str) {
        if let Err(error) = self
            .store
            .clear_mcp_server_error_with_prefix(name, STEP_UP_REASON)
            .await
        {
            error!(
                server = %name,
                error = %error.display_chain(),
                "failed to clear the MCP server error"
            );
        }
    }

    /// Calls one tool on one server, replacing a connection the server
    /// refuses because of the token or an expired session, and retrying the
    /// call once on the replacement.
    async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        progress: &UnboundedSender<Value>,
    ) -> Result<McpCallOutcome, McpCallError> {
        let slot = self
            .slot(server)
            .ok_or_else(|| unavailable(server, "the server is not connected"))?;
        let mut connection = slot
            .current()
            .ok_or_else(|| unavailable(server, "the server is not connected"))?;
        if connection.token_expired() {
            connection = self.refresh_connection(server, &slot, &connection).await?;
        }
        let advertised = connection
            .tool(tool)
            .ok_or_else(|| unavailable(server, &format!("it has no tool named {tool}")))?;
        match connection
            .client
            .tools_call(advertised, args.clone(), Some(progress))
            .await
        {
            Ok(outcome) => return Ok(outcome),
            Err(McpClientError::Unauthorized) => {
                connection = self.refresh_connection(server, &slot, &connection).await?;
            }
            // The client's contract is that the caller reconnects a legacy
            // session that expired; there is no token to refresh.
            Err(McpClientError::SessionExpired) => {
                connection = self.reconnect(server, &slot, &connection).await?;
            }
            // A challenge for more scopes is not a token the manager can
            // refresh, so the call fails as an unavailable server.
            Err(McpClientError::InsufficientScope { scope }) => {
                return Err(self
                    .step_up_unavailable(server, &slot, scope.as_deref())
                    .await);
            }
            Err(error) => return Err(McpCallError::Internal(anyhow::Error::new(error))),
        }

        // One retry on the replacement connection. A refusal of it too means
        // the server is unusable, not that a second replacement would help.
        let advertised = connection
            .tool(tool)
            .ok_or_else(|| unavailable(server, &format!("it has no tool named {tool}")))?;
        match connection
            .client
            .tools_call(advertised, args, Some(progress))
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(McpClientError::Unauthorized) => Err(self
                .mark_down(server, &slot, "the server rejected the refreshed token")
                .await),
            Err(McpClientError::SessionExpired) => Err(self
                .mark_down(server, &slot, "the server's session expired again")
                .await),
            Err(McpClientError::InsufficientScope { scope }) => Err(self
                .step_up_unavailable(server, &slot, scope.as_deref())
                .await),
            Err(error) => Err(McpCallError::Internal(anyhow::Error::new(error))),
        }
    }
}

impl McpConnections for McpManager {
    fn servers(&self, names: &[String]) -> McpAvailability {
        let mut servers = Vec::new();
        let mut unavailable = Vec::new();
        for name in names {
            match self.connection(name) {
                Some(connection) => servers.push((name.clone(), connection.tools.clone())),
                None => unavailable.push(name.clone()),
            }
        }
        McpAvailability {
            servers,
            unavailable,
        }
    }

    fn call(
        &self,
        server: String,
        tool: String,
        args: Value,
        delta: UnboundedSender<ToolDelta>,
    ) -> Pin<Box<dyn Future<Output = Result<McpCallOutcome, McpCallError>> + Send>> {
        let manager = self.clone();
        Box::pin(async move {
            // The server reports progress on the call's own response stream.
            // Those lines take the canonical tool path to the client, so the
            // notification is mapped to text and sent through the same delta
            // channel a canonical tool's output uses.
            let (progress, mut notifications) = mpsc::unbounded_channel::<Value>();
            let forwarding = tokio::spawn(async move {
                while let Some(message) = notifications.recv().await {
                    if let Some(text) = progress_text(&message) {
                        let _ = delta.send(ToolDelta { text });
                    }
                }
            });
            let outcome = manager.call_tool(&server, &tool, args, &progress).await;
            // Dropping the sender ends the forwarding task, which the caller
            // waits for so the last line reaches the session before the
            // result does.
            drop(progress);
            let _ = forwarding.await;
            outcome
        })
    }
}

/// One server's shared slot: the connection its task last installed, or None
/// while the server is down, plus the handle that asks its task for a new
/// connection and the task itself, so a stop can end it.
struct ServerSlot {
    connection: watch::Sender<Option<Arc<Connection>>>,
    /// A manual retry, or a caller that found the connection unusable: the
    /// server's task connects again.
    wake: Notify,
    /// The task serving this server. None once the slot has been stopped.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The step-up the pane is offered for this server, remembered until the
    /// server connects again.
    reauthorize: Mutex<Option<StepUp>>,
}

/// The step-up the pane is offered for one server: the URL the operator must
/// visit, and whether a tool call was what the server challenged. A call is
/// refused while the server still answers its tool list, so an offering a call
/// started outlives the connect that follows it.
#[derive(Clone)]
struct StepUp {
    url: String,
    from_call: bool,
}

impl ServerSlot {
    fn new() -> Self {
        Self {
            connection: watch::channel(None).0,
            wake: Notify::new(),
            task: Mutex::new(None),
            reauthorize: Mutex::new(None),
        }
    }

    /// Takes the task that serves this slot, so a later stop can abort it.
    fn attach(&self, task: tokio::task::JoinHandle<()>) {
        *self.task.lock().unwrap() = Some(task);
    }

    /// Ends the task serving this slot and drops its connection.
    fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
        self.drop_connection();
    }

    fn current(&self) -> Option<Arc<Connection>> {
        self.connection.borrow().clone()
    }

    /// Publishes a fresh connection. The server is up from here.
    fn publish(&self, connection: Arc<Connection>) {
        // `send` drops the value when no receiver is subscribed, which is the
        // usual case: readers borrow the latest value instead.
        self.connection.send_replace(Some(connection));
    }

    /// Drops the connection: the server is down until its task publishes one.
    fn drop_connection(&self) {
        self.connection.send_replace(None);
    }

    /// The step-up the pane is offered for this server, if any.
    fn step_up(&self) -> Option<StepUp> {
        self.reauthorize.lock().unwrap().clone()
    }

    /// Offers the operator the step-up that `url` continues.
    fn remember_step_up(&self, url: String, from_call: bool) {
        *self.reauthorize.lock().unwrap() = Some(StepUp { url, from_call });
    }

    /// Drops the offered step-up: the server connected, so the token it
    /// challenged is authorized.
    fn clear_step_up(&self) {
        *self.reauthorize.lock().unwrap() = None;
    }

    /// Waits for a connection other than `rejected`, which is the one the
    /// caller found unusable. Waiting on the slot's own channel keeps the
    /// caller out of a polling loop and cannot miss a publication. A slot
    /// whose task has been taken has been stopped, so there is nothing left
    /// to publish and the wait ends rather than running to its timeout.
    async fn wait_for_connection(
        &self,
        rejected: &Arc<Connection>,
        timeout: Duration,
    ) -> Option<Arc<Connection>> {
        let mut connection = self.connection.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(current) = connection.borrow_and_update().clone()
                && !Arc::ptr_eq(&current, rejected)
            {
                return Some(current);
            }
            if self.stopped() {
                return None;
            }
            tokio::select! {
                changed = connection.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }

    /// True once the task serving this slot has been taken by a stop.
    fn stopped(&self) -> bool {
        self.task.lock().unwrap().is_none()
    }
}

/// One server's live connection: the client, the tools it last reported with
/// the moment they were listed and the server's TTL, and the OAuth token's
/// expiry.
struct Connection {
    client: Arc<McpClient>,
    tools: Vec<McpTool>,
    listed_at: Instant,
    ttl: Option<Duration>,
    expires_at_secs: Option<i64>,
}

impl Connection {
    /// The same connection with the tools just listed.
    fn with_tools(&self, listed: McpToolList) -> Self {
        Self {
            client: self.client.clone(),
            tools: listed.tools,
            listed_at: Instant::now(),
            ttl: listed.ttl_ms.map(Duration::from_millis),
            expires_at_secs: self.expires_at_secs,
        }
    }

    /// The tool the server advertises under this name.
    fn tool(&self, name: &str) -> Option<&McpTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// True when the OAuth token has reached its stored expiry, so a call
    /// would use a token the server may already reject.
    fn token_expired(&self) -> bool {
        token_expired(self.expires_at_secs)
    }

    /// How long until the cached tools outlive the server's TTL, or None when
    /// the server named no TTL. A list that is already due reports the floor,
    /// which keeps a server that always says zero from making the manager
    /// list in a tight loop.
    fn refresh_in(&self) -> Option<Duration> {
        let remaining = self.ttl?.saturating_sub(self.listed_at.elapsed());
        Some(remaining.max(MIN_LIST_REFRESH))
    }
}

/// True when a stored OAuth token's expiry has passed. A server with no
/// stored expiry is never due.
fn token_expired(expires_at_secs: Option<i64>) -> bool {
    expires_at_secs.is_some_and(|expiry| bosun_common::time::unix_secs(SystemTime::now()) >= expiry)
}

/// True when an operation failed because the server rejected the stored
/// token, which a refresh can replace.
fn token_rejected(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref(), Some(McpClientError::Unauthorized))
}

/// The scope an `insufficient_scope` challenge named, when an operation
/// failed with one. The outer option is whether the challenge is there.
fn challenge_scope(error: &anyhow::Error) -> Option<Option<String>> {
    match error.downcast_ref::<McpClientError>() {
        Some(McpClientError::InsufficientScope { scope }) => Some(scope.clone()),
        _ => None,
    }
}

/// Every step-up reason starts with this prefix, which is how a recorded
/// error is told from an ordinary connection failure.
const STEP_UP_REASON: &str = "the server requires more OAuth scopes";

/// The line a step-up challenge leaves on the server row and reports to the
/// model: it names the scopes the flow asks for, which is what the operator
/// acts on. A challenge that named no scope against a row that stores none
/// leaves none to name.
fn step_up_reason(scopes: &str) -> String {
    match scopes.is_empty() {
        true => format!("{STEP_UP_REASON}; re-authorize"),
        false => format!("{STEP_UP_REASON}; re-authorize asking for {scopes}"),
    }
}

/// The error a call reports when the server cannot serve it.
fn unavailable(server: &str, reason: &str) -> McpCallError {
    McpCallError::Unavailable {
        server: server.to_string(),
        reason: reason.to_string(),
    }
}

/// The line one progress notification adds to the live session view. A
/// message that is not a progress notification adds nothing.
fn progress_text(message: &Value) -> Option<String> {
    if message.get("method").and_then(Value::as_str) != Some("notifications/progress") {
        return None;
    }
    let params = message.get("params")?;
    if let Some(text) = params.get("message").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let progress = params.get("progress")?;
    Some(match params.get("total") {
        Some(total) => format!("progress {progress}/{total}"),
        None => format!("progress {progress}"),
    })
}

/// Serves one server for as long as the manager lives: connect and follow the
/// tool list, and reconnect with backoff whenever the connection fails.
#[instrument(skip_all, fields(server = %name))]
async fn serve(manager: McpManager, name: String, slot: Arc<ServerSlot>) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match serve_connection(&manager, &name, &slot).await {
            // A wake asked for a fresh connection, so there is nothing to
            // wait for and no failure to record.
            Ok(()) => backoff = INITIAL_BACKOFF,
            Err(error) => {
                let reason = error.display_chain();
                warn!(error = %reason, "MCP server connection failed");
                slot.drop_connection();
                manager.record_failure(&name, &reason).await;
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = slot.wake.notified() => {}
                }
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Serves one live connection until it must be replaced: connect, list the
/// tools, and keep the list fresh on a change notification, at the server's
/// own TTL, or when the server refuses a listing. `Ok` means a wake asked for
/// a fresh connection; `Err` means the connection failed.
async fn serve_connection(
    manager: &McpManager,
    name: &str,
    slot: &Arc<ServerSlot>,
) -> Result<(), anyhow::Error> {
    let mut connection = manager.connect_and_list(name, slot).await?;
    slot.publish(connection.clone());
    // A step-up a tool call asked for stands while its flow is live: this
    // connect serves the server's tool list, but every call that needed the
    // missing scope is still refused until the operator authorizes it. A
    // step-up a connect asked for is done, because the server answered.
    let call_step_up = slot.step_up().is_some_and(|step_up| step_up.from_call)
        && manager.oauth.pending_authorize(name).await.is_some();
    if !call_step_up {
        slot.clear_step_up();
        manager.clear_failure(name).await;
    }
    info!(
        era = connection.client.era().map(ServerEra::version),
        "MCP server connected"
    );

    // The change stream is the server's own notification that its tool list
    // moved. A server from a revision before the current one has no such
    // stream; a stream that ends is opened again, and on the client the
    // connection holds at the time, because a refused listing replaces its
    // client with one built from the refreshed token.
    //
    // The channel holds one pending wake: a server that floods
    // `tools/list_changed` cannot grow it, and the manager re-lists once for
    // however many arrived.
    let (changed, mut changes) = mpsc::channel::<()>(1);
    let (client, mut current_client) = watch::channel(connection.client.clone());
    let has_change_stream = matches!(connection.client.era(), Some(ServerEra::Modern { .. }));
    if !has_change_stream {
        debug!("the server has no tools/list_changed stream");
    }
    let listen = listen_for_changes(&mut current_client, &changed);
    tokio::pin!(listen);

    loop {
        let refresh_in = connection.refresh_in();
        tokio::select! {
            // The stream is opened again inside this future, so it completes
            // only when the server refused the stream, which the caller
            // answers by connecting again: that refreshes a rejected token,
            // or starts a step-up for scopes the token lacks.
            error = &mut listen, if has_change_stream => {
                return Err(manager.step_up_error(name, slot, error).await);
            }
            Some(()) = changes.recv() => {
                connection = manager.relist(name, slot, &connection).await?;
                client.send_replace(connection.client.clone());
                slot.publish(connection.clone());
            }
            _ = tokio::time::sleep(refresh_in.unwrap_or_default()), if refresh_in.is_some() => {
                connection = manager.relist(name, slot, &connection).await?;
                client.send_replace(connection.client.clone());
                slot.publish(connection.clone());
            }
            _ = slot.wake.notified() => return Ok(()),
        }
    }
}

/// Opens one server's tool-list change stream on the client the connection
/// holds at the time, and opens it again after each end, until the caller
/// drops this future. Ending the stream is routine, so the cached list keeps
/// serving during the wait and the stream comes back without an unrelated
/// reconnect. A stream the server refuses because it rejected the client's
/// token, or because it challenged for scopes the token lacks, ends the loop:
/// the caller makes the connection again, which is what refreshes the token
/// or starts a step-up.
async fn listen_for_changes(
    current_client: &mut watch::Receiver<Arc<McpClient>>,
    changed: &mpsc::Sender<()>,
) -> McpClientError {
    loop {
        let client = current_client.borrow_and_update().clone();
        match client
            .subscriptions_listen(|| {
                // A full channel already holds the wake this notification
                // would add.
                let _ = changed.try_send(());
            })
            .await
        {
            Ok(()) => debug!("the tools/list_changed stream ended"),
            Err(
                error @ (McpClientError::Unauthorized | McpClientError::InsufficientScope { .. }),
            ) => return error,
            Err(error) => warn!(
                error = %error.display_chain(),
                "the tools/list_changed stream failed"
            ),
        }
        tokio::time::sleep(LISTEN_RETRY_DELAY).await;
    }
}

/// The next wait after a failed connect: twice the last one, capped so a
/// server that stays down is retried at a steady rate.
fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use bosun_test_support::wait_for;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::mcp_oauth::OAuthCallbackQuery;
    use crate::mcp_test_support::StubReply;
    use crate::mcp_test_support::StubRequest;
    use crate::mcp_test_support::accepted;
    use crate::mcp_test_support::discover_ok;
    use crate::mcp_test_support::empty_log;
    use crate::mcp_test_support::json_ok;
    use crate::mcp_test_support::notification;
    use crate::mcp_test_support::oauth_stub;
    use crate::mcp_test_support::result_message;
    use crate::mcp_test_support::sse_ok;
    use crate::mcp_test_support::stub_endpoint;
    use crate::mcp_test_support::text_status;

    /// A manager over `store`. The HTTP client times out, so a stub that stops
    /// answering fails its test instead of hanging it. The OAuth context
    /// carries a redirect URI, which a step-up flow needs to start.
    fn manager(store: &Store) -> McpManager {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        McpManager::new(
            store.clone(),
            McpOAuthContext::new(
                client.clone(),
                store.clone(),
                Some("https://control.example/mcp/oauth/callback".to_string()),
            ),
            client,
        )
    }

    /// One enabled, credential-free server row pointing at `url`.
    async fn insert_server(store: &Store, name: &str, url: &str) {
        let server = McpServerSecret {
            name: name.to_string(),
            url: url.to_string(),
            enabled: true,
            auth: McpAuth::None,
            bearer_token: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_access_token: None,
            oauth_refresh_token: None,
            oauth_expires_at_secs: None,
            oauth_scope: None,
            added_at_secs: 1,
            updated_at_secs: None,
            last_error: None,
        };
        store.insert_mcp_server(&server).await.unwrap();
    }

    /// A store with one enabled, credential-free server row pointing at `url`.
    async fn store_with_server(dir: &tempfile::TempDir, name: &str, url: &str) -> Store {
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        insert_server(&store, name, url).await;
        store
    }

    /// One enabled OAuth server row: the stored token is not expired, so a
    /// connect uses it rather than refreshing first.
    async fn insert_oauth_server(store: &Store, name: &str, url: &str) {
        let server = McpServerSecret {
            name: name.to_string(),
            url: url.to_string(),
            enabled: true,
            auth: McpAuth::OAuth,
            bearer_token: None,
            oauth_client_id: Some("client".to_string()),
            oauth_client_secret: None,
            oauth_access_token: Some("stale".to_string()),
            oauth_refresh_token: Some("refresh".to_string()),
            oauth_expires_at_secs: None,
            oauth_scope: None,
            added_at_secs: 1,
            updated_at_secs: None,
            last_error: None,
        };
        store.insert_mcp_server(&server).await.unwrap();
    }

    /// A store with one enabled OAuth server row pointing at `url`.
    async fn store_with_oauth_server(dir: &tempfile::TempDir, name: &str, url: &str) -> Store {
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        insert_oauth_server(&store, name, url).await;
        store
    }

    /// The methods the stub served, in order.
    fn messages(log: &Mutex<Vec<StubRequest>>) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .map(StubRequest::message)
            .collect()
    }

    /// How many times the stub served one method.
    fn method_count(log: &Mutex<Vec<StubRequest>>, method: &str) -> usize {
        messages(log)
            .iter()
            .filter(|served| served.as_str() == method)
            .count()
    }

    /// How many times the stub was asked for its tool list.
    fn list_count(log: &Mutex<Vec<StubRequest>>) -> usize {
        method_count(log, "tools/list")
    }

    /// The tool names the manager holds for one server, empty while it is
    /// down.
    fn cached_names(manager: &McpManager, name: &str) -> Vec<String> {
        manager
            .servers(&[name.to_string()])
            .servers
            .first()
            .map(|(_, tools)| tools.iter().map(|tool| tool.name.clone()).collect())
            .unwrap_or_default()
    }

    /// A modern server: it answers `server/discover`, serves `tools` with
    /// `ttl_ms` of cache, and holds a change stream open.
    fn modern_reply(request: &StubRequest, tools: &[&str], ttl_ms: u64) -> StubReply {
        match request.message().as_str() {
            "server/discover" => discover_ok(request),
            "tools/list" => json_ok(result_message(
                request.id(),
                json!({
                    "tools": tools
                        .iter()
                        .map(|name| json!({
                            "name": name,
                            "inputSchema": { "type": "object", "properties": {} },
                        }))
                        .collect::<Vec<Value>>(),
                    "ttlMs": ttl_ms,
                }),
            )),
            "subscriptions/listen" => sse_ok(&[], true),
            other => panic!("unexpected method {other}"),
        }
    }

    #[tokio::test]
    async fn a_modern_server_connects_and_caches_its_tools() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the modern server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;

        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
        // One listing: neither the change stream nor the TTL has asked for
        // another.
        assert_eq!(list_count(&log), 1);
        let row = store.get_mcp_server("srv").await.unwrap().unwrap();
        assert!(row.last_error.is_none());
    }

    #[tokio::test]
    async fn the_tool_list_is_served_from_the_cache_while_its_ttl_holds() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        // A minute of TTL outlives the reads below, so the manager must not
        // list again.
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        manager.connect("srv").await;
        wait_for("the server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;

        // The pause is what gives the server's task time to list again if it
        // wrongly decided the cache had expired.
        for _ in 0..5 {
            assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(list_count(&log), 1);
    }

    #[tokio::test]
    async fn a_legacy_server_connects_through_the_initialize_handshake() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                // A server with no `server/discover` refuses it over HTTP.
                "server/discover" => text_status(400, "unknown method"),
                "initialize" => json_ok(result_message(
                    request.id(),
                    json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": { "name": "legacy" },
                    }),
                ))
                .with_session_id("session-1"),
                "notifications/initialized" => accepted(),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "legacy-tool",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                    }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the legacy server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;

        assert_eq!(
            cached_names(&manager, "srv"),
            vec!["legacy-tool".to_string()]
        );
        assert!(messages(&log).contains(&"initialize".to_string()));
        assert_eq!(list_count(&log), 1);
    }

    #[tokio::test]
    async fn a_tools_list_changed_notification_re_lists_the_tools() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let listings = Arc::new(AtomicUsize::new(0));
        let url = stub_endpoint(
            move |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => {
                    let listing = listings.fetch_add(1, Ordering::SeqCst);
                    let name = if listing == 0 { "before" } else { "after" };
                    json_ok(result_message(
                        request.id(),
                        json!({
                            "tools": [{
                                "name": name,
                                "inputSchema": { "type": "object", "properties": {} },
                            }],
                            "ttlMs": 60_000,
                        }),
                    ))
                }
                "subscriptions/listen" => {
                    sse_ok(&[notification("notifications/tools/list_changed")], true)
                }
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        // The change notification re-lists the server's tools and publishes
        // them, with the cache still inside its TTL.
        wait_for("the re-listed tools", || {
            let manager = manager.clone();
            async move { cached_names(&manager, "srv") == vec!["after".to_string()] }
        })
        .await;
        assert_eq!(list_count(&log), 2);
    }

    #[tokio::test]
    async fn one_server_failing_leaves_another_serving() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "healthy", &url).await;
        // A second server on a port nothing listens on.
        insert_server(&store, "down", "http://127.0.0.1:1/mcp").await;
        let manager = manager(&store);

        manager.connect("healthy").await;
        manager.connect("down").await;

        wait_for("the healthy server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "healthy").is_empty() }
        })
        .await;
        wait_for("the failed connect on the other server's row", || {
            let store = store.clone();
            async move {
                store
                    .get_mcp_server("down")
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_some()
            }
        })
        .await;

        let availability = manager.servers(&["healthy".to_string(), "down".to_string()]);
        assert_eq!(availability.servers.len(), 1);
        assert_eq!(availability.servers[0].0, "healthy");
        assert_eq!(availability.unavailable, vec!["down".to_string()]);
    }

    #[tokio::test]
    async fn a_disabled_server_is_not_connected() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        store.set_mcp_server_enabled("srv", false).await.unwrap();
        let manager = manager(&store);

        manager.connect("srv").await;

        // No slot means no task, so the disabled server never reached the
        // stub at all.
        assert!(manager.slot("srv").is_none());
        assert!(messages(&log).is_empty());
        let availability = manager.servers(&["srv".to_string()]);
        assert!(availability.servers.is_empty());
        assert_eq!(availability.unavailable, vec!["srv".to_string()]);
    }

    #[tokio::test]
    async fn start_connects_the_enabled_servers_only() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "on", &url).await;
        let off_log = empty_log();
        let off_url = stub_endpoint(
            |request| modern_reply(request, &["off"], 60_000),
            off_log.clone(),
        )
        .await;
        insert_server(&store, "off", &off_url).await;
        store.set_mcp_server_enabled("off", false).await.unwrap();
        let manager = manager(&store);

        manager.start().await;

        wait_for("the enabled server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "on").is_empty() }
        })
        .await;
        // A disabled server gets no task, so its stub was never called.
        assert!(manager.slot("off").is_none());
        assert!(messages(&off_log).is_empty());
    }

    #[tokio::test]
    async fn a_failed_connect_retries_with_backoff_without_a_retry() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        // The stub refuses every request until the test says it is ready, so
        // the first connect fails and only the task's own backoff can recover.
        let ready = Arc::new(AtomicBool::new(false));
        let serving = ready.clone();
        let url = stub_endpoint(
            move |request| {
                if !serving.load(Ordering::SeqCst) {
                    return text_status(400, "not ready");
                }
                modern_reply(request, &["echo"], 60_000)
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;
        wait_for("the failure on the server row", || {
            let store = store.clone();
            async move {
                store
                    .get_mcp_server("srv")
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_some()
            }
        })
        .await;

        ready.store(true, Ordering::SeqCst);

        // Nothing asks for a retry here: the task connects again after its
        // backoff and clears the row's error.
        wait_for("the backoff retry to connect", || {
            let store = store.clone();
            async move {
                store
                    .get_mcp_server("srv")
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_none()
            }
        })
        .await;
        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
    }

    #[tokio::test]
    async fn connecting_a_connected_server_makes_a_new_connection() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        manager.connect("srv").await;
        wait_for("the server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;
        assert_eq!(list_count(&log), 1);

        // The retry, an edit, and an enable all ask for a real attempt rather
        // than a status read, so the server lists its tools again.
        manager.connect("srv").await;
        wait_for("the second listing", || {
            let log = log.clone();
            async move { list_count(&log) == 2 }
        })
        .await;
    }

    #[tokio::test]
    async fn a_server_switched_off_oauth_ignores_the_stale_token_expiry() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        // The row keeps an expiry from an earlier OAuth configuration, which
        // a bearer connection must not read as its own.
        store
            .set_mcp_oauth_tokens("srv", "stale", None, Some(1), None)
            .await
            .unwrap();
        let manager = manager(&store);

        // A slot the test holds itself: no task serves it, and it is only
        // where a step-up URL would be remembered.
        let slot = Arc::new(ServerSlot::new());
        let connection = manager.connect_and_list("srv", &slot).await.unwrap();

        assert!(!connection.token_expired());
    }

    #[tokio::test]
    async fn a_tool_call_forwards_progress_to_the_session_view() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                "subscriptions/listen" => sse_ok(&[], true),
                // The call answers on its own stream: a progress notification
                // first, then the result.
                "tools/call" => sse_ok(
                    &[
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": { "progress": 1, "total": 2, "message": "half way" },
                        }),
                        result_message(
                            request.id(),
                            json!({ "content": [{ "type": "text", "text": "done" }] }),
                        ),
                    ],
                    false,
                ),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        manager.connect("srv").await;
        wait_for("the server's tools", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;

        let (delta, mut deltas) = mpsc::unbounded_channel();
        let outcome = manager
            .call("srv".to_string(), "echo".to_string(), json!({}), delta)
            .await
            .unwrap();

        assert_eq!(outcome.text, "done");
        assert!(!outcome.is_error);
        // The call waits for the forwarding task, so the progress line is
        // already on the session's channel.
        let mut lines = Vec::new();
        while let Ok(line) = deltas.try_recv() {
            lines.push(line.text);
        }
        assert_eq!(lines, vec!["half way".to_string()]);
    }

    #[tokio::test]
    async fn a_down_server_records_its_error_and_a_retry_clears_it() {
        let dir = tempdir().unwrap();
        // A port nothing listens on, so the first connect fails at once.
        let store = store_with_server(&dir, "srv", "http://127.0.0.1:1/mcp").await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the failure on the server row", || {
            let store = store.clone();
            async move {
                store
                    .get_mcp_server("srv")
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_some()
            }
        })
        .await;
        let availability = manager.servers(&["srv".to_string()]);
        assert!(availability.servers.is_empty());
        assert_eq!(availability.unavailable, vec!["srv".to_string()]);

        // The operator points the row at a working server and retries.
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        store
            .update_mcp_server("srv", &url, McpAuth::None, None, None, None)
            .await
            .unwrap();
        manager.connect("srv").await;

        wait_for("the retry to connect", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;
        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
        // The connection is published before the row's error is cleared, so
        // the row is polled for that too rather than read once.
        wait_for("the row's error to clear", || {
            let store = store.clone();
            async move {
                store
                    .get_mcp_server("srv")
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_none()
            }
        })
        .await;
    }

    /// Connects one server and waits until its tools are cached.
    async fn connected(manager: &McpManager, name: &str) {
        manager.connect(name).await;
        wait_for("the server's tools", || {
            let manager = manager.clone();
            let name = name.to_string();
            async move { !cached_names(&manager, &name).is_empty() }
        })
        .await;
    }

    #[tokio::test]
    async fn an_expired_tool_list_is_listed_again() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        // A TTL far below the shortest wait between refreshes, so the cached
        // list is already due when the manager comes to look at it.
        let url = stub_endpoint(|request| modern_reply(request, &["echo"], 200), log.clone()).await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        wait_for("the TTL refresh to list again", || {
            let log = log.clone();
            async move { list_count(&log) >= 2 }
        })
        .await;

        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
    }

    #[tokio::test]
    async fn a_call_for_an_unknown_tool_is_unavailable() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let url = stub_endpoint(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        let (progress, _notifications) = mpsc::unbounded_channel();
        let error = manager
            .call_tool("srv", "missing", json!({}), &progress)
            .await
            .unwrap_err();

        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_call_on_a_down_server_is_unavailable() {
        let dir = tempdir().unwrap();
        // A port nothing listens on, so the server never connects.
        let store = store_with_server(&dir, "srv", "http://127.0.0.1:1/mcp").await;
        let manager = manager(&store);
        manager.connect("srv").await;
        wait_for("the failure on the server row", || {
            let store = store.clone();
            async move {
                store
                    .get_mcp_server("srv")
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_some()
            }
        })
        .await;

        let (progress, _notifications) = mpsc::unbounded_channel();
        let error = manager
            .call_tool("srv", "echo", json!({}), &progress)
            .await
            .unwrap_err();

        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_connect_refreshes_a_token_the_server_rejects() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        // The stored token is "stale" and the server takes only "access", so
        // the first connect is refused and the retry needs the refresh.
        let accepted = Arc::new(Mutex::new("access".to_string()));
        let (url, refreshes) = oauth_stub(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
            accepted,
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);

        connected(&manager, "srv").await;

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_access_token.as_deref(), Some("access"));
        // The refusal is not recorded: the retry connected.
        let row = store.get_mcp_server("srv").await.unwrap().unwrap();
        assert!(row.last_error.is_none());
    }

    #[tokio::test]
    async fn a_call_refreshes_a_token_the_server_rejects_and_retries() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let accepted = Arc::new(Mutex::new("stale".to_string()));
        let (url, refreshes) = oauth_stub(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                "subscriptions/listen" => sse_ok(&[], true),
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "content": [{ "type": "text", "text": "done" }] }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            accepted.clone(),
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        // The server replaces the token while the connection is live, so the
        // call is refused and the manager must refresh and call again.
        *accepted.lock().unwrap() = "access".to_string();

        let (progress, _notifications) = mpsc::unbounded_channel();
        let outcome = manager
            .call_tool("srv", "echo", json!({}), &progress)
            .await
            .unwrap();

        assert_eq!(outcome.text, "done");
        assert!(!outcome.is_error);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        // The retried call ran on a connection built with the new token.
        let retry = log
            .lock()
            .unwrap()
            .iter()
            .rfind(|request| request.message() == "tools/call")
            .map(|request| request.authorization.clone())
            .unwrap();
        assert_eq!(retry.as_deref(), Some("Bearer access"));
    }

    #[tokio::test]
    async fn a_call_reconnects_a_legacy_server_whose_session_expired() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let calls = Arc::new(AtomicUsize::new(0));
        let served = calls.clone();
        let url = stub_endpoint(
            move |request| match request.message().as_str() {
                // A server from an older revision: no `server/discover`.
                "server/discover" => text_status(400, "unknown method"),
                "initialize" => json_ok(result_message(
                    request.id(),
                    json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
                ))
                .with_session_id("session-1"),
                "notifications/initialized" => accepted(),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                    }),
                )),
                "tools/call" => {
                    // The first call meets a server that has forgotten the
                    // session; the retry runs on a session made after the
                    // reconnect.
                    if served.fetch_add(1, Ordering::SeqCst) == 0 {
                        text_status(404, "no such session")
                    } else {
                        json_ok(result_message(
                            request.id(),
                            json!({ "content": [{ "type": "text", "text": "done" }] }),
                        ))
                    }
                }
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        let (progress, _notifications) = mpsc::unbounded_channel();
        let outcome = manager
            .call_tool("srv", "echo", json!({}), &progress)
            .await
            .unwrap();

        assert_eq!(outcome.text, "done");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(method_count(&log, "initialize"), 2);
    }

    #[tokio::test]
    async fn the_change_stream_is_opened_again_after_it_ends() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        // The server ends the change stream at once, which is routine, and
        // names no TTL, so nothing else would re-list the tools.
        let url = stub_endpoint(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                    }),
                )),
                "subscriptions/listen" => sse_ok(&[], false),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        wait_for("the change stream to be opened again", || {
            let log = log.clone();
            async move { method_count(&log, "subscriptions/listen") >= 2 }
        })
        .await;

        // The cached list kept serving while the stream was gone.
        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
        assert_eq!(list_count(&log), 1);
    }

    #[tokio::test]
    async fn a_re_list_refreshes_a_token_the_server_rejects() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let accepted = Arc::new(Mutex::new("stale".to_string()));
        let (url, refreshes) = oauth_stub(
            // A TTL short enough that the manager re-lists while the test is
            // watching, so the refusal lands on a re-list.
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 200,
                    }),
                )),
                "subscriptions/listen" => sse_ok(&[], true),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            accepted.clone(),
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        // The server replaces the token while the connection is live, so the
        // next listing is refused.
        *accepted.lock().unwrap() = "access".to_string();

        wait_for("the refresh after the refused listing", || {
            let refreshes = refreshes.clone();
            async move { refreshes.load(Ordering::SeqCst) == 1 }
        })
        .await;

        // The tools stayed cached, and the refusal was not recorded: the
        // connection was made again with the new token.
        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
        let row = store.get_mcp_server("srv").await.unwrap().unwrap();
        assert!(row.last_error.is_none());
    }

    #[tokio::test]
    async fn a_burst_of_list_changed_notifications_re_lists_once() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let streams = Arc::new(AtomicUsize::new(0));
        let served = streams.clone();
        // Five changes arrive together on the first stream, as a broken
        // server would send them. The poll for the second stream is what says
        // the burst has been handled, and the server names no TTL, so no
        // other reason to list exists.
        let url = stub_endpoint(
            move |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                    }),
                )),
                "subscriptions/listen" => match served.fetch_add(1, Ordering::SeqCst) {
                    0 => sse_ok(
                        &[
                            notification("notifications/tools/list_changed"),
                            notification("notifications/tools/list_changed"),
                            notification("notifications/tools/list_changed"),
                            notification("notifications/tools/list_changed"),
                            notification("notifications/tools/list_changed"),
                        ],
                        false,
                    ),
                    _ => sse_ok(&[], false),
                },
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
        )
        .await;
        let store = store_with_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        wait_for("the change stream to be opened again", || {
            let log = log.clone();
            async move { method_count(&log, "subscriptions/listen") >= 2 }
        })
        .await;

        // The manager holds one wake however many changes arrived, so a
        // manager that listed once per change would have listed five times by
        // the time the stream was opened again.
        let listings = list_count(&log);
        assert!(
            listings <= 3,
            "five changes must not produce five listings, got {listings}"
        );
    }

    /// A connection for a slot test: nothing here reaches a server.
    fn held_connection() -> Arc<Connection> {
        let client = McpClient::new(reqwest::Client::new(), "http://127.0.0.1:1/mcp", None);
        Arc::new(Connection {
            client: Arc::new(client),
            tools: Vec::new(),
            listed_at: Instant::now(),
            ttl: None,
            expires_at_secs: None,
        })
    }

    #[tokio::test]
    async fn a_stopped_slot_is_not_waited_for() {
        let slot = Arc::new(ServerSlot::new());
        // A task that never publishes, as a server that is connecting is.
        slot.attach(tokio::spawn(std::future::pending::<()>()));
        let rejected = held_connection();

        slot.stop();

        let waited = tokio::time::timeout(
            Duration::from_secs(5),
            slot.wait_for_connection(&rejected, RECONNECT_TIMEOUT),
        )
        .await
        .expect("a stopped slot must not be waited for");

        assert!(waited.is_none());
    }

    #[tokio::test]
    async fn a_stopped_server_is_not_waited_for_after_a_token_refresh() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let (url, _refreshes) = oauth_stub(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;
        let slot = manager.slot("srv").unwrap();
        let connection = slot.current().unwrap();

        // The operator disables or deletes the server while a caller holds
        // the connection, so no task connects again for it.
        manager.stop("srv");

        let refreshed = tokio::time::timeout(
            Duration::from_secs(5),
            manager.refresh_connection("srv", &slot, &connection),
        )
        .await
        .expect("a stopped server must not be waited for");

        let error = match refreshed {
            Err(error) => error,
            Ok(_) => panic!("a stopped server must not be reconnected"),
        };
        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_deleted_server_is_not_waited_for_after_a_token_refresh() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let (url, _refreshes) = oauth_stub(
            |request| modern_reply(request, &["echo"], 60_000),
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;
        let slot = manager.slot("srv").unwrap();
        let connection = slot.current().unwrap();

        // A delete removes the row before it stops the task, so this is that
        // window: the row is gone and the slot is still the live one.
        store.remove_mcp_server("srv").await.unwrap();

        let refreshed = tokio::time::timeout(
            Duration::from_secs(5),
            manager.refresh_connection("srv", &slot, &connection),
        )
        .await
        .expect("a deleted server must not be waited for");

        let error = match refreshed {
            Err(error) => error,
            Ok(_) => panic!("a deleted server must not be reconnected"),
        };
        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");
    }

    #[tokio::test]
    async fn the_change_stream_is_opened_again_on_the_connection_in_use() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let accepted = Arc::new(Mutex::new("stale".to_string()));
        let (url, _refreshes) = oauth_stub(
            // The listing is due soon enough to meet the replaced token, and
            // the server ends every change stream at once, so the listener
            // opens streams on whichever client the connection holds.
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 200,
                    }),
                )),
                "subscriptions/listen" => sse_ok(&[], false),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            accepted.clone(),
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;

        // The server replaces the token, so the next listing is refused and
        // the manager connects again with the refreshed one.
        *accepted.lock().unwrap() = "access".to_string();

        wait_for("a change stream opened with the new token", || {
            let log = log.clone();
            async move {
                log.lock()
                    .unwrap()
                    .iter()
                    .rfind(|request| request.message() == "subscriptions/listen")
                    .and_then(|request| request.authorization.clone())
                    .as_deref()
                    == Some("Bearer access")
            }
        })
        .await;

        // Only the stream the connection already held was opened with the
        // retired token: the listener followed the replaced client instead of
        // asking the server again on the client it refuses.
        let retired = log
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.message() == "subscriptions/listen")
            .filter(|request| request.authorization.as_deref() == Some("Bearer stale"))
            .count();
        assert_eq!(
            retired, 1,
            "no stream is opened with the retired token again"
        );
    }

    /// The challenge a server sends when the token is missing a scope.
    const CHALLENGE_SCOPE: &str = "Bearer error=\"insufficient_scope\", scope=\"files.write\"";

    /// The `scope` query parameter of an authorization URL.
    fn authorize_scope(authorize_url: &str) -> String {
        reqwest::Url::parse(authorize_url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "scope")
            .map(|(_, value)| value.to_string())
            .unwrap_or_default()
    }

    /// The `state` query parameter of an authorization URL.
    fn authorize_state(authorize_url: &str) -> String {
        reqwest::Url::parse(authorize_url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.to_string())
            .unwrap()
    }

    /// An OAuth server row whose stored token was granted `files.read`, so a
    /// step-up has a granted set to union with the challenge's scope.
    async fn scoped_oauth_store(dir: &tempfile::TempDir, url: &str) -> Store {
        let store = store_with_oauth_server(dir, "srv", url).await;
        store
            .set_mcp_oauth_tokens("srv", "stale", Some("refresh"), None, Some("files.read"))
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn a_call_challenged_for_scopes_records_the_step_up_and_stops_serving_the_tools() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let challenging = Arc::new(AtomicBool::new(false));
        let challenged = challenging.clone();
        let (url, _refreshes) = oauth_stub(
            move |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(&[], true),
                // The server accepts the token but refuses the listing and the
                // call once the token's scopes are too few.
                "tools/list" | "tools/call" if challenged.load(Ordering::SeqCst) => {
                    text_status(403, "insufficient scope").with_www_authenticate(CHALLENGE_SCOPE)
                }
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "content": [{ "type": "text", "text": "done" }] }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;
        challenging.store(true, Ordering::SeqCst);

        let (progress, _notifications) = mpsc::unbounded_channel();
        let error = manager
            .call_tool("srv", "echo", json!({}), &progress)
            .await
            .unwrap_err();

        // The model reads an unavailable server, not an internal failure.
        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");
        // The URL the operator must visit asks for the union of the granted
        // scope and the challenged one.
        let authorize_url = manager.reauthorize_url("srv").await.expect("a step-up URL");
        assert_eq!(authorize_scope(&authorize_url), "files.read files.write");
        // The row records that the server is down, which is what the pane
        // shows the operator.
        assert!(
            store
                .get_mcp_server("srv")
                .await
                .unwrap()
                .unwrap()
                .last_error
                .is_some()
        );
        // The server serves no tools until the operator authorizes it, and the
        // connect that follows the challenge meets the same refusal.
        let availability = manager.servers(&["srv".to_string()]);
        assert!(availability.servers.is_empty());
        assert_eq!(availability.unavailable, vec!["srv".to_string()]);
    }

    #[tokio::test]
    async fn a_connect_challenged_for_scopes_clears_the_step_up_once_it_connects() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let challenging = Arc::new(AtomicBool::new(true));
        let challenged = challenging.clone();
        let (url, _refreshes) = oauth_stub(
            move |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(&[], true),
                "tools/list" if challenged.load(Ordering::SeqCst) => {
                    text_status(403, "insufficient scope").with_www_authenticate(CHALLENGE_SCOPE)
                }
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the step-up on the server's row", || {
            let manager = manager.clone();
            let store = store.clone();
            async move {
                manager.reauthorize_url("srv").await.is_some()
                    && store
                        .get_mcp_server("srv")
                        .await
                        .unwrap()
                        .unwrap()
                        .last_error
                        .is_some()
            }
        })
        .await;

        let authorize_url = manager.reauthorize_url("srv").await.unwrap();
        assert_eq!(authorize_scope(&authorize_url), "files.read files.write");
        assert!(manager.servers(&["srv".to_string()]).servers.is_empty());

        // The operator grants the scope, so the task's next attempt connects.
        challenging.store(false, Ordering::SeqCst);

        wait_for("the server to connect again", || {
            let manager = manager.clone();
            let store = store.clone();
            async move {
                !manager.servers(&["srv".to_string()]).servers.is_empty()
                    && manager.reauthorize_url("srv").await.is_none()
                    && store
                        .get_mcp_server("srv")
                        .await
                        .unwrap()
                        .unwrap()
                        .last_error
                        .is_none()
            }
        })
        .await;
        assert_eq!(cached_names(&manager, "srv"), vec!["echo".to_string()]);
    }

    #[tokio::test]
    async fn a_change_stream_challenged_for_scopes_starts_the_step_up() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let (url, _refreshes) = oauth_stub(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                // The server accepts the token for its tool list and wants a
                // scope it lacks for the change stream.
                "subscriptions/listen" => {
                    text_status(403, "insufficient scope").with_www_authenticate(CHALLENGE_SCOPE)
                }
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the step-up the refused change stream starts", || {
            let manager = manager.clone();
            let store = store.clone();
            async move {
                manager.reauthorize_url("srv").await.is_some()
                    && store
                        .get_mcp_server("srv")
                        .await
                        .unwrap()
                        .unwrap()
                        .last_error
                        .is_some()
            }
        })
        .await;

        let authorize_url = manager.reauthorize_url("srv").await.unwrap();
        assert_eq!(authorize_scope(&authorize_url), "files.read files.write");
    }

    #[tokio::test]
    async fn a_challenge_that_names_no_scope_asks_for_what_the_resource_advertises() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let (url, _refreshes) = oauth_stub(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                // The challenge names no scope, and the row stores none.
                "tools/list" => text_status(403, "insufficient scope")
                    .with_www_authenticate("Bearer error=\"insufficient_scope\""),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = store_with_oauth_server(&dir, "srv", &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the step-up URL", || {
            let manager = manager.clone();
            async move { manager.reauthorize_url("srv").await.is_some() }
        })
        .await;

        // With no scope to ask for, the flow asks for the scopes the resource
        // advertises.
        let authorize_url = manager.reauthorize_url("srv").await.unwrap();
        assert_eq!(authorize_scope(&authorize_url), "read write");
    }

    #[tokio::test]
    async fn a_scope_less_challenge_asks_for_the_stored_and_the_advertised_scopes() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let (url, _refreshes) = oauth_stub(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "tools/list" => text_status(403, "insufficient scope")
                    .with_www_authenticate("Bearer error=\"insufficient_scope\""),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        // The row's token was granted `files.read`, which on its own is what
        // the operator would be asked for again, and the same challenge would
        // come back.
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;

        wait_for("the step-up URL", || {
            let manager = manager.clone();
            async move { manager.reauthorize_url("srv").await.is_some() }
        })
        .await;

        let authorize_url = manager.reauthorize_url("srv").await.unwrap();
        assert_eq!(
            authorize_scope(&authorize_url),
            "files.read read write",
            "the stored scopes and the advertised ones are both asked for"
        );
    }

    #[tokio::test]
    async fn a_step_up_a_call_started_stands_while_the_server_still_serves_its_list() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let challenging = Arc::new(AtomicBool::new(false));
        let challenged = challenging.clone();
        let (url, _refreshes) = oauth_stub(
            move |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(&[], true),
                // Only the call needs the scope, so the server keeps serving
                // its tool list.
                "tools/call" if challenged.load(Ordering::SeqCst) => {
                    text_status(403, "insufficient scope").with_www_authenticate(CHALLENGE_SCOPE)
                }
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "content": [{ "type": "text", "text": "done" }] }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;
        challenging.store(true, Ordering::SeqCst);

        let (progress, _notifications) = mpsc::unbounded_channel();
        let error = manager
            .call_tool("srv", "echo", json!({}), &progress)
            .await
            .unwrap_err();

        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");
        // The server's task serves the tool list again, which must not take
        // the step-up away: the calls that need the scope keep failing until
        // the operator authorizes the server.
        wait_for("the server's tools to be served again", || {
            let manager = manager.clone();
            async move { !cached_names(&manager, "srv").is_empty() }
        })
        .await;

        let authorize_url = manager
            .reauthorize_url("srv")
            .await
            .expect("the offering a call started");
        assert_eq!(authorize_scope(&authorize_url), "files.read files.write");
        assert!(
            store
                .get_mcp_server("srv")
                .await
                .unwrap()
                .unwrap()
                .last_error
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_offering_follows_the_live_flow_and_is_dropped_when_the_flow_is_gone() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let (url, _refreshes) = oauth_stub(
            |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(&[], true),
                // The challenge keeps the server down, so the offering is not
                // ended by a connect.
                "tools/list" => {
                    text_status(403, "insufficient scope").with_www_authenticate(CHALLENGE_SCOPE)
                }
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);

        manager.connect("srv").await;
        wait_for("the step-up URL", || {
            let manager = manager.clone();
            async move { manager.reauthorize_url("srv").await.is_some() }
        })
        .await;
        let offered = manager.reauthorize_url("srv").await.unwrap();
        // The task is stopped so nothing starts a further flow while the test
        // looks at the offering.
        manager.slot("srv").unwrap().stop();

        // A fresh authorize request for the same server replaces the flow, and
        // the pane is offered the flow that is live.
        let server = store.load_mcp_server("srv").await.unwrap().unwrap();
        let replacement = manager.oauth.start_authorization(&server).await.unwrap();
        assert_ne!(replacement, offered);
        assert_eq!(
            manager.reauthorize_url("srv").await.as_deref(),
            Some(replacement.as_str())
        );

        // Completing that flow consumes it, so no URL is offered any more.
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(authorize_state(&replacement)),
            error: None,
            iss: None,
        };
        assert_eq!(
            manager.oauth.complete_authorization(&query).await.unwrap(),
            "srv"
        );
        assert_eq!(manager.reauthorize_url("srv").await, None);
    }

    #[tokio::test]
    async fn dropping_the_offering_clears_the_step_up_reason_with_it() {
        let dir = tempdir().unwrap();
        let log = empty_log();
        let challenging = Arc::new(AtomicBool::new(false));
        let challenged = challenging.clone();
        let (url, _refreshes) = oauth_stub(
            move |request| match request.message().as_str() {
                "server/discover" => discover_ok(request),
                "subscriptions/listen" => sse_ok(&[], true),
                // Only the call needs the scope, so the server keeps serving
                // its tool list and stays connected.
                "tools/call" if challenged.load(Ordering::SeqCst) => {
                    text_status(403, "insufficient scope").with_www_authenticate(CHALLENGE_SCOPE)
                }
                "tools/list" => json_ok(result_message(
                    request.id(),
                    json!({
                        "tools": [{
                            "name": "echo",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                        "ttlMs": 60_000,
                    }),
                )),
                "tools/call" => json_ok(result_message(
                    request.id(),
                    json!({ "content": [{ "type": "text", "text": "done" }] }),
                )),
                other => panic!("unexpected method {other}"),
            },
            log.clone(),
            Arc::new(Mutex::new("stale".to_string())),
        )
        .await;
        let store = scoped_oauth_store(&dir, &url).await;
        let manager = manager(&store);
        connected(&manager, "srv").await;
        challenging.store(true, Ordering::SeqCst);

        let (progress, _notifications) = mpsc::unbounded_channel();
        let error = manager
            .call_tool("srv", "echo", json!({}), &progress)
            .await
            .unwrap_err();
        assert!(matches!(error, McpCallError::Unavailable { .. }), "{error}");

        let offered = manager
            .reauthorize_url("srv")
            .await
            .expect("the offering a call started");
        // The task is stopped so nothing else clears the row while the test
        // looks at it.
        manager.slot("srv").unwrap().stop();
        assert!(
            store
                .get_mcp_server("srv")
                .await
                .unwrap()
                .unwrap()
                .last_error
                .is_some()
        );

        // The callback consumes the flow without storing tokens, so the URL
        // the pane was offered can no longer be completed.
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(authorize_state(&offered)),
            error: None,
            iss: Some("http://elsewhere.example".to_string()),
        };
        assert!(manager.oauth.complete_authorization(&query).await.is_err());

        // The reason goes with the URL it belongs to: the server must not keep
        // showing a step-up the operator cannot act on.
        assert_eq!(manager.reauthorize_url("srv").await, None);
        assert_eq!(
            store
                .get_mcp_server("srv")
                .await
                .unwrap()
                .unwrap()
                .last_error,
            None
        );
    }

    #[tokio::test]
    async fn dropping_an_offering_leaves_a_failure_that_is_not_a_step_up_reason() {
        let dir = tempdir().unwrap();
        let store = store_with_oauth_server(&dir, "srv", "http://127.0.0.1:1/mcp").await;
        let manager = manager(&store);

        // A connect failure is the operator's to see, so it survives.
        store
            .set_mcp_server_error("srv", "failed to connect")
            .await
            .unwrap();
        manager.clear_step_up_failure("srv").await;
        assert_eq!(
            store
                .get_mcp_server("srv")
                .await
                .unwrap()
                .unwrap()
                .last_error
                .as_deref(),
            Some("failed to connect")
        );

        store
            .set_mcp_server_error("srv", &step_up_reason("files.read"))
            .await
            .unwrap();
        manager.clear_step_up_failure("srv").await;
        assert_eq!(
            store
                .get_mcp_server("srv")
                .await
                .unwrap()
                .unwrap()
                .last_error,
            None
        );
    }
}
