//! `rkrt` — the Phase 1 development CLI over rk-runtime.
//!
//! This is the single-node container runner the spec's Phase 1 demo drives:
//! pull an image from a registry, run it in real namespaces and cgroups, see
//! it, stop it, remove it. The cluster CLI (`rk-ctl`) replaces day-to-day use
//! of this tool from Phase 2 on; `rkrt` stays useful for poking the runtime
//! directly.

use std::io::{Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use rk_runtime::cgroup::Limits;
use rk_runtime::container::{self, PullPolicy, RunOptions};
use rk_runtime::image::ImageRef;
use rk_runtime::registry;
use rk_runtime::store::Store;

fn main() -> ExitCode {
    // The shim path must never touch clap or tokio: it forks, and forking
    // after runtime threads exist is undefined-behavior bingo. Route it out
    // before anything else spins up.
    if std::env::args().nth(1).as_deref() == Some("shim") {
        return rk_runtime::init::shim_main();
    }
    if let Err(err) = rk_types::bootstrap::init_tracing() {
        eprintln!("rkrt: {err}");
        return ExitCode::FAILURE;
    }
    match run_cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Print the full error chain — the cause is usually the story.
            let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
            eprintln!("rkrt: {}", chain.join(": "));
            ExitCode::FAILURE
        }
    }
}

#[derive(Parser)]
#[command(
    name = "rkrt",
    about = "RustKube container runtime (Phase 1 dev CLI)",
    version
)]
struct Cli {
    /// State directory (defaults to /var/lib/rustkube, or $RKRT_ROOT).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pull an image and extract its layers into the local store.
    Pull {
        /// Image reference, e.g. `nginx`, `nginx:1.27`, `ghcr.io/owner/app:v1`.
        image: String,
    },
    /// Create and start a container.
    Run {
        /// Container name (lowercase, digits, '-').
        #[arg(long)]
        name: String,
        /// CPU limit, e.g. `500m` or `1.5`.
        #[arg(long, value_parser = parse_cpu)]
        cpus: Option<rk_types::Cpu>,
        /// Memory limit, e.g. `256Mi` or `1G`.
        #[arg(long, value_parser = parse_memory)]
        memory: Option<rk_types::Memory>,
        /// Extra environment, `KEY=value`; later entries win.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// When to pull the image.
        #[arg(long, value_parser = parse_pull_policy, default_value = "auto")]
        pull: PullPolicy,
        /// Replace the image ENTRYPOINT (Kubernetes `command` semantics).
        #[arg(long)]
        entrypoint: Option<String>,
        /// Image reference.
        image: String,
        /// Arguments replacing the image CMD (ENTRYPOINT still applies).
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// List containers.
    Ps,
    /// Stop a container (SIGTERM, then SIGKILL after the timeout).
    Stop {
        name: String,
        /// Seconds to wait before SIGKILL.
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
    /// Remove a stopped container (or a running one with --force).
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Print a container's console log.
    Logs {
        name: String,
        /// Keep printing as new output arrives.
        #[arg(long, short = 'f')]
        follow: bool,
    },
}

fn parse_cpu(raw: &str) -> Result<rk_types::Cpu, rk_types::QuantityError> {
    raw.parse()
}

fn parse_memory(raw: &str) -> Result<rk_types::Memory, rk_types::QuantityError> {
    raw.parse()
}

fn parse_pull_policy(raw: &str) -> Result<PullPolicy, String> {
    match raw {
        "auto" => Ok(PullPolicy::Auto),
        "always" => Ok(PullPolicy::Always),
        "never" => Ok(PullPolicy::Never),
        other => Err(format!("unknown pull policy {other:?} (auto|always|never)")),
    }
}

fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let store = Store::new(cli.root.unwrap_or_else(Store::default_root));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match cli.command {
        Cmd::Pull { image } => {
            let image: ImageRef = image.parse()?;
            store.ensure_layout()?;
            let report = runtime.block_on(registry::pull(&store, &image))?;
            println!("pulled   {}", report.reference);
            println!("manifest {}", report.manifest_digest);
            println!("config   {}", report.config_digest);
            for layer in &report.layers {
                let note = if layer.reused { "cached" } else { "fetched" };
                println!(
                    "layer    {}  {:>9}  {}",
                    layer.digest,
                    human_size(layer.size_bytes),
                    note
                );
            }
        }
        Cmd::Run {
            name,
            cpus,
            memory,
            env,
            pull,
            entrypoint,
            image,
            args,
        } => {
            let summary = runtime.block_on(container::run(
                &store,
                RunOptions {
                    name,
                    image,
                    limits: Limits { cpu: cpus, memory },
                    env,
                    command: entrypoint.map(|e| vec![e]),
                    args: (!args.is_empty()).then_some(args),
                    pull,
                    dns_server: None,
                    namespace: None,
                },
            ))?;
            println!(
                "started {} (id {}, pid1 {}, shim {})",
                summary.name, summary.id, summary.pid1, summary.shim_pid
            );
            println!(
                "address {} — curl http://{}/ from this host",
                summary.ip, summary.ip
            );
        }
        Cmd::Ps => {
            let rows = container::list(&store)?;
            println!(
                "{:<16} {:<36} {:<12} {:<14} {:<8} CREATED",
                "NAME", "IMAGE", "STATUS", "IP", "PID"
            );
            for row in rows {
                println!(
                    "{:<16} {:<36} {:<12} {:<14} {:<8} {}",
                    row.name,
                    row.image,
                    row.status.to_string(),
                    row.ip,
                    row.pid1.map_or_else(|| "-".to_owned(), |p| p.to_string()),
                    row.created_at
                );
            }
        }
        Cmd::Stop { name, timeout } => {
            let status = container::stop(&store, &name, Duration::from_secs(timeout))?;
            println!("{name}: {status}");
        }
        Cmd::Rm { name, force } => {
            container::remove(&store, &name, force)?;
            println!("removed {name}");
        }
        Cmd::Logs { name, follow } => {
            let path = container::logs_path(&store, &name)?;
            let mut file = std::fs::File::open(&path)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            std::io::stdout().write_all(&buffer)?;
            if follow {
                let mut offset = buffer.len() as u64;
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let len = file.metadata()?.len();
                    if len > offset {
                        file.seek(std::io::SeekFrom::Start(offset))?;
                        let mut chunk = Vec::new();
                        file.read_to_end(&mut chunk)?;
                        std::io::stdout().write_all(&chunk)?;
                        std::io::stdout().flush()?;
                        offset = len;
                    }
                }
            }
        }
    }
    Ok(())
}

fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
