use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::future::IntoFuture;
use std::io::IsTerminal;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use bosun_agent::adapters::provider_for;
use bosun_agent::config::resolve_api_key;
use bosun_agent::config::resolve_model;
use bosun_agent::provider::Provider;
use bosun_agent::provider::ProviderError;
use bosun_common::config::CliConfig;
use bosun_common::config::ControlConfig;
use bosun_common::config::NodeConfig;
use bosun_common::config::cli_config_path;
use bosun_common::config::load_cli_config;
use bosun_common::config::load_config;
use bosun_common::config::save_cli_config;
#[cfg(windows)]
use bosun_common::error::ErrorExt;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use bosun_common::session::SessionView;
use bosun_common::telemetry::setup_logging;
use bosun_common::types::CloneRequest;
use bosun_common::types::DevRequest;
use bosun_common::types::DirEntry;
use bosun_common::types::DirListing;
use bosun_common::types::StopRequest;
use bosun_common::types::UpdateStatus;
use bosun_common::types::X_BOSUN_VERSION;
use bosun_common::version::VERSION;
use bosun_common::version::compare;
use bosun_control::api::AppState;
use bosun_control::commands::CommandQueue;
use bosun_control::loops::AgentRegistry;
use bosun_control::mcp_manager::McpManager;
use bosun_control::projects::ProjectHub;
use bosun_control::registry::NodeHealth;
use bosun_control::registry::NodeRegistry;
use bosun_control::skills_repos::GitHubClient;
use bosun_control::tunnel::TunnelRegistry;
use bosun_node::manager::NodeManager;
use bosun_store::store::Store;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use dialoguer::FuzzySelect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use reqwest::header::HeaderMap;
#[cfg(windows)]
use tracing::error;
use tracing::info;
use tracing::warn;

mod attach;
mod crew;
mod markdown;
mod update;

#[derive(Parser)]
#[command(name = "bosun", version, about = "Control panel for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane.
    Serve(ServeArgs),
    /// Run a node daemon.
    Node(NodeArgs),
    /// List nodes registered with the control plane.
    Nodes(NodesArgs),
    /// Clone a repository on a node and start a session.
    Clone(CloneArgs),
    /// Start a session in an existing directory on a node, picked interactively.
    Dev(DevArgs),
    /// List sessions on the control plane.
    List(ListArgs),
    /// Attach to a session interactively.
    Open(OpenArgs),
    /// Manage the CLI config file.
    Config(ConfigArgs),
    /// Stop a session and remove it from the node.
    Stop(StopArgs),
    /// Update this binary to the control plane's version, or demand updates
    /// from nodes. The self-update fetches its binary from the release feed:
    /// BOSUN_UPDATE_BASE_URL when set, else GitHub Releases.
    Update(UpdateArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Path to the control-plane config file.
    #[arg(long)]
    config: PathBuf,
    /// Override the log filter. Defaults to RUST_LOG, then info.
    #[arg(long)]
    log_filter: Option<String>,
}

#[derive(Args)]
struct NodeArgs {
    /// Path to the node config file. Required for a node boot, not for
    /// `--rollback`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the log filter. Defaults to RUST_LOG, then info.
    #[arg(long)]
    log_filter: Option<String>,
    /// Revert to the previous binary and restart into it, or swap and stop
    /// when no --config is given.
    #[arg(long)]
    rollback: bool,
}

#[derive(Args)]
struct NodesArgs {
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct CloneArgs {
    /// Node to clone the repository on.
    #[arg(long)]
    node: String,
    /// Git repository URL.
    repo_url: String,
    /// Git ref to check out. Defaults to the remote default branch.
    git_ref: Option<String>,
    /// Persona to run the session with. Defaults to the control plane's default persona.
    #[arg(long)]
    persona: Option<String>,
    /// First instruction for the session.
    #[arg(long)]
    message: Option<String>,
    /// MCP server to make available to the session. Repeatable; defaults to none.
    #[arg(long = "mcp", value_name = "NAME")]
    mcp: Vec<String>,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct DevArgs {
    /// Node whose directories to browse.
    #[arg(long)]
    node: String,
    /// Persona to run the session with. Defaults to the control plane's default persona.
    #[arg(long)]
    persona: Option<String>,
    /// First instruction for the session.
    #[arg(long)]
    message: Option<String>,
    /// MCP server to make available to the session. Repeatable; defaults to none.
    #[arg(long = "mcp", value_name = "NAME")]
    mcp: Vec<String>,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct ListArgs {
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct OpenArgs {
    /// Session id to connect to. Picked from a list when omitted.
    session_id: Option<String>,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Store the control-plane base URL in the CLI config file.
    Set(ConfigSetArgs),
    /// Print the stored control-plane base URL and the config file path.
    Get,
    /// Reset the stored control-plane base URL to the default.
    Unset,
}

#[derive(Args)]
struct ConfigSetArgs {
    /// Config key. Only cp-url is supported.
    key: String,
    /// Value to store.
    value: String,
}

#[derive(Args)]
struct StopArgs {
    /// Session id to stop.
    session_id: String,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct UpdateArgs {
    /// Nodes to command an update on. When omitted, updates this binary.
    nodes: Vec<String>,
    /// Allow a downgrade to the control plane's version.
    #[arg(long)]
    force: bool,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    if update::update_marker_is_set() {
        let result = update::finalize_update().await;
        if let Err(error) = result {
            error!(error = %error.display_chain(), "failed to install the update");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => run_serve(args).await,
        Command::Node(args) => run_node(args).await,
        Command::Nodes(args) => run_nodes(args).await,
        Command::Clone(args) => run_clone(args).await,
        Command::Dev(args) => run_dev(args).await,
        Command::List(args) => run_list(args).await,
        Command::Open(args) => run_open(args).await,
        Command::Config(args) => run_config(args),
        Command::Stop(args) => run_stop(args).await,
        Command::Update(args) => run_update_cmd(args).await,
    }
}

fn resolve_cp_url(flag: Option<&str>) -> anyhow::Result<String> {
    let stored = load_cli_config()?.cp_url;
    Ok(resolve_cp_url_from(
        flag,
        env::var("BOSUN_CP_URL").ok(),
        stored,
    ))
}

fn resolve_cp_url_from(flag: Option<&str>, env_url: Option<String>, stored: String) -> String {
    flag.map(ToString::to_string).or(env_url).unwrap_or(stored)
}

/// Resolves the configured GitHub token. `env:VAR` reads the environment;
/// any other value is a literal. An empty literal means no token, exactly
/// like an absent one, so the client never sends an empty Authorization
/// header. The resolved value only lives in the GitHub client and is never
/// stored or exposed.
fn resolve_github_token(github_token: &Option<String>) -> Result<Option<String>, ProviderError> {
    Ok(github_token
        .as_deref()
        .map(resolve_api_key)
        .transpose()?
        .filter(|token| !token.is_empty()))
}

/// Builds the HTTP client the CLI uses to reach the control plane. When
/// `BOSUN_CA_CERT` names a PEM file, the client trusts it, so a control plane
/// behind a private CA (or self-signed certificate) can be reached.
fn cp_client() -> anyhow::Result<reqwest::Client> {
    let ca_cert = std::env::var("BOSUN_CA_CERT")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    bosun_common::tls::reqwest_client(ca_cert.as_deref())
}

async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    setup_logging(args.log_filter.as_deref())?;
    let mut config: ControlConfig =
        load_config(&args.config).context("failed to load control-plane config")?;
    // The error's Display lists every problem on its own line.
    config.validate_personas()?;
    if let Some(redirect_uri) = config.oauth_redirect_uri.as_deref() {
        // The callback route registers here; a path the router cannot serve
        // or already serves would leave the callback unreachable, so the
        // operator's flow could never complete.
        bosun_control::api::oauth_callback_path(redirect_uri)?;
    }
    info!(
        listen_addr = %config.listen_addr,
        node_timeout_secs = config.node_timeout_secs,
        "control plane configured"
    );

    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create data directory {}",
                config.data_dir.display()
            )
        })?;
    // Prompt files are optional, but the directory exists so an operator knows
    // where to drop `<persona name>.md` files.
    let personas_dir = config.data_dir.join("personas");
    tokio::fs::create_dir_all(&personas_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create personas directory {}",
                personas_dir.display()
            )
        })?;
    config
        .load_persona_prompts()
        .context("failed to load persona prompts")?;
    let store = Store::open(&config.data_dir.join("store.db")).with_context(|| {
        format!(
            "failed to open the session store at {}",
            config.data_dir.display()
        )
    })?;

    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    let mut prices: HashMap<String, (f64, f64)> = HashMap::new();
    let mut model_names: Vec<&String> = config.models.keys().collect();
    model_names.sort();
    for name in model_names {
        let resolved = resolve_model(&config.models, Some(name))?;
        let provider = provider_for(&resolved)?;
        providers.insert(name.clone(), Arc::from(provider));
        prices.insert(
            name.clone(),
            (
                resolved.config.price_input_per_mtok,
                resolved.config.price_output_per_mtok,
            ),
        );
    }
    if providers.is_empty() {
        warn!("no models configured; sessions cannot run");
    }
    if config.personas.is_empty() {
        warn!("no personas configured; sessions cannot run");
    }

    let github_token = resolve_github_token(&config.github_token)?;
    let mcp_oauth = bosun_control::mcp_oauth::McpOAuthContext::new(
        reqwest::Client::new(),
        store.clone(),
        config.oauth_redirect_uri.clone(),
    );
    // Every enabled server connects in its own task, so a server that is down
    // delays nothing here and does not stop the control plane from starting.
    let mcp = Arc::new(McpManager::new(
        store.clone(),
        mcp_oauth.clone(),
        reqwest::Client::new(),
    ));
    mcp.start().await;
    let mut loops = AgentRegistry::new(providers.clone(), config.personas.clone(), prices);
    loops.mcp = Some(mcp.clone());
    loops.nudge = config.nudge;
    let (tool_activity, tool_activity_rx) = tokio::sync::mpsc::unbounded_channel();
    loops.tool_activity = Some(tool_activity);
    let github = GitHubClient::new(
        "https://api.github.com",
        "https://raw.githubusercontent.com",
        github_token,
    );
    let projects = Arc::new(ProjectHub::new(
        Duration::from_secs(config.merged_branch_hours * 3600),
        github.has_token(),
    ));

    let state = Arc::new(AppState {
        registry: Arc::new(NodeRegistry::new(Duration::from_secs(
            config.node_timeout_secs,
        ))),
        commands: Arc::new(CommandQueue::new(Duration::from_secs(
            config.node_timeout_secs,
        ))),
        tunnels: Arc::new(TunnelRegistry::new()),
        store,
        github: github.clone(),
        loops: Arc::new(loops),
        providers,
        personas: config.personas,
        default_persona: config.default_persona,
        oauth_redirect_uri: config.oauth_redirect_uri,
        mcp_oauth,
        projects: projects.clone(),
        mcp,
    });
    // The loops' `spawn` tool starts child sessions through the registry, so
    // the registry needs the node-facing handles before any loop runs.
    state.loops.attach_child_spawner(
        state.registry.clone(),
        state.commands.clone(),
        state.tunnels.clone(),
    );
    bosun_control::api::recover(&state).await;
    tokio::spawn(bosun_control::projects::run(
        projects.clone(),
        state.store.clone(),
        state.tunnels.clone(),
        tool_activity_rx,
    ));
    tokio::spawn(bosun_control::projects::run_pull_requests(
        projects,
        state.store.clone(),
        github,
    ));
    let app = bosun_control::api::router(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;

    match (config.tls_cert.as_deref(), config.tls_key.as_deref()) {
        (Some(cert), Some(key)) => {
            let server_config = bosun_common::tls::load_server_config(cert, key)
                .context("failed to load the control-plane TLS certificate")?;
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
            serve_until_stopped(
                |stopped| async move {
                    serve_tls(listener, app, acceptor, &config.listen_addr, stopped).await
                },
                shutdown_signal(),
            )
            .await
        }
        (None, None) => {
            info!(listen_addr = %config.listen_addr, "control plane listening");
            serve_until_stopped(
                |stopped| async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            let _ = stopped.await;
                        })
                        .into_future()
                        .await
                        .context("control plane server failed")
                },
                shutdown_signal(),
            )
            .await
        }
        _ => Err(anyhow::anyhow!("tls_cert and tls_key must be set together")),
    }
}

async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    acceptor: tokio_rustls::TlsAcceptor,
    listen_addr: &str,
    stopped: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    info!(listen_addr = %listen_addr, "control plane listening (TLS)");
    let mut stopped = stopped;
    // The connections are kept, not detached: the caller bounds how long they
    // are waited for, and a client that holds a stream open is why that bound
    // exists.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted.context("control plane accept failed")?,
            _ = &mut stopped => break,
        };
        let acceptor = acceptor.clone();
        let service = hyper_util::service::TowerToHyperService::new(app.clone().into_service());
        connections.spawn(async move {
            let stream = acceptor
                .accept(stream)
                .await
                .context("TLS handshake failed")?;
            let io = hyper_util::rt::TokioIo::new(stream);
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await
                .map_err(|error| anyhow::anyhow!("TLS connection failed: {error}"))?;
            Ok::<(), anyhow::Error>(())
        });
    }
    // The listener is closed. Every connection that is still open is a client
    // that has not finished, and the caller gives them the drain before it
    // stops the process.
    while connections.join_next().await.is_some() {}
    Ok(())
}

/// Serves until the process is asked to stop, then waits for the connections the
/// stop leaves open. A pane's event stream never completes on its own, so the
/// wait is bounded: the clients reconnect from their cursors, and a service
/// manager that waited for the process would run out its own timeout and kill it
/// less politely.
///
/// `serve` receives the future that fires when the wait begins: the plain path
/// hands it to axum's graceful shutdown, the TLS path stops accepting and joins
/// the connections it opened, so both paths drain the same way.
async fn serve_until_stopped<S, F>(
    serve: S,
    stop: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()>
where
    S: FnOnce(tokio::sync::oneshot::Receiver<()>) -> F,
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let (stopping, stopped) = tokio::sync::oneshot::channel();
    let mut serving = Box::pin(serve(stopped));
    tokio::select! {
        result = &mut serving => result,
        _ = stop => {
            let _ = stopping.send(());
            match tokio::time::timeout(SHUTDOWN_DRAIN, &mut serving).await {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        secs = SHUTDOWN_DRAIN.as_secs(),
                        "streams are still open after the shutdown drain; stopping anyway"
                    );
                    Ok(())
                }
            }
        }
    }
}

/// How long a stopping control plane waits for its open connections to finish.
/// An event stream a client holds open never finishes on its own, so the wait
/// has to end somewhere: the streams are clients that reconnect, and a service
/// manager that waits for the process would kill it less politely.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// Resolves when the process is asked to stop: Ctrl-C in a terminal, or the
/// SIGTERM a service manager sends when a unit is stopped or restarted. A
/// process that watched only for Ctrl-C is killed outright under systemd, with
/// no chance to close what it holds: for the node, that is the shells it is
/// running.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(error) => {
                    warn!(error = %error, "failed to watch for SIGTERM; watching Ctrl-C alone");
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn run_node(args: NodeArgs) -> anyhow::Result<()> {
    setup_logging(args.log_filter.as_deref())?;
    #[cfg(windows)]
    if let Err(error) = bosun_node::update::finalize_staged_update().await {
        error!(error = %error.display_chain(), "failed to install the staged update; continuing from the staged path");
    }
    #[cfg(windows)]
    if let Err(error) = bosun_node::update::finalize_rollback().await {
        error!(error = %error.display_chain(), "failed to finalize the rollback; continuing from the current binary");
    }
    if args.rollback {
        // Without --config the rollback swaps and stops here; with --config
        // it restarts the process and never returns.
        bosun_node::update::rollback(args.config.is_none())
            .await
            .context("node rollback failed")?;
        return Ok(());
    }
    let config_path = args
        .config
        .as_deref()
        .context("node boot requires --config")?;
    let config: NodeConfig = load_config(config_path).context("failed to load node config")?;
    info!(
        cp_url = %config.cp_url,
        node_name = %config.node_name,
        "node configured"
    );

    let tls_config =
        bosun_common::tls::load_client_config(config.ca_cert.as_deref())?.map(Arc::new);
    let manager = Arc::new(NodeManager::new(
        config.work_dir.clone(),
        config.browse_roots.clone(),
        config.cp_url.clone(),
        tls_config.clone(),
    ));
    // The one outbound tunnel starts at boot and reconnects on its own, so
    // every session's tool calls reach the control plane as soon as it is up.
    manager.start_node_tunnel(&config.node_name);
    manager.restore().await;
    // Sessions are restored and the tunnel is up, so the node is as ready
    // as it gets. Under Type=notify the unit is not started until this.
    bosun_node::notify::ready();

    let polling = manager.clone();
    tokio::select! {
        _ = bosun_node::poll::run_poll_loop(
            config.cp_url.clone(),
            config.node_name.clone(),
            polling,
            tls_config,
            config.update.enabled,
            config.update.base_url.clone(),
            bosun_node::poll::UPDATE_RETRY_DELAY,
        ) => {}
        _ = shutdown_signal() => {}
    }
    // The node is going down: kill the shells it is running, so a restart
    // cannot leave a command running with nothing left to report its exit to.
    // Nothing re-runs that call by itself: the control plane records the
    // failure, and a boot that resumes the session is what re-issues it.
    manager.kill_all_shells().await;
    Ok(())
}

async fn run_nodes(args: NodesArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;

    let client = cp_client()?;
    let response = client
        .get(format!("{cp_url}/nodes"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("control plane at {cp_url} returned an error"))?;
    maybe_print_update_notice(response.headers());
    let health: Vec<NodeHealth> = response.json().await.context("failed to parse node list")?;

    let now = SystemTime::now();
    if health.is_empty() {
        println!("no nodes registered");
        return Ok(());
    }
    println!(
        "{:<16}  {:<6}  {:<10}  {:<width$}  last seen",
        "name",
        "state",
        "version",
        "update",
        width = UPDATE_STATUS_WIDTH,
    );
    for row in node_rows(now, &health) {
        println!("{row}");
    }
    Ok(())
}

/// The update-status column width in the node table.
const UPDATE_STATUS_WIDTH: usize = 26;

/// Cuts the update status to the column width with an ellipsis, so a long
/// reason (internal errors carry URLs) cannot push the last column out of
/// line.
fn truncate_status(status: &UpdateStatus) -> String {
    let text = status.to_string();
    if text.chars().count() <= UPDATE_STATUS_WIDTH {
        return text;
    }
    let kept: String = text.chars().take(UPDATE_STATUS_WIDTH - 3).collect();
    format!("{kept}...")
}

/// One line of the node table: name, liveness, version, update status, and
/// last seen.
fn node_rows(now: SystemTime, health: &[NodeHealth]) -> Vec<String> {
    health
        .iter()
        .map(|node| {
            format!(
                "{:<16}  {:<6}  {:<10}  {:<width$}  {}",
                node.name,
                if node.up { "up" } else { "down" },
                node.version,
                truncate_status(&node.update_status),
                format_ago(now, node.last_seen_secs),
                width = UPDATE_STATUS_WIDTH,
            )
        })
        .collect()
}

async fn run_clone(args: CloneArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;

    let client = cp_client()?;
    let request = CloneRequest {
        node: args.node.clone(),
        repo_url: args.repo_url,
        git_ref: args.git_ref,
        persona: args.persona,
        prompt: args.message,
        mcp_servers: args.mcp,
    };
    let response = client
        .post(format!("{cp_url}/clone"))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .with_context(|| format!("failed to read response from {cp_url}"))?;
        return Err(anyhow::anyhow!("clone failed: {text}"));
    }
    maybe_print_update_notice(response.headers());
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    let session: Session = serde_json::from_str(&text).context("failed to parse clone response")?;
    println!(
        "cloned session {} on node {} (status {})",
        session.id,
        session.node,
        session.state.as_str()
    );
    println!("open with: bosun open {}", session.id);
    Ok(())
}

#[derive(Clone)]
enum Choice {
    SpawnHere,
    Up,
    Dir(DirEntry),
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Choice::SpawnHere => write!(f, "spawn here"),
            Choice::Up => write!(f, ".."),
            Choice::Dir(entry) => {
                let repo = if entry.is_repo { " [repo]" } else { "" };
                write!(f, "{}{}", entry.name, repo)
            }
        }
    }
}

async fn run_dev(args: DevArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;
    let client = cp_client()?;
    let mut current: Option<PathBuf> = None;

    loop {
        let listing = fetch_dirs(&client, &cp_url, &args.node, current.as_deref()).await?;

        let mut choices: Vec<Choice> = Vec::new();
        if current.is_some() {
            choices.push(Choice::SpawnHere);
            choices.push(Choice::Up);
        }
        choices.extend(listing.entries.iter().cloned().map(Choice::Dir));

        let title = match &current {
            Some(path) => format!("browse {}", path.display()),
            None => format!("browse node {} roots", args.node),
        };
        let selected = match FuzzySelect::new()
            .with_prompt(&title)
            .items(&choices)
            .default(0)
            .interact_opt()
        {
            Ok(Some(index)) => index,
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let choice = &choices[selected];

        match choice {
            Choice::SpawnHere => {
                let dir = current.expect("spawn here is only offered below the roots screen");
                spawn_dev(
                    &client,
                    &cp_url,
                    &args.node,
                    &dir,
                    args.persona.clone(),
                    args.message.clone(),
                    args.mcp.clone(),
                )
                .await?;
                return Ok(());
            }
            Choice::Up => current = listing.parent.clone(),
            Choice::Dir(entry) => current = Some(entry.path.clone()),
        }
    }
}

async fn fetch_dirs(
    client: &reqwest::Client,
    cp_url: &str,
    node: &str,
    path: Option<&Path>,
) -> anyhow::Result<DirListing> {
    let mut request = client.get(format!("{cp_url}/nodes/{node}/dirs"));
    if let Some(path) = path {
        request = request.query(&[("path", path.to_string_lossy().to_string())]);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .with_context(|| format!("failed to read response from {cp_url}"))?;
        return Err(anyhow::anyhow!(
            "failed to list directories on node {node}: {text}"
        ));
    }
    maybe_print_update_notice(response.headers());
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    let listing: DirListing =
        serde_json::from_str(&text).context("failed to parse directory listing")?;
    Ok(listing)
}

async fn spawn_dev(
    client: &reqwest::Client,
    cp_url: &str,
    node: &str,
    dir: &Path,
    persona: Option<String>,
    prompt: Option<String>,
    mcp_servers: Vec<String>,
) -> anyhow::Result<()> {
    let request = DevRequest {
        node: node.to_string(),
        dir: dir.to_path_buf(),
        persona,
        prompt,
        mcp_servers,
    };
    let response = client
        .post(format!("{cp_url}/dev"))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .with_context(|| format!("failed to read response from {cp_url}"))?;
        return Err(anyhow::anyhow!("dev failed: {text}"));
    }
    maybe_print_update_notice(response.headers());
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    let session: Session = serde_json::from_str(&text).context("failed to parse dev response")?;
    println!(
        "started dev session {} on node {} (status {})",
        session.id,
        session.node,
        session.state.as_str()
    );
    println!("open with: bosun open {}", session.id);
    Ok(())
}

async fn fetch_sessions(cp_url: &str) -> anyhow::Result<Vec<SessionView>> {
    let client = cp_client()?;
    let response = client
        .get(format!("{cp_url}/sessions"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("control plane at {cp_url} returned an error"))?;
    maybe_print_update_notice(response.headers());
    response
        .json()
        .await
        .context("failed to parse session list")
}

async fn run_list(args: ListArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;
    let sessions = fetch_sessions(&cp_url).await?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    let terminal = std::io::stdout().is_terminal();
    let colour = terminal && env::var_os("NO_COLOR").is_none();
    let width = if terminal {
        crossterm::terminal::size().map_or(LIST_WIDTH, |(columns, _)| columns as usize)
    } else {
        LIST_WIDTH
    };
    for line in list_lines(&sessions, width) {
        println!("{}", ansi(&line, colour));
    }
    Ok(())
}

/// The width `bosun list` lays its rows out to when stdout is not a terminal.
const LIST_WIDTH: usize = 100;
/// Cells in a tree's task bar.
const LIST_TASK_BAR_CELLS: usize = 10;

/// The groups `bosun list` sorts session trees into, in the order it prints
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ListGroup {
    NeedsYou,
    Working,
    Idle,
}

impl ListGroup {
    fn heading(self) -> Span<'static> {
        let (text, colour) = match self {
            ListGroup::NeedsYou => ("NEEDS YOU", crew::NEEDS_YOU),
            ListGroup::Working => ("WORKING", crew::WORKING),
            ListGroup::Idle => ("IDLE", crew::MUTED),
        };
        Span::styled(
            text,
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        )
    }
}

/// One session tree as `bosun list` shows it: its sessions with the root
/// first, the crew they make, and the group the tree sorts into.
struct ListedTree<'a> {
    sessions: Vec<&'a SessionView>,
    crew: Vec<crew::Member>,
    group: ListGroup,
}

/// The session list as trees: every child folds into its root. A tree needs
/// you when any member asks, works when any member runs, and is idle
/// otherwise. Within a group the newest tree comes first.
fn list_trees(sessions: &[SessionView]) -> Vec<ListedTree<'_>> {
    let ids: HashSet<&str> = sessions
        .iter()
        .map(|view| view.session.id.as_str())
        .collect();
    let mut trees: Vec<ListedTree> = sessions
        .iter()
        .filter(|view| {
            view.session.owner_id == view.session.id
                || !ids.contains(view.session.owner_id.as_str())
        })
        .map(|root| {
            let crew = crew::crew_members(&root.session.id, sessions, None);
            let members: Vec<&SessionView> = crew
                .iter()
                .filter_map(|member| sessions.iter().find(|view| view.session.id == member.id))
                .collect();
            let group = if members.iter().any(|view| view.overview.asking) {
                ListGroup::NeedsYou
            } else if members.iter().any(|view| {
                matches!(
                    view.session.state,
                    SessionState::Running | SessionState::Creating
                )
            }) {
                ListGroup::Working
            } else {
                ListGroup::Idle
            };
            ListedTree {
                sessions: members,
                crew,
                group,
            }
        })
        .collect();
    trees.sort_by(|a, b| {
        let (a_root, b_root) = (&a.sessions[0].session, &b.sessions[0].session);
        a.group
            .cmp(&b.group)
            .then(b_root.created_at_secs.cmp(&a_root.created_at_secs))
            .then(a_root.id.cmp(&b_root.id))
    });
    trees
}

/// What is happening in a tree now: who asks, or who is doing what, or the
/// root's state word when the tree is idle.
fn now_spans(tree: &ListedTree) -> Vec<Span<'static>> {
    let members = || tree.sessions.iter().zip(tree.crew.iter());
    let name_of = |id: &str| {
        tree.crew
            .iter()
            .find(|member| member.id == id)
            .map_or_else(|| crew::short_id(id), |member| member.name.clone())
    };
    match tree.group {
        ListGroup::NeedsYou => {
            let Some((_, asker)) = members().find(|(view, _)| view.overview.asking) else {
                return Vec::new();
            };
            // The list carries no question text, only that a question waits.
            let text = match asker.parent_id.as_deref() {
                None => format!("! {} asks you", asker.name),
                Some(parent) => format!("! {} asks {}", asker.name, name_of(parent)),
            };
            vec![Span::styled(text, Style::default().fg(crew::NEEDS_YOU))]
        }
        ListGroup::Working => {
            let running = || {
                members().filter(|(view, _)| {
                    matches!(
                        view.session.state,
                        SessionState::Running | SessionState::Creating
                    )
                })
            };
            let newest = running()
                .filter_map(|(view, member)| view.overview.activity.as_ref().map(|a| (a, member)))
                .max_by_key(|(activity, _)| activity.at_ms);
            let (name, caption) = match newest {
                Some((activity, member)) => (
                    member.name.clone(),
                    crew::activity_caption(&activity.phase, &tree.crew)
                        .unwrap_or_else(|| "working".into()),
                ),
                None => match running().next() {
                    Some((_, member)) => (member.name.clone(), "working".into()),
                    None => return Vec::new(),
                },
            };
            vec![
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!(" is {caption}")),
            ]
        }
        ListGroup::Idle => {
            let word = crew::state_word(&tree.crew[0]);
            vec![Span::styled(
                word.text().into_owned(),
                Style::default().fg(crew::MUTED),
            )]
        }
    }
}

/// The last part of a repository URL or a directory, without `.git`.
fn repo_name(session: &Session) -> String {
    let source = session.repo_url.as_deref().unwrap_or(&session.dir);
    let name = source
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(source);
    name.trim_end_matches(".git").to_string()
}

/// `left` and `right` on one row `width` wide, `right` against the right
/// edge. `left` is cut to leave room for `right`; `right` is dropped when it
/// alone does not fit.
fn fit_row(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let right_width: usize = right.iter().map(|span| span.content.chars().count()).sum();
    if right_width + 2 > width {
        return crew::clip_spans(left, width);
    }
    let mut line = crew::clip_spans(left, width - right_width - 2);
    let left_width: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    line.spans
        .push(Span::raw(" ".repeat(width - left_width - right_width)));
    line.spans.extend(right);
    line
}

/// The `bosun list` rows: a heading per group, then two rows per tree. The
/// first names the crew as flag tags, the summary, and on the right the node,
/// the repository and the tree's cost. The second says what is happening now
/// with the root's task progress, and the root's id to open it with.
fn list_lines(sessions: &[SessionView], width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(crew::MUTED);
    let mut lines = Vec::new();
    let mut group = None;
    for tree in list_trees(sessions) {
        if group != Some(tree.group) {
            if group.is_some() {
                lines.push(Line::default());
            }
            lines.push(Line::from(tree.group.heading()));
            group = Some(tree.group);
        }
        let root = tree.sessions[0];
        let mut left = Vec::new();
        for (index, member) in tree.crew.iter().enumerate() {
            if index > 0 {
                left.push(Span::raw(" "));
            }
            left.push(crew::tag_span(&member.persona));
        }
        let summary = root
            .session
            .summary
            .clone()
            .unwrap_or_else(|| format!("{} · {}", root.session.node, root.session.dir));
        left.push(Span::raw(format!("  {summary}")));
        let cost: f64 = tree.sessions.iter().map(|view| view.overview.cost).sum();
        let right = vec![Span::styled(
            format!(
                "{} · {} · ${cost:.2}",
                root.session.node,
                repo_name(&root.session)
            ),
            dim,
        )];
        lines.push(fit_row(left, right, width));

        let mut now = vec![Span::raw("   ")];
        now.extend(now_spans(&tree));
        let tasks = root.overview.tasks;
        if tasks.total > 0 {
            now.push(Span::raw("  "));
            now.extend(crew::task_bar(
                tasks.done,
                tasks.in_progress,
                tasks.total,
                LIST_TASK_BAR_CELLS,
            ));
            now.push(Span::styled(
                format!(" {} of {} tasks", tasks.done, tasks.total),
                dim,
            ));
        }
        lines.push(fit_row(
            now,
            vec![Span::styled(root.session.id.clone(), dim)],
            width,
        ));
    }
    lines
}

/// A row as text: with `colour`, each styled span in ANSI SGR codes;
/// without, the plain letters, so a tag still reads as its two letters.
fn ansi(line: &Line, colour: bool) -> String {
    let mut out = String::new();
    for span in &line.spans {
        let mut codes = Vec::new();
        if colour {
            if span.style.add_modifier.contains(Modifier::BOLD) {
                codes.push("1".to_string());
            }
            if let Some(Color::Rgb(r, g, b)) = span.style.fg {
                codes.push(format!("38;2;{r};{g};{b}"));
            }
            if let Some(Color::Rgb(r, g, b)) = span.style.bg {
                codes.push(format!("48;2;{r};{g};{b}"));
            }
        }
        if codes.is_empty() {
            out.push_str(&span.content);
        } else {
            out.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.content));
        }
    }
    out
}

async fn run_open(args: OpenArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;
    let session_id = match args.session_id {
        Some(id) => {
            let sessions = fetch_sessions(&cp_url).await?;
            if !sessions.iter().any(|view| view.session.id == id) {
                return Err(anyhow::anyhow!("session {id} not found"));
            }
            id
        }
        None => {
            let Some(id) = pick_session(&cp_url).await? else {
                return Ok(());
            };
            id
        }
    };
    attach::attach(&cp_url, &session_id).await
}

async fn pick_session(cp_url: &str) -> anyhow::Result<Option<String>> {
    let sessions = fetch_sessions(cp_url).await?;
    if sessions.is_empty() {
        return Err(anyhow::anyhow!("no sessions to open"));
    }
    let items: Vec<String> = sessions
        .iter()
        .map(|view| {
            let s = &view.session;
            format!("{}  {}  {}", s.id, s.node, s.state.as_str())
        })
        .collect();
    let selected = match FuzzySelect::new()
        .with_prompt("session")
        .items(&items)
        .default(0)
        .interact_opt()
    {
        Ok(Some(index)) => index,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(sessions[selected].session.id.clone()))
}
fn run_config(args: ConfigArgs) -> anyhow::Result<()> {
    match args.command {
        ConfigCommand::Set(args) => {
            if args.key != "cp-url" {
                return Err(anyhow::anyhow!("unknown config key: {}", args.key));
            }
            let mut config = load_cli_config()?;
            config.cp_url = args.value;
            save_cli_config(&config)?;
            println!(
                "stored cp-url {} in {}",
                config.cp_url,
                cli_config_path().display()
            );
            Ok(())
        }
        ConfigCommand::Get => {
            let config = load_cli_config()?;
            println!(
                "cp-url {} (in {})",
                config.cp_url,
                cli_config_path().display()
            );
            Ok(())
        }
        ConfigCommand::Unset => {
            let mut config = load_cli_config()?;
            config.cp_url = CliConfig::default().cp_url;
            save_cli_config(&config)?;
            println!("reset cp-url to {}", config.cp_url);
            Ok(())
        }
    }
}

async fn run_stop(args: StopArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;

    let client = cp_client()?;
    let request = StopRequest {
        session_id: args.session_id.clone(),
    };
    let response = client
        .post(format!("{cp_url}/stop"))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .with_context(|| format!("failed to read response from {cp_url}"))?;
        return Err(anyhow::anyhow!("stop failed: {text}"));
    }
    maybe_print_update_notice(response.headers());
    println!("stopped session {}", args.session_id);
    Ok(())
}

async fn run_update_cmd(args: UpdateArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;
    let client = cp_client()?;
    if args.nodes.is_empty() {
        update::run_update(&client, &cp_url, args.force)
            .await
            .map_err(anyhow::Error::from)
    } else {
        update::update_nodes(&client, &cp_url, &args.nodes, args.force).await
    }
}

/// Set once a newer control plane has been announced, so the notice prints
/// at most once per invocation no matter how many commands reach it.
static UPDATE_NOTICE_PRINTED: AtomicBool = AtomicBool::new(false);

/// Prints the update notice when the response header names a newer control
/// plane and this invocation has not announced one yet.
pub(crate) fn maybe_print_update_notice(headers: &HeaderMap) {
    let _ = print_update_notice_once(&UPDATE_NOTICE_PRINTED, &mut std::io::stderr(), headers);
}

/// The single stderr line announcing a newer control plane, decided from the
/// response header. Equal, older, unparsable, and missing versions announce
/// nothing.
fn print_update_notice_once(
    flag: &AtomicBool,
    stderr: &mut dyn std::io::Write,
    headers: &HeaderMap,
) -> std::io::Result<()> {
    if flag.load(AtomicOrdering::Relaxed) {
        return Ok(());
    }
    let Some(line) = update_notice_line(headers) else {
        return Ok(());
    };
    flag.store(true, AtomicOrdering::Relaxed);
    writeln!(stderr, "{line}")
}

fn update_notice_line(headers: &HeaderMap) -> Option<String> {
    let cp_version = headers.get(X_BOSUN_VERSION)?.to_str().ok()?;
    (compare(cp_version, VERSION) == Some(std::cmp::Ordering::Greater))
        .then(|| format!("bosun {cp_version} available, run \"bosun update\""))
}

fn format_ago(now: SystemTime, unix_secs: u64) -> String {
    let now_secs = bosun_common::time::unix_secs(now) as u64;
    let diff = now_secs.saturating_sub(unix_secs);
    if diff < 60 {
        format!("{diff}s ago")
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else {
        format!("{}h ago", diff / 3600)
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use bosun_common::session::ActivityAt;
    use bosun_common::session::ActivityPhase;
    use bosun_common::session::Permission;
    use bosun_common::session::TaskCounts;
    use bosun_common::types::UpdateStatus;

    use super::*;

    /// The TLS path drains like the plain one: a connection that is open when
    /// the stop begins is waited for, not cut. This is the path a deployed
    /// control plane runs, and it had no drain before `serve_until_stopped`
    /// owned one. The test drives the helper with its own stop, which is why
    /// the helper takes that future rather than the process signal.
    #[tokio::test]
    async fn a_tls_stop_waits_for_an_open_connection() {
        use std::io::Write as _;

        use tokio::io::AsyncWriteExt as _;

        let dir = tempfile::tempdir().unwrap();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let mut cert_file = std::fs::File::create(&cert_path).unwrap();
        cert_file
            .write_all(certified.cert.pem().as_bytes())
            .unwrap();
        let mut key_file = std::fs::File::create(&key_path).unwrap();
        key_file
            .write_all(certified.key_pair.serialize_pem().as_bytes())
            .unwrap();

        let server_config = bosun_common::tls::load_server_config(&cert_path, &key_path).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        // A route that never answers, the way an event stream never completes.
        let app = axum::Router::new().route(
            "/hang",
            axum::routing::get(|| async { std::future::pending::<()>().await }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (stopping, stopped) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(serve_until_stopped(
            move |stopped| async move { serve_tls(listener, app, acceptor, "tls", stopped).await },
            async move {
                let _ = stopped.await;
            },
        ));

        // One connection, with a request that will not finish.
        let client_config = bosun_common::tls::load_client_config(Some(&cert_path))
            .unwrap()
            .expect("a CA file builds a client config");
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(name, stream).await.unwrap();
        tls.write_all(b"GET /hang HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        // The stop begins: the connection that is open is waited for, and the
        // drain ends it at the bound rather than cutting it at once.
        let started = std::time::Instant::now();
        let _ = stopping.send(());
        let result = tokio::time::timeout(SHUTDOWN_DRAIN + Duration::from_secs(5), serving)
            .await
            .expect("the drain must end at its bound")
            .unwrap();
        assert!(result.is_ok(), "the server stopped cleanly");
        assert!(
            started.elapsed() >= SHUTDOWN_DRAIN,
            "the stop waits for the open connection rather than cutting it: {:?}",
            started.elapsed()
        );
        drop(tls);
    }

    /// The drain that both serve paths share. A serving future that never
    /// finishes — an open pane's event stream — is abandoned once the wait runs
    /// out, and the process stops anyway. It costs the bound, because the bound
    /// is what the test is about.
    #[tokio::test]
    async fn a_serving_future_that_never_finishes_is_abandoned_at_the_drain() {
        let started = std::time::Instant::now();
        let result = serve_until_stopped(
            |stopped| async move {
                let _ = stopped.await;
                std::future::pending::<()>().await;
                anyhow::Ok(())
            },
            std::future::ready(()),
        )
        .await;
        assert!(result.is_ok(), "the process stops anyway");
        let waited = started.elapsed();
        assert!(
            waited >= SHUTDOWN_DRAIN,
            "the drain is given its full bound, not skipped: {waited:?}"
        );
        assert!(
            waited < SHUTDOWN_DRAIN + Duration::from_secs(2),
            "and the bound ends it, rather than the future: {waited:?}"
        );
    }

    /// A serving future that finishes inside the drain is the one whose result
    /// the caller sees: a stop does not swallow a server failure.
    #[tokio::test]
    async fn a_serving_future_that_finishes_inside_the_drain_returns_its_result() {
        let result = serve_until_stopped(
            |stopped| async move {
                let _ = stopped.await;
                anyhow::bail!("the server failed on the way out");
            },
            std::future::ready(()),
        )
        .await;
        assert!(
            result.is_err(),
            "the serving future's failure reaches the caller"
        );
    }

    #[test]
    fn format_ago_reports_seconds_then_minutes_then_hours() {
        let now = UNIX_EPOCH + Duration::from_secs(100_000);
        assert_eq!(format_ago(now, 99_990), "10s ago");
        assert_eq!(format_ago(now, 99_000), "16m ago");
        assert_eq!(format_ago(now, 96_000), "1h ago");
    }

    #[test]
    fn resolve_cp_url_prefers_flag_over_env_over_stored() {
        let flag = Some("http://flag:8090");
        let env = Some("http://env:8090".to_string());
        let stored = "http://stored:8090".to_string();
        assert_eq!(
            resolve_cp_url_from(flag, env.clone(), stored.clone()),
            "http://flag:8090"
        );
        assert_eq!(
            resolve_cp_url_from(None, env, stored.clone()),
            "http://env:8090"
        );
        assert_eq!(
            resolve_cp_url_from(None, None, stored),
            "http://stored:8090"
        );
    }

    #[test]
    fn an_empty_github_token_literal_is_treated_as_absent() {
        assert_eq!(resolve_github_token(&None).unwrap(), None);
        assert_eq!(resolve_github_token(&Some("".to_string())).unwrap(), None);
        assert_eq!(
            resolve_github_token(&Some("a-secret-token".to_string())).unwrap(),
            Some("a-secret-token".to_string())
        );
    }

    #[test]
    fn node_rows_include_version_and_update_status() {
        let now = UNIX_EPOCH + Duration::from_secs(100_000);
        let health = vec![
            NodeHealth {
                name: "node-1".into(),
                version: "0.5.5".into(),
                update_status: UpdateStatus::Failed("checksum mismatch".into()),
                up: true,
                last_seen_secs: 99_990,
            },
            NodeHealth {
                name: "node-2".into(),
                version: "0.5.6".into(),
                update_status: UpdateStatus::UpToDate,
                up: false,
                last_seen_secs: 99_990,
            },
        ];
        let rows = node_rows(now, &health);
        assert_eq!(rows.len(), 2);
        let row = &rows[0];
        assert!(
            row.contains("node-1")
                && row.contains("0.5.5")
                && row.contains("failed: checksum mismatch")
                && row.contains("up")
                && row.contains("10s ago"),
            "unexpected row: {row}"
        );
        let row = &rows[1];
        assert!(
            row.contains("node-2")
                && row.contains("0.5.6")
                && row.contains("up-to-date")
                && row.contains("down"),
            "unexpected row: {row}"
        );
    }

    fn root_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            node: "n1".into(),
            repo_url: None,
            git_ref: None,
            dir: "/work".into(),
            model: "m".into(),
            persona: Some("coder".into()),
            parent_id: None,
            owner_id: id.to_string(),
            permission: Permission::ReadWrite,
            allowed_tools: "*".into(),
            mcp_servers: "".into(),
            state: SessionState::WaitingForInput,
            interrupt_cause: None,
            created_at_secs: 0,
            prompt: None,
            summary: None,
        }
    }

    /// A child of `root_session(owner)`, born on its node and directory.
    fn child_session(id: &str, owner: &str) -> Session {
        let parent = root_session(owner);
        Session {
            id: id.to_string(),
            dir: parent.dir.clone(),
            persona: Some("reviewer".into()),
            parent_id: Some(owner.to_string()),
            owner_id: owner.to_string(),
            permission: Permission::ReadOnly,
            state: SessionState::Running,
            ..root_session(id)
        }
    }

    fn listed(session: Session) -> SessionView {
        SessionView {
            session,
            overview: Default::default(),
        }
    }

    fn texts(sessions: &[SessionView], width: usize) -> Vec<String> {
        list_lines(sessions, width)
            .iter()
            .map(|line| ansi(line, false))
            .collect()
    }

    #[test]
    fn list_trees_fold_children_and_grandchildren_into_their_root() {
        let mut grand = child_session("grand-1", "child-1");
        grand.owner_id = "root-a".into();
        let sessions: Vec<SessionView> = vec![
            root_session("root-b"),
            child_session("child-1", "root-a"),
            grand,
            root_session("root-a"),
        ]
        .into_iter()
        .map(listed)
        .collect();
        let trees = list_trees(&sessions);
        let shape: Vec<(&str, Vec<&str>)> = trees
            .iter()
            .map(|tree| {
                (
                    tree.sessions[0].session.id.as_str(),
                    tree.crew
                        .iter()
                        .map(|member| member.name.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("root-a", vec!["Coder", "Reviewer", "Reviewer 2"]),
                ("root-b", vec!["Coder"]),
            ],
            "the working tree comes first; children fold into their root"
        );
        let rows = texts(&sessions, 100);
        assert!(rows[1].starts_with("Co Rv Rv  n1 · /work"), "{}", rows[1]);
    }

    #[test]
    fn list_groups_trees_under_needs_you_working_and_idle() {
        let mut asking = listed(root_session("asking"));
        asking.overview.asking = true;
        let mut working = listed(root_session("working"));
        working.session.state = SessionState::Running;
        let idle = listed(root_session("idle"));
        let rows = texts(&[idle, working, asking], 100);
        let headings: Vec<&str> = rows
            .iter()
            .filter(|row| !row.is_empty() && !row.starts_with(' ') && row == &&row.to_uppercase())
            .map(String::as_str)
            .collect();
        assert_eq!(headings, vec!["NEEDS YOU", "WORKING", "IDLE"]);
        assert!(rows[2].starts_with("   ! Coder asks you"), "{}", rows[2]);
        assert!(
            rows[2].ends_with("asking"),
            "the id to open it with: {}",
            rows[2]
        );
        assert!(
            rows.iter().any(|row| row.starts_with("   idle")),
            "{rows:?}"
        );
    }

    #[test]
    fn the_now_line_names_who_asks_and_who_is_doing_what() {
        let root = listed(root_session("root"));
        let mut child = listed(child_session("child", "root"));
        child.overview.asking = true;
        let rows = texts(&[root.clone(), child.clone()], 100);
        assert!(
            rows[2].starts_with("   ! Reviewer asks Coder"),
            "{}",
            rows[2]
        );

        child.overview.asking = false;
        child.overview.activity = Some(ActivityAt {
            at_ms: 5,
            phase: ActivityPhase::ToolStarted {
                name: "edit".into(),
                target: Some("src/winsw.ts".into()),
            },
        });
        let mut root = root;
        root.overview.tasks = TaskCounts {
            total: 7,
            done: 3,
            in_progress: 1,
        };
        root.overview.cost = 0.30;
        child.overview.cost = 0.12;
        let rows = texts(&[root, child], 100);
        assert!(
            rows[2].starts_with("   Reviewer is editing winsw.ts  ━━━━━━━━━━ 3 of 7 tasks"),
            "{}",
            rows[2]
        );
        assert!(rows[1].ends_with("n1 · work · $0.42"), "{}", rows[1]);
    }

    #[test]
    fn list_rows_fill_the_width_and_keep_the_right_column_at_the_edge() {
        let mut long = listed(root_session("root"));
        long.session.summary = Some("x".repeat(200));
        long.session.repo_url = Some("https://github.com/owner/bosun.git".into());
        let rows = texts(&[long], 80);
        assert_eq!(rows[1].chars().count(), 80, "{}", rows[1]);
        assert!(rows[1].ends_with("n1 · bosun · $0.00"), "{}", rows[1]);
        assert!(rows[1].contains('…'), "a long summary is cut: {}", rows[1]);
    }

    #[test]
    fn list_rows_read_without_colour_and_carry_ansi_codes_with_it() {
        let sessions = vec![listed(root_session("root"))];
        let line = &list_lines(&sessions, 100)[1];
        let plain = ansi(line, false);
        assert!(!plain.contains('\x1b'), "{plain:?}");
        assert!(
            plain.starts_with("Co  "),
            "the tag keeps its letters: {plain:?}"
        );
        let coloured = ansi(line, true);
        assert!(coloured.contains("\x1b["), "{coloured:?}");
        assert!(coloured.contains("Co"), "{coloured:?}");
    }

    #[test]
    fn node_rows_truncate_a_long_update_reason_to_keep_the_last_column_aligned() {
        let now = UNIX_EPOCH + Duration::from_secs(100_000);
        let health = vec![
            NodeHealth {
                name: "node-1".into(),
                version: "0.5.5".into(),
                update_status: UpdateStatus::Failed(
                    "checksum mismatch after https://example.com/long/url/path".into(),
                ),
                up: true,
                last_seen_secs: 99_990,
            },
            NodeHealth {
                name: "node-2".into(),
                version: "0.5.5".into(),
                update_status: UpdateStatus::UpToDate,
                up: true,
                last_seen_secs: 99_990,
            },
        ];
        let rows = node_rows(now, &health);
        assert!(
            rows[0].contains("failed: checksum mismat..."),
            "the long reason must be truncated: {}",
            rows[0]
        );
        assert_eq!(
            rows[0].rfind("10s ago"),
            rows[1].rfind("10s ago"),
            "the last column must stay aligned"
        );
    }

    #[test]
    fn node_args_parse_the_rollback_flag_without_a_config() {
        let with_flag = Cli::try_parse_from(["bosun", "node", "--rollback"])
            .expect("--rollback should parse without --config");
        let Command::Node(args) = with_flag.command else {
            panic!("expected the node command");
        };
        assert!(args.rollback);
        assert!(
            args.config.is_none(),
            "a rollback without --config must swap without restarting"
        );

        let with_config =
            Cli::try_parse_from(["bosun", "node", "--rollback", "--config", "x.toml"])
                .expect("--rollback with --config should parse");
        let Command::Node(args) = with_config.command else {
            panic!("expected the node command");
        };
        assert!(args.rollback);
        assert_eq!(
            args.config.as_deref(),
            Some(Path::new("x.toml")),
            "a rollback with --config must restart into the restored binary"
        );

        let without = Cli::try_parse_from(["bosun", "node", "--config", "x.toml"])
            .expect("node args without --rollback should parse");
        let Command::Node(args) = without.command else {
            panic!("expected the node command");
        };
        assert!(!args.rollback);
        assert_eq!(args.config.as_deref(), Some(Path::new("x.toml")));
    }

    #[test]
    fn clone_and_dev_parse_persona_instead_of_model_and_permission() {
        let clone = Cli::try_parse_from([
            "bosun",
            "clone",
            "--node",
            "node-1",
            "--persona",
            "reviewer",
            "https://example.com/repo",
        ])
        .expect("--persona should parse");
        let Command::Clone(args) = clone.command else {
            panic!("expected the clone command");
        };
        assert_eq!(args.persona.as_deref(), Some("reviewer"));
        assert!(args.git_ref.is_none());
        assert!(args.mcp.is_empty());

        let dev = Cli::try_parse_from(["bosun", "dev", "--node", "node-1", "--persona", "coder"])
            .expect("--persona should parse");
        let Command::Dev(args) = dev.command else {
            panic!("expected the dev command");
        };
        assert_eq!(args.persona.as_deref(), Some("coder"));
        assert!(args.mcp.is_empty());
    }

    #[test]
    fn clone_and_dev_parse_a_repeatable_mcp_flag() {
        let clone = Cli::try_parse_from([
            "bosun",
            "clone",
            "--node",
            "node-1",
            "--mcp",
            "srv-a",
            "--mcp",
            "srv-b",
            "https://example.com/repo",
        ])
        .expect("--mcp should parse for clone");
        let Command::Clone(args) = clone.command else {
            panic!("expected the clone command");
        };
        assert_eq!(args.mcp, ["srv-a", "srv-b"]);

        let dev = Cli::try_parse_from([
            "bosun", "dev", "--node", "node-1", "--mcp", "srv-a", "--mcp", "srv-b",
        ])
        .expect("--mcp should parse for dev");
        let Command::Dev(args) = dev.command else {
            panic!("expected the dev command");
        };
        assert_eq!(args.mcp, ["srv-a", "srv-b"]);
    }

    #[test]
    fn clone_and_dev_reject_the_old_model_and_permission_flags() {
        assert!(
            Cli::try_parse_from([
                "bosun",
                "clone",
                "--node",
                "node-1",
                "--model",
                "main",
                "https://example.com/repo",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "bosun",
                "clone",
                "--node",
                "node-1",
                "--permission",
                "read-only",
                "https://example.com/repo",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "bosun",
                "dev",
                "--node",
                "node-1",
                "--permission",
                "read-write",
            ])
            .is_err()
        );
    }

    /// A version strictly newer than `version`: the patch bumped and the
    /// prerelease dropped, so a zero patch or a prerelease `version` cannot
    /// break the arithmetic.
    fn newer_than(version: &str) -> String {
        let mut parsed = semver::Version::parse(version).expect("version must parse as semver");
        if parsed.patch == u64::MAX {
            parsed.minor += 1;
            parsed.patch = 0;
        } else {
            parsed.patch += 1;
        }
        parsed.pre = semver::Prerelease::EMPTY;
        parsed.to_string()
    }

    /// A version strictly older than `version`: the patch dropped, or the
    /// previous minor or major release when the patch is 0, with the
    /// prerelease dropped. `0.0.0` falls back to a prerelease, which sorts
    /// below every release.
    fn older_than(version: &str) -> String {
        let mut parsed = semver::Version::parse(version).expect("version must parse as semver");
        if parsed.patch > 0 {
            parsed.patch -= 1;
        } else if parsed.minor > 0 {
            parsed.minor -= 1;
        } else if parsed.major > 0 {
            parsed.major -= 1;
        } else {
            return "0.0.0-0".to_string();
        }
        parsed.pre = semver::Prerelease::EMPTY;
        parsed.to_string()
    }

    fn newer_version() -> String {
        newer_than(VERSION)
    }

    fn older_version() -> String {
        older_than(VERSION)
    }

    #[test]
    fn version_helpers_are_strictly_newer_and_older() {
        assert_eq!(
            compare(&newer_version(), VERSION),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare(&older_version(), VERSION),
            Some(std::cmp::Ordering::Less)
        );
    }

    #[test]
    fn version_helpers_survive_a_zero_patch_and_prerelease_versions() {
        for version in ["0.5.0", "0.5.5-alpha.1", "0.5.5-alpha"] {
            assert_eq!(
                compare(&newer_than(version), version),
                Some(std::cmp::Ordering::Greater),
                "newer_than({version:?}) must stay strictly newer"
            );
            assert_eq!(
                compare(&older_than(version), version),
                Some(std::cmp::Ordering::Less),
                "older_than({version:?}) must stay strictly older"
            );
        }
    }

    fn headers_with_version(version: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            X_BOSUN_VERSION,
            reqwest::header::HeaderValue::from_str(version).unwrap(),
        );
        headers
    }

    fn captured_output(flag: &AtomicBool, headers: &HeaderMap) -> String {
        let mut output = Vec::new();
        print_update_notice_once(flag, &mut output, headers).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn update_notice_line_announces_a_newer_control_plane() {
        let newer = newer_version();
        let line = update_notice_line(&headers_with_version(&newer))
            .expect("a newer control plane must announce");
        assert_eq!(
            line,
            format!("bosun {newer} available, run \"bosun update\"")
        );
    }

    #[test]
    fn update_notice_line_is_silent_for_equal_older_unparsable_and_missing() {
        assert_eq!(update_notice_line(&headers_with_version(VERSION)), None);
        assert_eq!(
            update_notice_line(&headers_with_version(&older_version())),
            None
        );
        assert_eq!(update_notice_line(&headers_with_version("banana")), None);
        assert_eq!(update_notice_line(&HeaderMap::new()), None);
    }

    #[test]
    fn update_notice_prints_once_per_invocation() {
        let flag = AtomicBool::new(false);
        let headers = headers_with_version(&newer_version());
        let newer = newer_version();
        let first = captured_output(&flag, &headers);
        assert_eq!(
            first,
            format!("bosun {newer} available, run \"bosun update\"\n")
        );
        let second = captured_output(&flag, &headers);
        assert_eq!(second, "", "the notice must print at most once");
    }

    #[test]
    fn update_notice_does_not_consume_the_flag_on_silent_headers() {
        let flag = AtomicBool::new(false);
        assert_eq!(captured_output(&flag, &headers_with_version(VERSION)), "");
        let announced = captured_output(&flag, &headers_with_version(&newer_version()));
        assert!(
            announced.contains("available"),
            "a silent header must not consume the once-per-invocation notice: {announced:?}"
        );
    }

    #[test]
    fn update_args_parse_force_and_cp_url() {
        let plain = Cli::try_parse_from(["bosun", "update"]).expect("update should parse");
        let Command::Update(args) = plain.command else {
            panic!("expected the update command");
        };
        assert!(!args.force);
        assert!(args.cp_url.is_none());
        assert!(args.nodes.is_empty(), "no node args means self-update");

        let forced =
            Cli::try_parse_from(["bosun", "update", "--force"]).expect("--force should parse");
        let Command::Update(args) = forced.command else {
            panic!("expected the update command");
        };
        assert!(args.force);
        assert!(args.nodes.is_empty());

        let with_url =
            Cli::try_parse_from(["bosun", "update", "--cp-url", "http://cp:8090", "--force"])
                .expect("--cp-url and --force should parse together");
        let Command::Update(args) = with_url.command else {
            panic!("expected the update command");
        };
        assert!(args.force);
        assert_eq!(args.cp_url.as_deref(), Some("http://cp:8090"));
    }

    #[test]
    fn update_args_parse_named_nodes_as_an_update_command() {
        let with_node =
            Cli::try_parse_from(["bosun", "update", "node-a"]).expect("a node should parse");
        let Command::Update(args) = with_node.command else {
            panic!("expected the update command");
        };
        assert_eq!(args.nodes, ["node-a"]);
        assert!(!args.force);

        let forced = Cli::try_parse_from(["bosun", "update", "node-a", "node-b", "--force"])
            .expect("nodes and --force should parse");
        let Command::Update(args) = forced.command else {
            panic!("expected the update command");
        };
        assert_eq!(args.nodes, ["node-a", "node-b"]);
        assert!(args.force);

        let flag_first = Cli::try_parse_from(["bosun", "update", "--force", "node-a"])
            .expect("--force before a node should parse");
        let Command::Update(args) = flag_first.command else {
            panic!("expected the update command");
        };
        assert_eq!(args.nodes, ["node-a"]);
        assert!(args.force);
    }
}
