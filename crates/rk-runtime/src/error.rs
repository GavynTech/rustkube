//! The crate-wide error type. Every fallible operation returns
//! [`RuntimeError`]; `unwrap`/`expect` stay banned outside tests.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    ImageRef(#[from] crate::image::ImageRefError),

    #[error("registry operation failed for {reference}")]
    Registry {
        reference: String,
        #[source]
        source: oci_client::errors::OciDistributionError,
    },

    #[error("digest mismatch for {what}: manifest says {expected}, blob hashes to {actual}")]
    DigestMismatch {
        what: String,
        expected: String,
        actual: String,
    },

    #[error("malformed digest {digest:?}: {reason}")]
    BadDigest {
        digest: String,
        reason: &'static str,
    },

    #[error("unsupported layer media type {media_type:?} for {digest}")]
    UnsupportedLayer { media_type: String, digest: String },

    #[error("image {reference} is not in the local store (run `rkrt pull {reference}` first)")]
    ImageNotPulled { reference: String },

    #[error("image config supplies no entrypoint or cmd, and no command was given")]
    NoCommand,

    #[error("invalid container name {name:?}: {reason}")]
    BadName { name: String, reason: &'static str },

    #[error("invalid pod CIDR {cidr:?}: {reason}")]
    BadPodCidr { cidr: String, reason: &'static str },

    #[error("container {name:?} already exists")]
    AlreadyExists { name: String },

    #[error("container {name:?} not found")]
    NotFound { name: String },

    #[error("container {name:?} is still running (pid {pid}); stop it first or pass --force")]
    StillRunning { name: String, pid: i32 },

    #[error("container {name:?} never became ready: {detail}")]
    FailedToStart { name: String, detail: String },

    #[error("{context} ({path})")]
    Io {
        context: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {what} at {path}")]
    Json {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("cgroup operation failed at {path}: {detail}")]
    Cgroup { path: PathBuf, detail: String },

    #[error("network setup failed during {op}: {detail}")]
    Net { op: &'static str, detail: String },

    #[error("pod address pool exhausted ({0} /30 blocks in use)")]
    PoolExhausted(u32),

    #[error("mount of {what} at {target} failed")]
    Mount {
        what: &'static str,
        target: PathBuf,
        #[source]
        source: nix::Error,
    },

    #[error("failed to spawn the container shim")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("system call {call} failed")]
    Sys {
        call: &'static str,
        #[source]
        source: nix::Error,
    },

    #[error(
        "this operation drives namespaces, cgroups and mounts and must run as root; \
         re-run on the rustkube VM as root"
    )]
    NeedsRoot,
}

impl RuntimeError {
    pub(crate) fn io(
        context: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        RuntimeError::Io {
            context,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn json(
        what: &'static str,
        path: impl Into<PathBuf>,
        source: serde_json::Error,
    ) -> Self {
        RuntimeError::Json {
            what,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn sys(call: &'static str, source: nix::Error) -> Self {
        RuntimeError::Sys { call, source }
    }

    pub(crate) fn net(op: &'static str, detail: impl Into<String>) -> Self {
        RuntimeError::Net {
            op,
            detail: detail.into(),
        }
    }
}
