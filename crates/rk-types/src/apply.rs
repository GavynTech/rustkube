//! Kind-dispatched handling of arbitrary manifests, used by `rk-ctl apply`
//! and the API server's generic endpoints.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Job;
use crate::cluster::{Namespace, Node};
use crate::data::{ConfigMap, Secret};
use crate::ingress::Ingress;
use crate::meta::{ObjectMeta, ResourceKind};
use crate::pod::Pod;
use crate::service::Service;
use crate::workloads::{Deployment, ReplicaSet};

/// Any RustKube resource, dispatched on the manifest's `kind` field.
///
/// Serialization delegates to the inner resource (which carries its own
/// `kind`), and deserialization peeks at `kind` before picking the variant —
/// avoiding serde's tagged-enum representation, which would duplicate the
/// `kind` key on output.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // resources legitimately differ in size; boxing would complicate every match for negligible gain
pub enum AnyObject {
    Job(Job),
    Pod(Pod),
    ReplicaSet(ReplicaSet),
    Deployment(Deployment),
    Service(Service),
    Ingress(Ingress),
    Node(Node),
    Namespace(Namespace),
    ConfigMap(ConfigMap),
    Secret(Secret),
}

impl AnyObject {
    pub fn kind(&self) -> ResourceKind {
        match self {
            AnyObject::Job(_) => ResourceKind::Job,
            AnyObject::Pod(_) => ResourceKind::Pod,
            AnyObject::ReplicaSet(_) => ResourceKind::ReplicaSet,
            AnyObject::Deployment(_) => ResourceKind::Deployment,
            AnyObject::Service(_) => ResourceKind::Service,
            AnyObject::Ingress(_) => ResourceKind::Ingress,
            AnyObject::Node(_) => ResourceKind::Node,
            AnyObject::Namespace(_) => ResourceKind::Namespace,
            AnyObject::ConfigMap(_) => ResourceKind::ConfigMap,
            AnyObject::Secret(_) => ResourceKind::Secret,
        }
    }

    pub fn metadata(&self) -> &ObjectMeta {
        match self {
            AnyObject::Job(o) => &o.metadata,
            AnyObject::Pod(o) => &o.metadata,
            AnyObject::ReplicaSet(o) => &o.metadata,
            AnyObject::Deployment(o) => &o.metadata,
            AnyObject::Service(o) => &o.metadata,
            AnyObject::Ingress(o) => &o.metadata,
            AnyObject::Node(o) => &o.metadata,
            AnyObject::Namespace(o) => &o.metadata,
            AnyObject::ConfigMap(o) => &o.metadata,
            AnyObject::Secret(o) => &o.metadata,
        }
    }

    pub fn name(&self) -> &str {
        &self.metadata().name
    }

    /// Parse a (possibly multi-document) YAML manifest stream. Empty
    /// documents — e.g. a trailing `---` — are skipped.
    pub fn from_yaml_multi(input: &str) -> Result<Vec<AnyObject>, ApplyError> {
        let mut objects = Vec::new();
        for document in serde_yaml::Deserializer::from_str(input) {
            let value = serde_json::Value::deserialize(document)?;
            if value.is_null() {
                continue;
            }
            objects.push(serde_json::from_value(value)?);
        }
        Ok(objects)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("invalid YAML manifest: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("invalid manifest object: {0}")]
    Object(#[from] serde_json::Error),
}

impl Serialize for AnyObject {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AnyObject::Job(o) => o.serialize(serializer),
            AnyObject::Pod(o) => o.serialize(serializer),
            AnyObject::ReplicaSet(o) => o.serialize(serializer),
            AnyObject::Deployment(o) => o.serialize(serializer),
            AnyObject::Service(o) => o.serialize(serializer),
            AnyObject::Ingress(o) => o.serialize(serializer),
            AnyObject::Node(o) => o.serialize(serializer),
            AnyObject::Namespace(o) => o.serialize(serializer),
            AnyObject::ConfigMap(o) => o.serialize(serializer),
            AnyObject::Secret(o) => o.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AnyObject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = serde_json::Value::deserialize(deserializer)?;
        let kind_str = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::missing_field("kind"))?;
        let kind: ResourceKind = kind_str.parse().map_err(D::Error::custom)?;

        fn dispatch<T, E>(value: serde_json::Value) -> Result<T, E>
        where
            T: DeserializeOwned,
            E: serde::de::Error,
        {
            serde_json::from_value(value).map_err(E::custom)
        }

        Ok(match kind {
            ResourceKind::Job => AnyObject::Job(dispatch(value)?),
            ResourceKind::Pod => AnyObject::Pod(dispatch(value)?),
            ResourceKind::ReplicaSet => AnyObject::ReplicaSet(dispatch(value)?),
            ResourceKind::Deployment => AnyObject::Deployment(dispatch(value)?),
            ResourceKind::Service => AnyObject::Service(dispatch(value)?),
            ResourceKind::Ingress => AnyObject::Ingress(dispatch(value)?),
            ResourceKind::Node => AnyObject::Node(dispatch(value)?),
            ResourceKind::Namespace => AnyObject::Namespace(dispatch(value)?),
            ResourceKind::ConfigMap => AnyObject::ConfigMap(dispatch(value)?),
            ResourceKind::Secret => AnyObject::Secret(dispatch(value)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_document_manifest_parses_in_order() {
        let manifest = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: web-config
data:
  MODE: production
---
apiVersion: v1
kind: Pod
metadata:
  name: web-0
spec:
  containers:
    - name: nginx
      image: nginx:1.27
---
"#;
        let objects = AnyObject::from_yaml_multi(manifest).unwrap();
        assert_eq!(objects.len(), 2, "trailing empty document is skipped");
        assert_eq!(objects[0].kind(), ResourceKind::ConfigMap);
        assert_eq!(objects[1].kind(), ResourceKind::Pod);
        assert_eq!(objects[1].name(), "web-0");
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let result = AnyObject::from_yaml_multi("kind: Gizmo\nmetadata:\n  name: x\n");
        assert!(result.is_err());
    }

    #[test]
    fn missing_kind_is_an_error() {
        let result = AnyObject::from_yaml_multi("metadata:\n  name: x\n");
        assert!(result.is_err());
    }

    #[test]
    fn serialization_does_not_duplicate_kind() {
        let objects =
            AnyObject::from_yaml_multi("kind: Namespace\nmetadata:\n  name: prod\n").unwrap();
        let yaml = serde_yaml::to_string(&objects[0]).unwrap();
        assert_eq!(yaml.matches("kind:").count(), 1);
        let back: AnyObject = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(objects[0], back);
    }
}
