//! RustKube controller manager: the generic reconcile-loop framework plus the
//! Deployment, ReplicaSet and Node controllers. The loops land in Phase 5;
//! node-death handling builds on the heartbeats added in Phase 3.

use serde::Deserialize;

fn default_api_url() -> String {
    "http://127.0.0.1:6443".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ControllersConfig {
    pub api_url: String,
}

impl Default for ControllersConfig {
    fn default() -> Self {
        ControllersConfig {
            api_url: default_api_url(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_defaults() {
        let config: ControllersConfig =
            serde_yaml::from_str("apiUrl: http://10.0.0.1:6443\n").unwrap();
        assert_eq!(config.api_url, "http://10.0.0.1:6443");
        assert_eq!(
            ControllersConfig::default().api_url,
            "http://127.0.0.1:6443"
        );
    }
}
