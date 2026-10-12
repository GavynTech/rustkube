//! Container lifecycle for RustKube — no Docker daemon anywhere.
//!
//! Phase 1 turns this crate into a working single-node container runtime:
//!
//! - [`registry`] pulls images with `oci-client` and verifies every blob's
//!   sha256 against the manifest before it is stored;
//! - [`store`] keeps blobs and extracted layers content-addressed under
//!   `/var/lib/rustkube`, [`layers`] converts OCI whiteouts to overlayfs
//!   format during extraction;
//! - [`rootfs`] stacks layers with overlayfs;
//! - [`container`] orchestrates create/start/stop/remove, [`cgroup`] applies
//!   cgroups v2 CPU/memory limits, [`net`] wires a veth pair per container;
//! - [`init`] is the shim: fresh PID/UTS/mount/IPC/net namespaces,
//!   `pivot_root`, a minimal `/dev`, then `execve` of the image's command.
//!
//! The `rkrt` binary (this crate's `src/bin/rkrt.rs`) is the Phase 1 dev CLI
//! over these pieces; from Phase 3 the node agent drives them instead.

pub mod cgroup;
pub mod container;
pub mod error;
pub mod image;
pub mod init;
pub mod layers;
pub mod net;
pub mod registry;
pub mod rootfs;
pub mod spec;
pub mod store;
pub mod volumes;

pub use error::{Result, RuntimeError};
pub use image::{ImageRef, ImageRefError, Reference};
