//! RustKube scheduler: watches for pods with no `nodeName`, scores Ready
//! nodes by free CPU/memory (resource-aware bin packing), honors
//! `nodeSelector`, and writes the binding back through the API server.
//! The scheduling loop itself lands in Phase 4.

use serde::Deserialize;

fn default_api_url() -> String {
    "http://127.0.0.1:6443".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SchedulerConfig {
    pub api_url: String,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig {
            api_url: default_api_url(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_defaults() {
        let config: SchedulerConfig =
            serde_yaml::from_str("apiUrl: http://10.0.0.1:6443\n").unwrap();
        assert_eq!(config.api_url, "http://10.0.0.1:6443");
        assert_eq!(SchedulerConfig::default().api_url, "http://127.0.0.1:6443");
    }
}
