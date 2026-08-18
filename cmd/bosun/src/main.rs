use std::path::PathBuf;

use anyhow::Context;
use bosun_common::config::ControlConfig;
use bosun_common::config::NodeConfig;
use bosun_common::config::load_config;
use bosun_common::telemetry::setup_logging;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => run_serve(args),
        Command::Node(args) => run_node(args),
    }
}

fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    setup_logging(args.log_filter.as_deref())?;
    let config: ControlConfig =
        load_config(&args.config).context("failed to load control-plane config")?;
    info!(
        listen_addr = %config.listen_addr,
        template_path = %config.template_path.display(),
        node_timeout_secs = config.node_timeout_secs,
        proxy_bind = %config.proxy_bind,
        "control plane configured"
    );
    Ok(())
}

fn run_node(args: NodeArgs) -> anyhow::Result<()> {
    setup_logging(args.log_filter.as_deref())?;
    let config: NodeConfig = load_config(&args.config).context("failed to load node config")?;
    info!(
        cp_url = %config.cp_url,
        node_name = %config.node_name,
        work_dir = %config.work_dir.display(),
        advertise_addr = %config.advertise_addr,
        heartbeat_interval_secs = config.heartbeat_interval_secs,
        "node configured"
    );
    Ok(())
}
