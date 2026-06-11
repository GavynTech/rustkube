//! Shared resource model for RustKube.
//!
//! Every resource follows the Kubernetes shape: an `apiVersion`/`kind`
//! envelope, [`meta::ObjectMeta`], a `spec` (desired state) and a `status`
//! (observed state). All types serialize to/from camelCase YAML so manifests
//! look like the Kubernetes manifests they imitate.
//!
//! This crate also hosts the small amount of runtime plumbing every RustKube
//! binary shares ([`bootstrap`]): tracing initialisation and config loading.
//! Keeping it here honors the fixed nine-crate workspace layout without
//! duplicating startup code six times.

pub mod apply;
pub mod bootstrap;
pub mod cluster;
pub mod data;
pub mod ingress;
pub mod meta;
pub mod pod;
pub mod quantity;
pub mod service;
pub mod watch;
pub mod workloads;

pub use apply::{AnyObject, ApplyError};
pub use cluster::{Namespace, Node, NodeCondition, NodeSpec, NodeStatus};
pub use data::{ConfigMap, Secret, SecretError};
pub use ingress::Ingress;
pub use meta::{LabelSelector, ObjectMeta, OwnerReference, ResourceKind};
pub use pod::{Container, Pod, PodPhase, PodSpec, PodStatus, Probe, RestartPolicy};
pub use quantity::{Cpu, Memory, QuantityError, ResourceList, Resources};
pub use service::{Service, ServiceType};
pub use watch::{EventType, List, WatchEvent};
pub use workloads::{Deployment, ReplicaSet};

/// Implemented by every top-level resource so the API server, store and CLI
/// can be written generically over resource types.
pub trait Resource:
    Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static
{
    const KIND: ResourceKind;

    fn metadata(&self) -> &ObjectMeta;
    fn metadata_mut(&mut self) -> &mut ObjectMeta;
}
