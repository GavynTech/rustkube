use std::path::PathBuf;

use clap::Parser;
use rk_scheduler::SchedulerConfig;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "rk-scheduler", version, about = "RustKube pod scheduler")]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/scheduler.yaml, /etc/rustkube/scheduler.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: SchedulerConfig =
        rk_types::bootstrap::load_config(args.config.as_deref(), "scheduler")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        "rk-scheduler starting"
    );
    info!("scheduling loop arrives in Phase 4; exiting cleanly");
    Ok(())
}
