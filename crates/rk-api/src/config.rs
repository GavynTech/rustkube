use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiConfig {
    pub listen_addr: SocketAddr,
    /// sqlite database path; parent directories are created at startup.
    pub db_path: PathBuf,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 6443)),
            db_path: PathBuf::from("rustkube.db"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_from_yaml() {
        let config: ApiConfig =
            serde_yaml::from_str("listenAddr: 0.0.0.0:7443\ndbPath: /tmp/rk.db\n").unwrap();
        assert_eq!(config.listen_addr, SocketAddr::from(([0, 0, 0, 0], 7443)));
        assert_eq!(config.db_path, PathBuf::from("/tmp/rk.db"));
    }

    #[test]
    fn defaults_bind_loopback_with_a_local_db() {
        let config = ApiConfig::default();
        assert_eq!(config.listen_addr, SocketAddr::from(([127, 0, 0, 1], 6443)));
        assert_eq!(config.db_path, PathBuf::from("rustkube.db"));
    }
}
