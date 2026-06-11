use std::path::PathBuf;

use clap::Parser;
use rk_controllers::ControllersConfig;
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "rk-controllers",
    version,
    about = "RustKube controller manager"
)]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/controllers.yaml, /etc/rustkube/controllers.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: ControllersConfig =
        rk_types::bootstrap::load_config(args.config.as_deref(), "controllers")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        "rk-controllers starting"
    );
    info!("reconcile loops arrive in Phase 5; exiting cleanly");
    Ok(())
}
