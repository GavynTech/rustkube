use std::path::PathBuf;

use clap::Parser;
use rk_agent::AgentConfig;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "rk-agent", version, about = "RustKube node agent")]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/agent.yaml, /etc/rustkube/agent.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: AgentConfig = rk_types::bootstrap::load_config(args.config.as_deref(), "agent")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        node_name = config.node_name.as_deref().unwrap_or("<hostname at registration>"),
        data_dir = %config.data_dir.display(),
        "rk-agent starting"
    );
    info!("pod reconciliation arrives in Phase 3; exiting cleanly");
    Ok(())
}
