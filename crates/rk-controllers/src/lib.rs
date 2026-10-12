//! RustKube controller manager: the Deployment, ReplicaSet and Node
//! controllers plus owner-reference garbage collection, all driven by one
//! reconcile loop over a shared set of list+watch caches.
//!
//! Each controller's decision is a pure function — [`replicaset::reconcile`],
//! [`deployment::reconcile`], [`node::classify`], [`gc::orphans`] — so the
//! interesting logic is unit-tested without a cluster. The binary's loop is
//! the only impure part: it snapshots the caches, runs the pure planners in
//! dependency order (Deployment → ReplicaSet → Node → GC), and applies the
//! resulting ops through the shared API client.

pub mod config;
pub mod deployment;
pub mod gc;
pub mod ingress;
pub mod ingress_tls;
pub mod job;
pub mod node;
pub mod replicaset;
pub mod support;

pub use config::ControllersConfig;
