use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use rk_api::config::ApiConfig;
use rk_api::resources::AppState;
use rk_store::SqliteStore;
use tracing::{error, info, warn};

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

    if let Some(parent) = config.db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let store = SqliteStore::open(&config.db_path)
        .await
        .with_context(|| format!("failed to open store at {}", config.db_path.display()))?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        addr = %config.listen_addr,
        db = %config.db_path.display(),
        tls = config.tls.is_some(),
        "rk-api starting"
    );

    let state = AppState {
        store: Arc::new(store),
    };
    let mut app = rk_api::server::router(state);

    // Bearer-token auth, if a token is configured.
    match load_token(&config)? {
        Some(token) => {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::<str>::from(token.as_str()),
                rk_api::auth::require_bearer,
            ));
            info!("bearer-token authentication enabled");
        }
        None => warn!("authentication disabled: no authTokenFile configured (dev mode)"),
    }

    // HTTPS when a cert/key is configured, plaintext otherwise.
    match &config.tls {
        Some(tls) => {
            // rustls 0.23 needs a process-wide crypto provider chosen explicitly.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let rustls_config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert_file, &tls.key_file)
                    .await
                    .with_context(|| {
                        format!(
                            "loading TLS cert {} / key {}",
                            tls.cert_file.display(),
                            tls.key_file.display()
                        )
                    })?;
            let handle = axum_server::Handle::new();
            tokio::spawn(graceful_shutdown(handle.clone()));
            info!(addr = %config.listen_addr, "serving HTTPS");
            axum_server::bind_rustls(config.listen_addr, rustls_config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
                .context("server error")?;
        }
        None => {
            warn!("serving plaintext HTTP (no tls configured)");
            let listener = tokio::net::TcpListener::bind(config.listen_addr)
                .await
                .with_context(|| format!("failed to bind {}", config.listen_addr))?;
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .context("server error")?;
        }
    }

    info!("rk-api stopped");
    Ok(())
}

/// Read the bearer token from the configured file, trimming whitespace.
fn load_token(config: &ApiConfig) -> anyhow::Result<Option<String>> {
    let Some(path) = &config.auth_token_file else {
        return Ok(None);
    };
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading auth token {}", path.display()))?
        .trim()
        .to_owned();
    if token.is_empty() {
        anyhow::bail!("auth token file {} is empty", path.display());
    }
    Ok(Some(token))
}

/// Wait for SIGINT, then ask axum-server to drain in-flight requests.
async fn graceful_shutdown(handle: axum_server::Handle) {
    shutdown_signal().await;
    handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
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
