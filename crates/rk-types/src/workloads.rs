//! ReplicaSet and Deployment: the workload controllers' resources.
//!
//! Ownership chain (reconciled from Phase 5): a Deployment owns ReplicaSets,
//! a ReplicaSet owns Pods stamped from its template.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Resource;
use crate::meta::{LabelSelector, ObjectMeta, ResourceKind, api_version_v1};
use crate::pod::PodSpec;

fn kind_replica_set() -> String {
    "ReplicaSet".to_owned()
}

fn kind_deployment() -> String {
    "Deployment".to_owned()
}

fn default_replicas() -> u32 {
    1
}

/// The pod stamp: labels/annotations plus a [`PodSpec`]. Pods created from it
/// get a generated name and an owner reference back to the ReplicaSet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PodTemplateSpec {
    pub metadata: TemplateMeta,
    pub spec: PodSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TemplateMeta {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaSet {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_replica_set")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: ReplicaSetSpec,
    #[serde(default)]
    pub status: ReplicaSetStatus,
}

impl Resource for ReplicaSet {
    const KIND: ResourceKind = ResourceKind::ReplicaSet;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplicaSetSpec {
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    pub selector: LabelSelector,
    pub template: PodTemplateSpec,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplicaSetStatus {
    pub replicas: u32,
    pub ready_replicas: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_deployment")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: DeploymentSpec,
    #[serde(default)]
    pub status: DeploymentStatus,
}

impl Resource for Deployment {
    const KIND: ResourceKind = ResourceKind::Deployment;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeploymentSpec {
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    pub selector: LabelSelector,
    pub template: PodTemplateSpec,
    pub strategy: DeploymentStrategy,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeploymentStrategy {
    #[serde(rename = "type")]
    pub strategy_type: DeploymentStrategyType,
    pub rolling_update: RollingUpdate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeploymentStrategyType {
    #[default]
    RollingUpdate,
    Recreate,
}

fn default_max_unavailable() -> u32 {
    1
}

fn default_max_surge() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollingUpdate {
    /// How many replicas may be down below `spec.replicas` during a rollout.
    #[serde(default = "default_max_unavailable")]
    pub max_unavailable: u32,
    /// How many replicas may exist above `spec.replicas` during a rollout.
    #[serde(default = "default_max_surge")]
    pub max_surge: u32,
}

impl Default for RollingUpdate {
    fn default() -> Self {
        RollingUpdate {
            max_unavailable: default_max_unavailable(),
            max_surge: default_max_surge(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeploymentStatus {
    pub replicas: u32,
    pub updated_replicas: u32,
    pub ready_replicas: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
apiVersion: v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 3
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
        - name: nginx
          image: nginx:1.27
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 1
"#;

    #[test]
    fn deployment_manifest_parses() {
        let deployment: Deployment = serde_yaml::from_str(MANIFEST).unwrap();
        assert_eq!(deployment.spec.replicas, 3);
        assert!(
            deployment
                .spec
                .selector
                .matches(&deployment.spec.template.metadata.labels),
            "selector must match its own template labels"
        );
        assert_eq!(deployment.spec.strategy.rolling_update.max_unavailable, 1);
        assert_eq!(
            deployment.spec.strategy.rolling_update.max_surge, 1,
            "default applies"
        );
    }

    #[test]
    fn replicas_default_to_one() {
        let rs: ReplicaSet = serde_yaml::from_str(
            "metadata:\n  name: rs\nspec:\n  selector:\n    matchLabels:\n      app: x\n",
        )
        .unwrap();
        assert_eq!(rs.spec.replicas, 1);
    }

    #[test]
    fn deployment_round_trips() {
        let deployment: Deployment = serde_yaml::from_str(MANIFEST).unwrap();
        let yaml = serde_yaml::to_string(&deployment).unwrap();
        let back: Deployment = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deployment, back);
    }
}
