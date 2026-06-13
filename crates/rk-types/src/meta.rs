//! Object metadata, label selectors and the resource-kind enum.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub(crate) fn api_version_v1() -> String {
    "v1".to_owned()
}

/// Metadata common to every resource. `uid`, `resourceVersion` and
/// `creationTimestamp` are assigned by the API server; clients leave them
/// unset. Resource versions are plain integers (the store revision) rather
/// than Kubernetes' opaque strings — simpler, and the optimistic-concurrency
/// semantics are identical.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ObjectMeta {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<u64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub owner_references: Vec<OwnerReference>,
}

impl ObjectMeta {
    pub fn named(name: impl Into<String>) -> Self {
        ObjectMeta {
            name: name.into(),
            ..ObjectMeta::default()
        }
    }

    /// Namespace to operate in: the explicit one, or `default`.
    pub fn namespace_or_default(&self) -> &str {
        self.namespace.as_deref().unwrap_or("default")
    }
}

/// Links a dependent object (e.g. a Pod) to the controller that owns it
/// (e.g. a ReplicaSet), so controllers can find and adopt their children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerReference {
    pub kind: ResourceKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default)]
    pub controller: bool,
}

/// Equality-based label selector. An empty selector matches everything;
/// controllers that require a non-empty selector must validate that
/// themselves (the Deployment/ReplicaSet controllers do, from Phase 5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LabelSelector {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,
}

impl LabelSelector {
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.match_labels
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }

    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty()
    }
}

/// Every resource kind RustKube knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ResourceKind {
    Pod,
    ReplicaSet,
    Deployment,
    Service,
    Ingress,
    Node,
    Namespace,
    ConfigMap,
    Secret,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 9] = [
        ResourceKind::Pod,
        ResourceKind::ReplicaSet,
        ResourceKind::Deployment,
        ResourceKind::Service,
        ResourceKind::Ingress,
        ResourceKind::Node,
        ResourceKind::Namespace,
        ResourceKind::ConfigMap,
        ResourceKind::Secret,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Pod => "Pod",
            ResourceKind::ReplicaSet => "ReplicaSet",
            ResourceKind::Deployment => "Deployment",
            ResourceKind::Service => "Service",
            ResourceKind::Ingress => "Ingress",
            ResourceKind::Node => "Node",
            ResourceKind::Namespace => "Namespace",
            ResourceKind::ConfigMap => "ConfigMap",
            ResourceKind::Secret => "Secret",
        }
    }

    /// Lowercase plural, used in API paths (`/api/v1/pods`) and by the CLI.
    pub fn plural(self) -> &'static str {
        match self {
            ResourceKind::Pod => "pods",
            ResourceKind::ReplicaSet => "replicasets",
            ResourceKind::Deployment => "deployments",
            ResourceKind::Service => "services",
            ResourceKind::Ingress => "ingresses",
            ResourceKind::Node => "nodes",
            ResourceKind::Namespace => "namespaces",
            ResourceKind::ConfigMap => "configmaps",
            ResourceKind::Secret => "secrets",
        }
    }

    /// Nodes and Namespaces are cluster-scoped; everything else lives in a
    /// namespace.
    pub fn namespaced(self) -> bool {
        !matches!(self, ResourceKind::Node | ResourceKind::Namespace)
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown resource kind {0:?}")]
pub struct KindParseError(pub String);

impl ResourceKind {
    /// kubectl-style short aliases, so `rk-ctl get rs` and `scale deploy`
    /// work like the real thing.
    fn from_alias(lower: &str) -> Option<ResourceKind> {
        Some(match lower {
            "po" => ResourceKind::Pod,
            "rs" => ResourceKind::ReplicaSet,
            "deploy" => ResourceKind::Deployment,
            "svc" => ResourceKind::Service,
            "ing" => ResourceKind::Ingress,
            "no" => ResourceKind::Node,
            "ns" => ResourceKind::Namespace,
            "cm" => ResourceKind::ConfigMap,
            _ => return None,
        })
    }
}

impl FromStr for ResourceKind {
    type Err = KindParseError;

    /// Accepts the canonical kind (`Pod`), the plural (`pods`), kubectl short
    /// aliases (`rs`, `deploy`), and any casing — so `rk-ctl get pods`, `get
    /// rs` and YAML `kind: Pod` all resolve.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        ResourceKind::ALL
            .into_iter()
            .find(|kind| lower == kind.as_str().to_ascii_lowercase() || lower == kind.plural())
            .or_else(|| ResourceKind::from_alias(&lower))
            .ok_or_else(|| KindParseError(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_matches_subset_of_labels() {
        let selector = LabelSelector {
            match_labels: BTreeMap::from([("app".to_owned(), "web".to_owned())]),
        };
        let labels = BTreeMap::from([
            ("app".to_owned(), "web".to_owned()),
            ("tier".to_owned(), "frontend".to_owned()),
        ]);
        assert!(selector.matches(&labels));

        let other = BTreeMap::from([("app".to_owned(), "db".to_owned())]);
        assert!(!selector.matches(&other));
    }

    #[test]
    fn empty_selector_matches_everything() {
        let selector = LabelSelector::default();
        assert!(selector.matches(&BTreeMap::new()));
        assert!(selector.matches(&BTreeMap::from([("a".to_owned(), "b".to_owned())])));
    }

    #[test]
    fn kind_parses_singular_plural_and_case() {
        assert_eq!("Pod".parse::<ResourceKind>().unwrap(), ResourceKind::Pod);
        assert_eq!("pods".parse::<ResourceKind>().unwrap(), ResourceKind::Pod);
        assert_eq!(
            "REPLICASETS".parse::<ResourceKind>().unwrap(),
            ResourceKind::ReplicaSet
        );
        // kubectl short aliases resolve too.
        assert_eq!(
            "rs".parse::<ResourceKind>().unwrap(),
            ResourceKind::ReplicaSet
        );
        assert_eq!(
            "deploy".parse::<ResourceKind>().unwrap(),
            ResourceKind::Deployment
        );
        assert_eq!(
            "svc".parse::<ResourceKind>().unwrap(),
            ResourceKind::Service
        );
        assert!("Gizmo".parse::<ResourceKind>().is_err());
    }

    #[test]
    fn meta_round_trips_through_yaml() {
        let meta = ObjectMeta {
            name: "web".to_owned(),
            namespace: Some("default".to_owned()),
            labels: BTreeMap::from([("app".to_owned(), "web".to_owned())]),
            ..ObjectMeta::default()
        };
        let yaml = serde_yaml::to_string(&meta).unwrap();
        let back: ObjectMeta = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(meta, back);
    }
}
