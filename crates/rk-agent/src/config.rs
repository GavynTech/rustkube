//! Agent configuration.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

fn default_api_url() -> String {
    "http://127.0.0.1:6443".to_owned()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/rustkube")
}

const fn default_heartbeat_secs() -> u64 {
    10
}

const fn default_resync_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentConfig {
    pub api_url: String,
    /// Node name to register as; defaults to the machine hostname.
    pub node_name: Option<String>,
    /// Root for image layers, container rootfs mounts and logs.
    pub data_dir: PathBuf,
    /// Node address advertised in status; defaults to the first non-loopback
    /// interface address.
    pub node_ip: Option<IpAddr>,
    /// Seconds between node heartbeats.
    pub heartbeat_interval_secs: u64,
    /// Seconds between full reconcile passes (watch events also trigger one).
    pub resync_interval_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            api_url: default_api_url(),
            node_name: None,
            data_dir: default_data_dir(),
            node_ip: None,
            heartbeat_interval_secs: default_heartbeat_secs(),
            resync_interval_secs: default_resync_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_defaults() {
        let config: AgentConfig = serde_yaml::from_str(
            "apiUrl: http://10.0.0.1:6443\nnodeName: worker-1\ndataDir: /tmp/rk\nheartbeatIntervalSecs: 3\n",
        )
        .unwrap();
        assert_eq!(config.node_name.as_deref(), Some("worker-1"));
        assert_eq!(config.data_dir, PathBuf::from("/tmp/rk"));
        assert_eq!(config.heartbeat_interval_secs, 3);

        let defaults = AgentConfig::default();
        assert!(defaults.node_name.is_none());
        assert_eq!(defaults.data_dir, PathBuf::from("/var/lib/rustkube"));
        assert_eq!(defaults.heartbeat_interval_secs, 10);
        assert_eq!(defaults.resync_interval_secs, 5);
    }
}
