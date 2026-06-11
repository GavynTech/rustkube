//! Container lifecycle for RustKube — no Docker daemon anywhere.
//!
//! Phase 1 adds, on top of [`image`]: registry pulls (oci-client), digest
//! verification, content-addressed layer storage, overlayfs rootfs assembly,
//! and execution in fresh PID/UTS/mount/IPC/net namespaces with `pivot_root`
//! and cgroups v2 limits (nix).

pub mod image;

pub use image::{ImageRef, ImageRefError, Reference};
