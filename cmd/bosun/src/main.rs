use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use bosun_common::config::CliConfig;
use bosun_common::config::ControlConfig;
use bosun_common::config::NodeConfig;
use bosun_common::config::load_config;
use bosun_common::error::ErrorExt;
use bosun_common::telemetry::setup_logging;
use bosun_common::types::Heartbeat;
use bosun_common::types::NodeStatus;
use bosun_common::types::SpawnRequest;
use bosun_control::api::AppState;
use bosun_control::proxy::ProxyManager;
use bosun_control::registry::NodeHealth;
use bosun_control::registry::NodeRegistry;
use bosun_control::registry::SessionHealth;
use bosun_node::manager::NodeManager;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::instrument;
use tracing::warn;

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
    Spawn(SpawnArgs),
    /// List sessions on the control plane.
    List(ListArgs),
    /// Print the connect command for a session.
    Open(OpenArgs),
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
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct SpawnArgs {
    /// Node to clone the repository on.
    #[arg(long)]
    node: String,
    /// Git repository URL.
    repo_url: String,
    /// Git ref to check out. Defaults to the remote default branch.
    git_ref: Option<String>,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct ListArgs {
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then http://127.0.0.1:8090.
    #[arg(long)]
    cp_url: Option<String>,
}

#[derive(Args)]
struct OpenArgs {
    /// Session id to connect to.
    session_id: String,
    /// Control-plane base URL. Defaults to BOSUN_CP_URL, then http://127.0.0.1:8090.
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
        Command::Spawn(args) => run_spawn(args).await,
        Command::List(args) => run_list(args).await,
        Command::Open(args) => run_open(args).await,
    }
}

fn resolve_cp_url(flag: Option<&str>) -> String {
    flag.map(ToString::to_string)
        .or_else(|| env::var("BOSUN_CP_URL").ok())
        .unwrap_or_else(|| CliConfig::default().cp_url)
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
        client: reqwest::Client::new(),
        template_path: config.template_path.clone(),
        proxies: Arc::new(ProxyManager::new(config.proxy_bind.clone())),
    });
    let app = bosun_control::api::router(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;
    info!(listen_addr = %config.listen_addr, "control plane listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("control plane server failed")?;
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
        listen_port = config.listen_port,
        "node configured"
    );

    let manager = Arc::new(NodeManager::new(
        config.work_dir.clone(),
        config.advertise_addr.clone(),
    ));
    let app = bosun_node::api::router(manager.clone());
    let listen_addr = format!("{}:{}", config.advertise_addr, config.listen_port);
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind node HTTP server on {listen_addr}"))?;
    info!(listen_addr = %listen_addr, "node server listening");

    let _server_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            error!("node server failed: {}", e.display_chain());
        }
    });

    let client = reqwest::Client::new();
    let control_addr = format!("{}:{}", config.advertise_addr, config.listen_port);
    let mut interval = tokio::time::interval(Duration::from_secs(config.heartbeat_interval_secs));
    loop {
        interval.tick().await;
        let heartbeat = Heartbeat {
            node_name: config.node_name.clone(),
            status: NodeStatus::Up,
            control_addr: control_addr.clone(),
            sessions: manager.sessions(),
        };
        if let Err(e) = send_heartbeat(&client, &config, &heartbeat).await {
            warn!("heartbeat failed: {}", e.display_chain());
        }
    }
}

#[instrument(skip(client, heartbeat))]
async fn send_heartbeat(
    client: &reqwest::Client,
    config: &NodeConfig,
    heartbeat: &Heartbeat,
) -> anyhow::Result<()> {
    let url = format!("{}/heartbeat", config.cp_url.trim_end_matches('/'));
    client
        .post(&url)
        .json(&heartbeat)
        .send()
        .await
        .context("failed to send heartbeat")?
        .error_for_status()
        .context("control plane rejected heartbeat")?;
    debug!(node = %config.node_name, "heartbeat sent");
    Ok(())
}

async fn run_nodes(args: NodesArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref());

    let client = reqwest::Client::new();
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

async fn run_spawn(args: SpawnArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref());

    let client = reqwest::Client::new();
    let request = SpawnRequest {
        node: args.node.clone(),
        repo_url: args.repo_url,
        git_ref: args.git_ref,
    };
    let response = client
        .post(format!("{cp_url}/spawn"))
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
        return Err(anyhow::anyhow!("spawn failed: {text}"));
    }
    let health: SessionHealth =
        serde_json::from_str(&text).context("failed to parse spawn response")?;
    println!(
        "spawned session {} on node {} (status {})",
        health.id, health.node, health.status
    );
    match health.proxy_port {
        Some(port) => println!("{}", connect_command(&cp_url, port)),
        None => println!("session has no proxy port"),
    }
    Ok(())
}

async fn run_list(args: ListArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref());

    let client = reqwest::Client::new();
    let sessions: Vec<SessionHealth> = client
        .get(format!("{cp_url}/sessions"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?
        .error_for_status()
        .with_context(|| format!("control plane at {cp_url} returned an error"))?
        .json()
        .await
        .context("failed to parse session list")?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!(
        "{:<36}  {:<10}  {:<24}  {:<12}  {:<6}  {:<6}",
        "id", "node", "repo", "ref", "status", "port"
    );
    for session in &sessions {
        let git_ref = session.git_ref.as_deref().unwrap_or("-");
        let port = session
            .proxy_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<36}  {:<10}  {:<24}  {:<12}  {:<6}  {:<6}",
            session.id, session.node, session.repo_url, git_ref, session.status, port
        );
    }
    Ok(())
}

async fn run_open(args: OpenArgs) -> anyhow::Result<()> {
    let cp_url = resolve_cp_url(args.cp_url.as_deref());

    let client = reqwest::Client::new();
    let sessions: Vec<SessionHealth> = client
        .get(format!("{cp_url}/sessions"))
        .send()
        .await
        .with_context(|| format!("failed to reach control plane at {cp_url}"))?
        .error_for_status()
        .with_context(|| format!("control plane at {cp_url} returned an error"))?
        .json()
        .await
        .context("failed to parse session list")?;

    let Some(session) = sessions.into_iter().find(|s| s.id == args.session_id) else {
        return Err(anyhow::anyhow!("session {} not found", args.session_id));
    };
    match session.proxy_port {
        Some(port) => println!("{}", connect_command(&cp_url, port)),
        None => {
            return Err(anyhow::anyhow!("session {} has no proxy port", session.id));
        }
    }
    Ok(())
}

fn connect_command(cp_url: &str, proxy_port: u16) -> String {
    let host = reqwest::Url::parse(cp_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1".into());
    format!("opencode --hostname {host} --port {proxy_port}")
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
    fn connect_command_uses_url_host_and_port() {
        assert_eq!(
            connect_command("http://192.168.1.10:8090", 43210),
            "opencode --hostname 192.168.1.10 --port 43210"
        );
    }

    #[test]
    fn connect_command_defaults_host_when_url_is_invalid() {
        assert_eq!(
            connect_command("not a url", 43210),
            "opencode --hostname 127.0.0.1 --port 43210"
        );
    }
}
