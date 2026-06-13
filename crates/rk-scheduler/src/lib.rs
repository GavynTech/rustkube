//! RustKube scheduler: watches for pods with no `nodeName`, scores Ready
//! nodes by free CPU/memory (resource-aware bin packing), honors
//! `nodeSelector`, and writes the binding back through the API server.
//!
//! Split like the agent: [`schedule`] is the pure decision (filter + score),
//! [`cache`] is a generic list+watch reflector over the API, and the binary's
//! loop is the only impure glue. A pod with no node gets bound (`spec.nodeName`
//! set); a pod nothing fits gets `status.message` explaining why — and since
//! unscheduled pods have no owning agent, the scheduler can write that status
//! without fighting anyone.

pub mod cache;
pub mod config;
pub mod schedule;

pub use config::SchedulerConfig;
