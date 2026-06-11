use std::net::SocketAddr;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiConfig {
    pub listen_addr: SocketAddr,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 6443)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_from_yaml() {
        let config: ApiConfig = serde_yaml::from_str("listenAddr: 0.0.0.0:7443\n").unwrap();
        assert_eq!(config.listen_addr, SocketAddr::from(([0, 0, 0, 0], 7443)));
    }

    #[test]
    fn default_binds_loopback() {
        assert_eq!(
            ApiConfig::default().listen_addr,
            SocketAddr::from(([127, 0, 0, 1], 6443))
        );
    }
}
