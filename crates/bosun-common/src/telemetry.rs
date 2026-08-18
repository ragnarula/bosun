use std::env;

use anyhow::Context;
use tracing::Level;
use tracing_subscriber::EnvFilter;

pub fn setup_logging(cli_filter: Option<&str>) -> Result<(), anyhow::Error> {
    let filter = cli_filter
        .map(ToString::to_string)
        .or_else(|| env::var("RUST_LOG").ok())
        .unwrap_or_else(|| Level::INFO.to_string());

    let filter = EnvFilter::try_new(filter).context("failed to parse log filter")?;

    tracing_subscriber::fmt().with_env_filter(filter).init();
    Ok(())
}
