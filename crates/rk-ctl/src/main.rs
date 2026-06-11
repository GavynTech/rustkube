//! RustKube CLI. Subcommands appear as their backends land: `apply`, `get`,
//! `describe`, `delete` and `logs` arrive with the API server in Phase 2,
//! `scale` with the controllers in Phase 5, `drain` in Phase 8 — no dead
//! flags in the meantime.

use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rk-ctl", version, about = "RustKube CLI")]
struct Cli {
    /// API server base URL.
    #[arg(
        long,
        global = true,
        env = "RK_API_URL",
        default_value = "http://127.0.0.1:6443"
    )]
    api_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the client version and, when reachable, the server version.
    Version,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => version(&cli.api_url).await,
    }
}

/// Print one line, exiting quietly if the reader of our stdout has gone away
/// (e.g. `rk-ctl ... | head`) — a closed pipe is not an error for a CLI.
fn print_line(line: &str) {
    use std::io::Write as _;
    if writeln!(std::io::stdout(), "{line}").is_err() {
        std::process::exit(0);
    }
}

async fn version(api_url: &str) -> anyhow::Result<()> {
    print_line(&format!("client: rk-ctl v{}", env!("CARGO_PKG_VERSION")));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    match client.get(format!("{api_url}/version")).send().await {
        Ok(response) if response.status().is_success() => {
            let info: serde_json::Value = response.json().await?;
            print_line(&format!(
                "server: {} v{}",
                info["component"].as_str().unwrap_or("<unknown>"),
                info["version"].as_str().unwrap_or("<unknown>"),
            ));
        }
        Ok(response) => print_line(&format!("server: error ({})", response.status())),
        Err(_) => print_line(&format!("server: unreachable at {api_url}")),
    }
    Ok(())
}
