use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use rk_api::config::ApiConfig;
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(name = "rk-api", version, about = "RustKube control-plane API server")]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/api.yaml, /etc/rustkube/api.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: ApiConfig = rk_types::bootstrap::load_config(args.config.as_deref(), "api")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        addr = %config.listen_addr,
        "rk-api starting"
    );

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;

    axum::serve(listener, rk_api::server::router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    info!("rk-api stopped");
    Ok(())
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("shutdown signal received"),
        Err(err) => {
            error!(%err, "failed to listen for the shutdown signal; running until killed");
            std::future::pending::<()>().await;
        }
    }
}
