use std::env;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use bosun_common::config::CliConfig;
use bosun_common::config::ControlConfig;
use bosun_common::config::NodeConfig;
use bosun_common::config::cli_config_path;
use bosun_common::config::load_cli_config;
use bosun_common::config::load_config;
use bosun_common::config::save_cli_config;
use bosun_common::telemetry::setup_logging;
use bosun_common::types::CloneRequest;
use bosun_common::types::DevRequest;
use bosun_common::types::DirEntry;
use bosun_common::types::DirListing;
use bosun_common::types::StopRequest;
use bosun_control::api::AppState;
use bosun_control::commands::CommandQueue;
use bosun_control::registry::NodeHealth;
use bosun_control::registry::NodeRegistry;
use bosun_control::registry::SessionHealth;
use bosun_control::tunnel::TunnelRegistry;
use bosun_node::manager::NodeManager;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use dialoguer::FuzzySelect;
use tracing::info;

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
    /// Open a session's connect command.
    Open(OpenArgs),
    /// Manage the CLI config file.
    Config(ConfigArgs),
    /// Stop a session and remove it from the node.
    Stop(StopArgs),
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
    /// Path to the node config file.
    #[arg(long)]
    config: PathBuf,
    /// Override the log filter. Defaults to RUST_LOG, then info.
    #[arg(long)]
    log_filter: Option<String>,
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
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then the stored config, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct DevArgs {
    /// Node whose directories to browse.
    #[arg(long)]
    node: String,
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
    /// Session id to connect to.
    session_id: String,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let mut builder = reqwest::Client::builder();
    if let Ok(ca_cert) = std::env::var("BOSUN_CA_CERT")
        && !ca_cert.is_empty()
    {
        let config = bosun_common::tls::load_client_config(Some(Path::new(&ca_cert)))?
            .expect("a CA cert was configured");
        builder = builder.use_preconfigured_tls(config);
    }
    builder.build().context("failed to build the HTTP client")
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

    let state = Arc::new(AppState {
        registry: Arc::new(NodeRegistry::new(Duration::from_secs(
            config.node_timeout_secs,
        ))),
        commands: Arc::new(CommandQueue::new(Duration::from_secs(
            config.node_timeout_secs,
        ))),
        tunnels: Arc::new(TunnelRegistry::new()),
    });
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
    let config: NodeConfig = load_config(&args.config).context("failed to load node config")?;
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
        _ = bosun_node::poll::run_poll_loop(config.cp_url.clone(), config.node_name.clone(), manager, tls_config) => Ok(()),
        _ = shutdown_signal() => Ok(()),
    }
}

async fn run_nodes(args: NodesArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;

    let client = cp_client()?;
    let health: Vec<NodeHealth> = client
        .get(format!("{cp_url}/nodes"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?
        .error_for_status()
        .with_context(|| format!("control plane at {cp_url} returned an error"))?
        .json()
        .await
        .context("failed to parse node list")?;

    let now = SystemTime::now();
    if health.is_empty() {
        println!("no nodes registered");
        return Ok(());
    }
    for node in &health {
        let status = if node.up { "up" } else { "down" };
        println!(
            "{:<16} {:<6} {}",
            node.name,
            status,
            format_ago(now, node.last_seen_secs)
        );
    }
    Ok(())
}

async fn run_clone(args: CloneArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;

    let client = cp_client()?;
    let request = CloneRequest {
        node: args.node.clone(),
        repo_url: args.repo_url,
        git_ref: args.git_ref,
    };
    let response = client
        .post(format!("{cp_url}/clone"))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    if !status.is_success() {
        return Err(anyhow::anyhow!("clone failed: {text}"));
    }
    let health: SessionHealth =
        serde_json::from_str(&text).context("failed to parse clone response")?;
    println!(
        "cloned session {} on node {} (status {})",
        health.id, health.node, health.status
    );
    println!("{}", connect_command(&cp_url, &health.id));
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
                spawn_dev(&client, &cp_url, &args.node, &dir).await?;
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
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "failed to list directories on node {node}: {text}"
        ));
    }
    let listing: DirListing =
        serde_json::from_str(&text).context("failed to parse directory listing")?;
    Ok(listing)
}

async fn spawn_dev(
    client: &reqwest::Client,
    cp_url: &str,
    node: &str,
    dir: &Path,
) -> anyhow::Result<()> {
    let request = DevRequest {
        node: node.to_string(),
        dir: dir.to_path_buf(),
    };
    let response = client
        .post(format!("{cp_url}/dev"))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    if !status.is_success() {
        return Err(anyhow::anyhow!("dev failed: {text}"));
    }
    let health: SessionHealth =
        serde_json::from_str(&text).context("failed to parse dev response")?;
    println!(
        "started dev session {} on node {} (status {})",
        health.id, health.node, health.status
    );
    println!("{}", connect_command(cp_url, &health.id));
    Ok(())
}

async fn fetch_sessions(cp_url: &str) -> anyhow::Result<Vec<SessionHealth>> {
    let client = cp_client()?;
    client
        .get(format!("{cp_url}/sessions"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?
        .error_for_status()
        .with_context(|| format!("control plane at {cp_url} returned an error"))?
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
            .or_else(|| session.dir.as_ref().map(|d| d.display().to_string()))
            .unwrap_or_else(|| "-".to_string());
        let git_ref = session.git_ref.as_deref().unwrap_or("-");
        println!(
            "{:<36}  {:<10}  {:<28}  {:<12}  {:<6}",
            session.id, session.node, source, git_ref, session.status
        );
    }
    Ok(())
}

async fn run_open(args: OpenArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref())?;
    let sessions = fetch_sessions(&cp_url).await?;

    let Some(session) = sessions.into_iter().find(|s| s.id == args.session_id) else {
        return Err(anyhow::anyhow!("session {} not found", args.session_id));
    };
    println!("{}", connect_command(&cp_url, &session.id));
    Ok(())
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
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read response from {cp_url}"))?;
    if !status.is_success() {
        return Err(anyhow::anyhow!("stop failed: {text}"));
    }
    println!("stopped session {}", args.session_id);
    Ok(())
}

/// Prints the command to attach to a session. A session is addressed at its
/// subdomain host, so the web UI sits at the origin root and opencode's own
/// `/session` API paths reach it unchanged. A control plane reachable only at
/// a non-loopback IP has no wildcard DNS, so its sessions have no address.
fn connect_command(cp_url: &str, session_id: &str) -> String {
    match bosun_common::origin::session_origin(cp_url, session_id) {
        Some(origin) => format!("opencode attach {origin}"),
        None => format!(
            "session {session_id} is not reachable from {cp_url}: an IP-addressed control plane has no session subdomains"
        ),
    }
}

fn format_ago(now: SystemTime, unix_secs: u64) -> String {
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
    use super::*;

    #[test]
    fn format_ago_reports_seconds_then_minutes_then_hours() {
        let now = UNIX_EPOCH + Duration::from_secs(100_000);
        assert_eq!(format_ago(now, 99_990), "10s ago");
        assert_eq!(format_ago(now, 99_000), "16m ago");
        assert_eq!(format_ago(now, 96_000), "1h ago");
    }

    #[test]
    fn connect_command_explains_when_an_ip_control_plane_has_no_subdomains() {
        assert_eq!(
            connect_command("http://192.168.1.10:8090", "abc-123"),
            "session abc-123 is not reachable from http://192.168.1.10:8090: \
             an IP-addressed control plane has no session subdomains"
        );
    }

    #[test]
    fn connect_command_trims_a_trailing_slash() {
        assert_eq!(
            connect_command("http://127.0.0.1:8090/", "abc-123"),
            "opencode attach http://abc-123.localhost:8090"
        );
    }

    #[test]
    fn connect_command_uses_the_session_subdomain_for_dns_hosts() {
        assert_eq!(
            connect_command("https://bosun.on.21cs.biz", "abc-123"),
            "opencode attach https://abc-123.bosun.on.21cs.biz"
        );
    }

    #[test]
    fn connect_command_keeps_the_port_on_the_subdomain() {
        assert_eq!(
            connect_command("http://bosun.example.com:8090", "abc-123"),
            "opencode attach http://abc-123.bosun.example.com:8090"
        );
    }

    #[test]
    fn connect_command_uses_the_localhost_subdomain_for_loopback() {
        assert_eq!(
            connect_command("http://localhost:8090", "abc-123"),
            "opencode attach http://abc-123.localhost:8090"
        );
        assert_eq!(
            connect_command("http://127.0.0.1:8090", "abc-123"),
            "opencode attach http://abc-123.localhost:8090"
        );
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
}
