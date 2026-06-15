//! Agent configuration.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use rk_types::{Cpu, Memory};
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

// `#[serde(default)]` on the struct fills any missing field from this
// `Default` impl, so each field gets its real default (not e.g. an empty
// api_url).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentConfig {
    pub api_url: String,
    /// File holding the bearer token to present to the API server (matches the
    /// server's `authTokenFile`). Unset means no `Authorization` header.
    pub token_file: Option<PathBuf>,
    /// PEM CA certificate that signed the API server's TLS cert, so HTTPS to it
    /// verifies. Unset means the system trust roots only.
    pub ca_file: Option<PathBuf>,
    /// Node name to register as; defaults to the machine hostname.
    pub node_name: Option<String>,
    /// Root for image layers, container rootfs mounts and logs.
    pub data_dir: PathBuf,
    /// Node address advertised in status; defaults to the first non-loopback
    /// interface address.
    pub node_ip: Option<IpAddr>,
    /// The /24 this node carves pod IPs from (e.g. `10.42.1.0/24`). Distinct
    /// per simulated node so their pod networks don't alias. Defaults to
    /// 10.42.0.0/24.
    pub pod_cidr: Option<String>,
    /// Labels advertised on the Node object, for `nodeSelector` scheduling
    /// (e.g. `disk: ssd`). The hostname label is always added on top.
    pub node_labels: BTreeMap<String, String>,
    /// Dev override for advertised CPU capacity (e.g. `2` or `500m`). Without
    /// it, the agent reports the machine's real core count — which, for
    /// netns-simulated nodes sharing a host, is identical on every node and
    /// makes resource-based scheduling hard to demo.
    pub capacity_cpu: Option<Cpu>,
    /// Dev override for advertised memory capacity (e.g. `512Mi`).
    pub capacity_memory: Option<Memory>,
    /// Seconds between node heartbeats.
    pub heartbeat_interval_secs: u64,
    /// Seconds between full reconcile passes (watch events also trigger one).
    pub resync_interval_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            api_url: default_api_url(),
            token_file: None,
            ca_file: None,
            node_name: None,
            data_dir: default_data_dir(),
            node_ip: None,
            pod_cidr: None,
            node_labels: BTreeMap::new(),
            capacity_cpu: None,
            capacity_memory: None,
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
            "apiUrl: http://10.0.0.1:6443\n\
             nodeName: worker-1\n\
             dataDir: /tmp/rk\n\
             podCidr: 10.42.1.0/24\n\
             nodeLabels: { disk: ssd }\n\
             capacityCpu: \"2\"\n\
             capacityMemory: 512Mi\n\
             heartbeatIntervalSecs: 3\n",
        )
        .unwrap();
        assert_eq!(config.node_name.as_deref(), Some("worker-1"));
        assert_eq!(config.data_dir, PathBuf::from("/tmp/rk"));
        assert_eq!(config.pod_cidr.as_deref(), Some("10.42.1.0/24"));
        assert_eq!(config.node_labels["disk"], "ssd");
        assert_eq!(config.capacity_cpu.unwrap().as_millis(), 2000);
        assert_eq!(config.capacity_memory.unwrap().as_bytes(), 512 << 20);
        assert_eq!(config.heartbeat_interval_secs, 3);

        let defaults = AgentConfig::default();
        assert!(defaults.node_name.is_none());
        assert_eq!(defaults.api_url, "http://127.0.0.1:6443");
        assert_eq!(defaults.data_dir, PathBuf::from("/var/lib/rustkube"));
        assert_eq!(defaults.heartbeat_interval_secs, 10);
        assert_eq!(defaults.resync_interval_secs, 5);
        assert!(defaults.pod_cidr.is_none());
        assert!(defaults.node_labels.is_empty());
    }
}
