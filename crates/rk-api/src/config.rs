use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiConfig {
    pub listen_addr: SocketAddr,
    /// sqlite database path; parent directories are created at startup.
    pub db_path: PathBuf,
    /// File holding the bearer token clients must present. When unset, the API
    /// runs open (a dev cluster); when set, every request except `/healthz`
    /// needs `Authorization: Bearer <token>`.
    pub auth_token_file: Option<PathBuf>,
    /// When set, the server speaks HTTPS using this certificate/key pair;
    /// otherwise it serves plaintext HTTP.
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// PEM-encoded server certificate chain.
    pub cert_file: PathBuf,
    /// PEM-encoded private key.
    pub key_file: PathBuf,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 6443)),
            db_path: PathBuf::from("rustkube.db"),
            auth_token_file: None,
            tls: None,
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
        assert!(config.auth_token_file.is_none());
        assert!(config.tls.is_none());
    }

    #[test]
    fn config_parses_auth_and_tls() {
        let config: ApiConfig = serde_yaml::from_str(
            "listenAddr: 0.0.0.0:6443\ndbPath: rk.db\nauthTokenFile: /etc/rustkube/pki/token\ntls:\n  certFile: /etc/rustkube/pki/server.crt\n  keyFile: /etc/rustkube/pki/server.key\n",
        )
        .unwrap();
        assert_eq!(
            config.auth_token_file,
            Some(PathBuf::from("/etc/rustkube/pki/token"))
        );
        let tls = config.tls.expect("tls present");
        assert_eq!(tls.cert_file, PathBuf::from("/etc/rustkube/pki/server.crt"));
        assert_eq!(tls.key_file, PathBuf::from("/etc/rustkube/pki/server.key"));
    }

    #[test]
    fn defaults_bind_loopback_with_a_local_db() {
        let config = ApiConfig::default();
        assert_eq!(config.listen_addr, SocketAddr::from(([127, 0, 0, 1], 6443)));
        assert_eq!(config.db_path, PathBuf::from("rustkube.db"));
    }
}
