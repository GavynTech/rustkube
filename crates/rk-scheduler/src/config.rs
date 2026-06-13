//! Scheduler configuration.

use serde::Deserialize;

fn default_api_url() -> String {
    "http://127.0.0.1:6443".to_owned()
}

const fn default_resync_secs() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SchedulerConfig {
    pub api_url: String,
    /// Seconds between full scheduling sweeps (watch events also trigger one).
    pub resync_interval_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig {
            api_url: default_api_url(),
            resync_interval_secs: default_resync_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_defaults() {
        let config: SchedulerConfig =
            serde_yaml::from_str("apiUrl: http://10.0.0.1:6443\nresyncIntervalSecs: 3\n").unwrap();
        assert_eq!(config.api_url, "http://10.0.0.1:6443");
        assert_eq!(config.resync_interval_secs, 3);
        assert_eq!(SchedulerConfig::default().api_url, "http://127.0.0.1:6443");
        assert_eq!(SchedulerConfig::default().resync_interval_secs, 10);
    }
}
