//! RustKube control-plane API server.
//!
//! REST CRUD for every resource kind ([`resources`]), validation against the
//! typed model, optimistic concurrency via `metadata.resourceVersion`, and a
//! Server-Sent-Events watch stream ([`watch`]) — all over an [`rk_store`]
//! backend ([`rk_store::SqliteStore`] in practice). The API server is the
//! only component that touches the store; agents, controllers, the scheduler
//! and the CLI all go through HTTP.

pub mod auth;
pub mod config;
pub mod error;
pub mod resources;
pub mod server;
pub mod services;
pub mod watch;
