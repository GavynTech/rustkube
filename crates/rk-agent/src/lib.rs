//! RustKube node agent (kubelet equivalent): registers its Node, heartbeats,
//! watches the API for pods bound to it and reconciles reality to match using
//! `rk-runtime` and `rk-net`. The reconcile loop lands in Phase 3.

use std::path::PathBuf;

use serde::Deserialize;

fn default_api_url() -> String {
    "http://127.0.0.1:6443".to_owned()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/rustkube")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentConfig {
    pub api_url: String,
    /// Node name to register as; defaults to the machine hostname at
    /// registration time (Phase 3).
    pub node_name: Option<String>,
    /// Root for image layers, container rootfs mounts and logs.
    pub data_dir: PathBuf,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            api_url: default_api_url(),
            node_name: None,
            data_dir: default_data_dir(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_defaults() {
        let config: AgentConfig = serde_yaml::from_str(
            "apiUrl: http://10.0.0.1:6443\nnodeName: worker-1\ndataDir: /tmp/rk\n",
        )
        .unwrap();
        assert_eq!(config.node_name.as_deref(), Some("worker-1"));
        assert_eq!(config.data_dir, PathBuf::from("/tmp/rk"));

        let defaults = AgentConfig::default();
        assert!(defaults.node_name.is_none());
        assert_eq!(defaults.data_dir, PathBuf::from("/var/lib/rustkube"));
    }
}
