//! Controller-manager configuration.

use serde::Deserialize;

fn default_api_url() -> String {
    "http://127.0.0.1:6443".to_owned()
}

const fn default_resync_secs() -> u64 {
    10
}

const fn default_node_not_ready_secs() -> u64 {
    30
}

const fn default_pod_eviction_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ControllersConfig {
    pub api_url: String,
    /// Seconds between full reconcile sweeps (watch events also trigger one).
    pub resync_interval_secs: u64,
    /// A node whose last heartbeat is older than this is marked NotReady.
    pub node_not_ready_secs: u64,
    /// A node NotReady at least this long has its pods evicted to reschedule.
    pub pod_eviction_secs: u64,
}

impl Default for ControllersConfig {
    fn default() -> Self {
        ControllersConfig {
            api_url: default_api_url(),
            resync_interval_secs: default_resync_secs(),
            node_not_ready_secs: default_node_not_ready_secs(),
            pod_eviction_secs: default_pod_eviction_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_defaults() {
        let config: ControllersConfig =
            serde_yaml::from_str("apiUrl: http://10.0.0.1:6443\nnodeNotReadySecs: 15\n").unwrap();
        assert_eq!(config.api_url, "http://10.0.0.1:6443");
        assert_eq!(config.node_not_ready_secs, 15);

        let defaults = ControllersConfig::default();
        assert_eq!(defaults.api_url, "http://127.0.0.1:6443");
        assert_eq!(defaults.resync_interval_secs, 10);
        assert_eq!(defaults.node_not_ready_secs, 30);
        assert_eq!(defaults.pod_eviction_secs, 60);
    }
}
