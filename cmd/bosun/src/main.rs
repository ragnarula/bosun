use std::collections::HashMap;
use std::env;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use bosun_agent::adapters::provider_for;
use bosun_agent::config::resolve_model;
use bosun_agent::provider::Provider;
use bosun_common::config::CliConfig;
use bosun_common::config::ControlConfig;
use bosun_common::config::NodeConfig;
use bosun_common::config::cli_config_path;
use bosun_common::config::load_cli_config;
use bosun_common::config::load_config;
use bosun_common::config::save_cli_config;
#[cfg(windows)]
use bosun_common::error::ErrorExt;
use bosun_common::session::Permission;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
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
use bosun_control::registry::NodeHealth;
use bosun_control::registry::NodeRegistry;
use bosun_control::tunnel::TunnelRegistry;
use bosun_executor::tools;
use bosun_node::manager::NodeManager;
use bosun_store::store::Store;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use dialoguer::FuzzySelect;
use reqwest::header::HeaderMap;
#[cfg(windows)]
use tracing::error;
use tracing::info;
use tracing::warn;

mod attach;
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
    /// Run an executor server for one session.
    Executor(ExecutorArgs),
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
    /// Model to run the session with. Defaults to the control plane's default.
    #[arg(long)]
    model: Option<String>,
    /// Tool permission: read-only or read-write. Defaults to read-write.
    #[arg(long)]
    permission: Option<String>,
    /// First instruction for the session.
    #[arg(long)]
    message: Option<String>,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct DevArgs {
    /// Node whose directories to browse.
    #[arg(long)]
    node: String,
    /// Model to run the session with. Defaults to the control plane's default.
    #[arg(long)]
    model: Option<String>,
    /// Tool permission: read-only or read-write. Defaults to read-write.
    #[arg(long)]
    permission: Option<String>,
    /// First instruction for the session.
    #[arg(long)]
    message: Option<String>,
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

#[derive(Args)]
struct ExecutorArgs {
    /// Directory the session works in.
    #[arg(long)]
    session_dir: PathBuf,
    /// Port the executor listens on.
    #[arg(long)]
    port: u16,
    /// Tool permission: read_only or read_write.
    #[arg(long, default_value = "read_write")]
    permission: String,
    /// Override the log filter. Defaults to RUST_LOG, then info.
    #[arg(long)]
    log_filter: Option<String>,
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
        Command::Executor(args) => run_executor(args).await,
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
    let config: ControlConfig =
        load_config(&args.config).context("failed to load control-plane config")?;
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
    let skills_dir = config.data_dir.join("skills");
    tokio::fs::create_dir_all(&skills_dir)
        .await
        .with_context(|| format!("failed to create skills directory {}", skills_dir.display()))?;
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

    let state = Arc::new(AppState {
        registry: Arc::new(NodeRegistry::new(Duration::from_secs(
            config.node_timeout_secs,
        ))),
        commands: Arc::new(CommandQueue::new(Duration::from_secs(
            config.node_timeout_secs,
        ))),
        tunnels: Arc::new(TunnelRegistry::new()),
        store,
        loops: Arc::new(AgentRegistry::new(
            Some(skills_dir.clone()),
            providers.clone(),
            config.subagents.clone(),
            prices,
        )),
        providers,
        subagents: config.subagents,
        default_model: config.default_model,
        skills_dir: Some(skills_dir),
    });
    bosun_control::api::recover(&state).await;
    let app = bosun_control::api::router(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;

    match (config.tls_cert.as_deref(), config.tls_key.as_deref()) {
        (Some(cert), Some(key)) => {
            let server_config = bosun_common::tls::load_server_config(cert, key)
                .context("failed to load the control-plane TLS certificate")?;
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
            serve_tls(listener, app, acceptor, &config.listen_addr).await
        }
        (None, None) => {
            info!(listen_addr = %config.listen_addr, "control plane listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .context("control plane server failed")?;
            Ok(())
        }
        _ => Err(anyhow::anyhow!("tls_cert and tls_key must be set together")),
    }
}

async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    acceptor: tokio_rustls::TlsAcceptor,
    listen_addr: &str,
) -> anyhow::Result<()> {
    info!(listen_addr = %listen_addr, "control plane listening (TLS)");
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted.context("control plane accept failed")?,
            _ = shutdown_signal() => break,
        };
        let acceptor = acceptor.clone();
        let service = hyper_util::service::TowerToHyperService::new(app.clone().into_service());
        tokio::spawn(async move {
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
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
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
    manager.restore().await;

    tokio::select! {
        _ = bosun_node::poll::run_poll_loop(
            config.cp_url.clone(),
            config.node_name.clone(),
            manager,
            tls_config,
            config.update.enabled,
            config.update.base_url.clone(),
            bosun_node::poll::UPDATE_RETRY_DELAY,
        ) => Ok(()),
        _ = shutdown_signal() => Ok(()),
    }
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
    let permission = args
        .permission
        .as_deref()
        .map(parse_permission)
        .transpose()?;

    let client = cp_client()?;
    let request = CloneRequest {
        node: args.node.clone(),
        repo_url: args.repo_url,
        git_ref: args.git_ref,
        model: args.model,
        permission,
        prompt: args.message,
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
        state_name(session.state)
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
    let permission = args
        .permission
        .as_deref()
        .map(parse_permission)
        .transpose()?;
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
                    args.model.clone(),
                    permission,
                    args.message.clone(),
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
    model: Option<String>,
    permission: Option<Permission>,
    prompt: Option<String>,
) -> anyhow::Result<()> {
    let request = DevRequest {
        node: node.to_string(),
        dir: dir.to_path_buf(),
        model,
        permission,
        prompt,
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
        state_name(session.state)
    );
    println!("open with: bosun open {}", session.id);
    Ok(())
}

async fn fetch_sessions(cp_url: &str) -> anyhow::Result<Vec<Session>> {
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
    println!(
        "{:<36}  {:<10}  {:<28}  {:<12}  {:<6}",
        "id", "node", "source", "ref", "status"
    );
    for session in &sessions {
        let source = session
            .repo_url
            .clone()
            .unwrap_or_else(|| session.dir.clone());
        let git_ref = session.git_ref.as_deref().unwrap_or("-");
        println!(
            "{:<36}  {:<10}  {:<28}  {:<12}  {:<6}",
            session.id,
            session.node,
            source,
            git_ref,
            state_name(session.state)
        );
    }
    Ok(())
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Creating => "creating",
        SessionState::Running => "running",
        SessionState::WaitingForInput => "waiting_for_input",
        SessionState::Interrupted => "interrupted",
        SessionState::Stopped => "stopped",
    }
}

async fn run_open(args: OpenArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;
    let session_id = match args.session_id {
        Some(id) => {
            let sessions = fetch_sessions(&cp_url).await?;
            if !sessions.iter().any(|s| s.id == id) {
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
        .map(|s| format!("{}  {}  {}", s.id, s.node, state_name(s.state)))
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
    Ok(Some(sessions[selected].id.clone()))
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

fn parse_permission(value: &str) -> anyhow::Result<Permission> {
    tools::permission_from_str(value).ok_or_else(|| {
        anyhow::anyhow!("invalid permission: {value}; expected read-only or read-write")
    })
}

async fn run_executor(args: ExecutorArgs) -> anyhow::Result<()> {
    let permission = parse_permission(&args.permission)?;
    setup_logging(args.log_filter.as_deref())?;
    bosun_executor::server::serve(args.session_dir, args.port, permission).await?;
    Ok(())
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

    use bosun_common::types::UpdateStatus;

    use super::*;

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
    fn parse_permission_accepts_dashes_and_underscores() {
        assert_eq!(parse_permission("read-only").unwrap(), Permission::ReadOnly);
        assert_eq!(parse_permission("read_only").unwrap(), Permission::ReadOnly);
        assert_eq!(
            parse_permission("read-write").unwrap(),
            Permission::ReadWrite
        );
        assert_eq!(
            parse_permission("read_write").unwrap(),
            Permission::ReadWrite
        );
        assert!(parse_permission("admin").is_err());
        assert!(parse_permission("").is_err());
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
