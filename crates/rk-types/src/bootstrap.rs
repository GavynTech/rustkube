//! Startup plumbing shared by every RustKube binary: tracing initialisation
//! and config-file loading.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("failed to initialise tracing: {0}")]
    Tracing(String),
}

/// Install the global tracing subscriber. Verbosity comes from `RUST_LOG`
/// (default `info`).
pub fn init_tracing() -> Result<(), BootstrapError> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .map_err(|err| BootstrapError::Tracing(err.to_string()))
}

/// Load a component's config. Resolution order:
///
/// 1. an explicit path (`--config`) — missing or invalid is a hard error;
/// 2. `$RK_CONFIG` — same;
/// 3. `./config/<component>.yaml`, then `/etc/rustkube/<component>.yaml`,
///    whichever exists first;
/// 4. the type's `Default` when no file is found.
pub fn load_config<T>(explicit: Option<&Path>, component: &str) -> Result<T, BootstrapError>
where
    T: DeserializeOwned + Default,
{
    if let Some(path) = explicit {
        return read_config(path);
    }
    if let Ok(env_path) = std::env::var("RK_CONFIG") {
        return read_config(Path::new(&env_path));
    }
    let candidates = [
        PathBuf::from(format!("config/{component}.yaml")),
        PathBuf::from(format!("/etc/rustkube/{component}.yaml")),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return read_config(&candidate);
        }
    }
    tracing::info!(component, "no config file found, using built-in defaults");
    Ok(T::default())
}

fn read_config<T: DeserializeOwned>(path: &Path) -> Result<T, BootstrapError> {
    let raw = std::fs::read_to_string(path).map_err(|source| BootstrapError::Read {
        path: path.to_owned(),
        source,
    })?;
    let config = serde_yaml::from_str(&raw).map_err(|source| BootstrapError::Parse {
        path: path.to_owned(),
        source,
    })?;
    tracing::info!(path = %path.display(), "loaded config");
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Default, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase", default)]
    struct DemoConfig {
        listen_addr: String,
        replicas: u32,
    }

    #[test]
    fn explicit_config_file_is_loaded() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "listenAddr: 0.0.0.0:6443\nreplicas: 3").unwrap();
        let config: DemoConfig = load_config(Some(file.path()), "demo").unwrap();
        assert_eq!(config.listen_addr, "0.0.0.0:6443");
        assert_eq!(config.replicas, 3);
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let result: Result<DemoConfig, _> =
            load_config(Some(Path::new("/nonexistent/rk.yaml")), "demo");
        assert!(matches!(result, Err(BootstrapError::Read { .. })));
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "listenAddr: [unclosed").unwrap();
        let result: Result<DemoConfig, _> = load_config(Some(file.path()), "demo");
        assert!(matches!(result, Err(BootstrapError::Parse { .. })));
    }

    #[test]
    fn defaults_apply_when_no_file_exists() {
        // Run from a temp dir so a developer's local ./config/ can't leak in.
        let dir = tempfile::tempdir().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let config: DemoConfig = load_config(None, "demo").unwrap();
        std::env::set_current_dir(original).unwrap();
        assert_eq!(config, DemoConfig::default());
    }
}
