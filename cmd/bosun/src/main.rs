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
use bosun_control::registry::NodeRegistry;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use tracing::debug;
use tracing::info;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => run_serve(args).await,
        Command::Node(args) => run_node(args).await,
        Command::Nodes(args) => run_nodes(args).await,
    }
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

    let registry = Arc::new(NodeRegistry::new(Duration::from_secs(
        config.node_timeout_secs,
    )));
    let app = bosun_control::api::router(registry);

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
        "node configured"
    );

    let client = reqwest::Client::new();
    let mut interval = tokio::time::interval(Duration::from_secs(config.heartbeat_interval_secs));
    loop {
        interval.tick().await;
        if let Err(e) = send_heartbeat(&client, &config).await {
            warn!("heartbeat failed: {}", e.display_chain());
        }
    }
}

async fn send_heartbeat(client: &reqwest::Client, config: &NodeConfig) -> anyhow::Result<()> {
    let url = format!("{}/heartbeat", config.cp_url.trim_end_matches('/'));
    let body = Heartbeat {
        node_name: config.node_name.clone(),
        status: NodeStatus::Up,
    };
    client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("failed to send heartbeat")?
        .error_for_status()
        .context("control plane rejected heartbeat")?;
    debug!(node = %config.node_name, "heartbeat sent");
    Ok(())
}

async fn run_nodes(args: NodesArgs) -> anyhow::Result<()> {
    let cp_url = args
        .cp_url
        .or_else(|| env::var("BOSUN_CP_URL").ok())
        .unwrap_or_else(|| CliConfig::default().cp_url);

    let client = reqwest::Client::new();
    let health: Vec<bosun_control::registry::NodeHealth> = client
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
}
