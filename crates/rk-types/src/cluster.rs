//! Cluster-scoped resources: Node and Namespace.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Resource;
use crate::meta::{ObjectMeta, ResourceKind, api_version_v1};
use crate::quantity::ResourceList;

fn kind_node() -> String {
    "Node".to_owned()
}

fn kind_namespace() -> String {
    "Namespace".to_owned()
}

/// A machine (or simulated machine in a network namespace) that can run pods.
/// Created by `rk-agent` when it registers; heartbeats keep it Ready.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_node")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: NodeSpec,
    #[serde(default)]
    pub status: NodeStatus,
}

impl Resource for Node {
    const KIND: ResourceKind = ResourceKind::Node;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeSpec {
    /// The /24 this node allocates pod IPs from, assigned at registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_cidr: Option<String>,
    /// Set by `rk-ctl drain`: no new pods get scheduled here.
    pub unschedulable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeStatus {
    pub condition: NodeCondition,
    #[serde(skip_serializing_if = "ResourceList::is_empty")]
    pub capacity: ResourceList,
    /// Capacity minus the requests of pods already placed here; what the
    /// scheduler bin-packs against.
    #[serde(skip_serializing_if = "ResourceList::is_empty")]
    pub allocatable: ResourceList,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<IpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeCondition {
    #[default]
    Unknown,
    Ready,
    NotReady,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_namespace")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: NamespaceSpec,
    #[serde(default)]
    pub status: NamespaceStatus,
}

impl Resource for Namespace {
    const KIND: ResourceKind = ResourceKind::Namespace;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NamespaceSpec {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NamespaceStatus {
    pub phase: NamespacePhase,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamespacePhase {
    #[default]
    Active,
    Terminating,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantity::{Cpu, Memory};

    #[test]
    fn node_manifest_parses() {
        let node: Node = serde_yaml::from_str(
            r#"
apiVersion: v1
kind: Node
metadata:
  name: worker-1
  labels:
    disk: ssd
spec:
  podCidr: 10.42.1.0/24
status:
  condition: Ready
  capacity:
    cpu: "4"
    memory: 8Gi
  address: 192.168.1.20
"#,
        )
        .unwrap();
        assert_eq!(node.spec.pod_cidr.as_deref(), Some("10.42.1.0/24"));
        assert!(!node.spec.unschedulable);
        assert_eq!(node.status.condition, NodeCondition::Ready);
        assert_eq!(node.status.capacity.cpu, Some(Cpu::from_cores(4)));
        assert_eq!(node.status.capacity.memory, Some(Memory::from_gib(8)));
    }

    #[test]
    fn namespace_defaults_to_active() {
        let ns: Namespace = serde_yaml::from_str("metadata:\n  name: prod\n").unwrap();
        assert_eq!(ns.status.phase, NamespacePhase::Active);
    }
}
