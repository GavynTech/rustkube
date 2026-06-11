//! RustKube control-plane API server.
//!
//! Phase 0 stands up the axum stack with `/healthz` and `/version`; Phase 2
//! adds the resource CRUD endpoints, validation, optimistic concurrency and
//! the watch stream on top of `rk-store`.

pub mod config;
pub mod server;
