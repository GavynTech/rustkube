//! ConfigMap and Secret: configuration data mounted into pods as files or
//! injected as environment variables.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::Resource;
use crate::meta::{ObjectMeta, ResourceKind, api_version_v1};

fn kind_config_map() -> String {
    "ConfigMap".to_owned()
}

fn kind_secret() -> String {
    "Secret".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMap {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_config_map")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, String>,
}

impl Resource for ConfigMap {
    const KIND: ResourceKind = ResourceKind::ConfigMap;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SecretError {
    #[error("secret has no key {0:?}")]
    MissingKey(String),
    #[error("value is not valid base64: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Like Kubernetes: `data` holds base64-encoded values at rest, `stringData`
/// is a write-only convenience that the API server folds into `data` on
/// admission (see [`Secret::normalize`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_secret")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub string_data: BTreeMap<String, String>,
}

impl Resource for Secret {
    const KIND: ResourceKind = ResourceKind::Secret;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

impl Secret {
    /// Fold plaintext `stringData` entries into base64 `data`, overwriting
    /// duplicates — the same admission behavior as Kubernetes.
    pub fn normalize(&mut self) {
        let string_data = std::mem::take(&mut self.string_data);
        for (key, value) in string_data {
            self.data.insert(key, BASE64.encode(value.as_bytes()));
        }
    }

    pub fn decode(&self, key: &str) -> Result<Vec<u8>, SecretError> {
        let encoded = self
            .data
            .get(key)
            .ok_or_else(|| SecretError::MissingKey(key.to_owned()))?;
        Ok(BASE64.decode(encoded.as_bytes())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_normalize_encodes_string_data() {
        let mut secret: Secret =
            serde_yaml::from_str("metadata:\n  name: creds\nstringData:\n  password: hunter2\n")
                .unwrap();
        secret.normalize();
        assert!(secret.string_data.is_empty());
        assert_eq!(secret.data["password"], "aHVudGVyMg==");
        assert_eq!(secret.decode("password").unwrap(), b"hunter2");
    }

    #[test]
    fn secret_decode_reports_missing_keys() {
        let secret: Secret = serde_yaml::from_str("metadata:\n  name: creds\n").unwrap();
        assert!(matches!(
            secret.decode("nope"),
            Err(SecretError::MissingKey(_))
        ));
    }

    #[test]
    fn config_map_round_trips() {
        let cm: ConfigMap =
            serde_yaml::from_str("metadata:\n  name: web-config\ndata:\n  MODE: production\n")
                .unwrap();
        assert_eq!(cm.data["MODE"], "production");
        let yaml = serde_yaml::to_string(&cm).unwrap();
        let back: ConfigMap = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(cm, back);
    }
}
