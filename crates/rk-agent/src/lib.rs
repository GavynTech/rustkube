//! RustKube node agent (kubelet equivalent): registers its Node, heartbeats,
//! watches the API for pods bound to it and reconciles reality to match
//! through `rk-runtime`.
//!
//! Split for testability: [`reconcile`] is pure (plan + status derivation),
//! [`cache`] maintains the list+watch view of desired pods, [`node`] owns
//! registration/heartbeats, and the binary's main loop is the only impure
//! glue. Probes, logs and volumes arrive in Phase 7; the shared pod sandbox
//! (multi-container pods, pod-level networking) in Phase 6.

pub mod cache;
pub mod config;
pub mod names;
pub mod node;
pub mod reconcile;

pub use config::AgentConfig;
