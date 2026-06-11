//! On-disk layout for images and containers, rooted at `/var/lib/rustkube`
//! (override with `RKRT_ROOT` or `--root` for tests and host-side poking).
//!
//! ```text
//! <root>/
//!   images/
//!     blobs/sha256/<hex>          compressed layer + config blobs, as fetched
//!     manifests/<hex>.json        raw manifests, named by manifest digest
//!     index.json                  image reference -> ImageRecord
//!   layers/sha256/<hex>/          extracted layer trees (overlay lowerdirs)
//!   containers/<name>/            per-container state (spec, rootfs, logs)
//!   net/ipam.json                 /30 block allocator state
//! ```
//!
//! Blobs and extracted layers are content-addressed by their sha256 digest,
//! so layers shared between images are stored and extracted exactly once.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, RuntimeError};

/// Everything an image reference resolves to locally. The `layers` list keeps
/// manifest order: index 0 is the base layer (bottom of the overlay stack).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRecord {
    pub manifest_digest: String,
    pub config_digest: String,
    pub layers: Vec<String>,
    pub pulled_at: String,
}

pub type ImageIndex = BTreeMap<String, ImageRecord>;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    /// `RKRT_ROOT` overrides the default so tests and non-root host use never
    /// touch the real state directory.
    pub fn default_root() -> PathBuf {
        match std::env::var_os("RKRT_ROOT") {
            Some(root) => PathBuf::from(root),
            None => PathBuf::from("/var/lib/rustkube"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.blobs_dir(),
            self.manifests_dir(),
            self.layers_dir(),
            self.containers_dir(),
            self.net_dir(),
        ] {
            fs::create_dir_all(&dir)
                .map_err(|source| RuntimeError::io("creating store directory", &dir, source))?;
        }
        Ok(())
    }

    fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    fn blobs_dir(&self) -> PathBuf {
        self.images_dir().join("blobs").join("sha256")
    }

    fn manifests_dir(&self) -> PathBuf {
        self.images_dir().join("manifests")
    }

    fn layers_dir(&self) -> PathBuf {
        self.root.join("layers").join("sha256")
    }

    pub fn containers_dir(&self) -> PathBuf {
        self.root.join("containers")
    }

    pub fn container_dir(&self, name: &str) -> PathBuf {
        self.containers_dir().join(name)
    }

    pub fn net_dir(&self) -> PathBuf {
        self.root.join("net")
    }

    pub fn ipam_path(&self) -> PathBuf {
        self.net_dir().join("ipam.json")
    }

    fn index_path(&self) -> PathBuf {
        self.images_dir().join("index.json")
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.blobs_dir().join(digest_hex(digest)?))
    }

    pub fn manifest_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self
            .manifests_dir()
            .join(format!("{}.json", digest_hex(digest)?)))
    }

    pub fn layer_dir(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.layers_dir().join(digest_hex(digest)?))
    }

    pub fn has_blob(&self, digest: &str) -> Result<bool> {
        Ok(self.blob_path(digest)?.exists())
    }

    pub fn has_layer(&self, digest: &str) -> Result<bool> {
        Ok(self.layer_dir(digest)?.exists())
    }

    /// Blob writes go through a temp file + rename so a crash mid-write never
    /// leaves a half blob behind a valid content address.
    pub fn write_blob(&self, digest: &str, bytes: &[u8]) -> Result<()> {
        let final_path = self.blob_path(digest)?;
        let tmp_path = final_path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp_path, bytes)
            .map_err(|source| RuntimeError::io("writing blob", &tmp_path, source))?;
        fs::rename(&tmp_path, &final_path)
            .map_err(|source| RuntimeError::io("committing blob", &final_path, source))?;
        Ok(())
    }

    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        fs::read(&path).map_err(|source| RuntimeError::io("reading blob", &path, source))
    }

    pub fn read_index(&self) -> Result<ImageIndex> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(ImageIndex::new());
        }
        let raw = fs::read(&path)
            .map_err(|source| RuntimeError::io("reading image index", &path, source))?;
        serde_json::from_slice(&raw)
            .map_err(|source| RuntimeError::json("image index", &path, source))
    }

    pub fn write_index(&self, index: &ImageIndex) -> Result<()> {
        let path = self.index_path();
        let raw = serde_json::to_vec_pretty(index)
            .map_err(|source| RuntimeError::json("image index", &path, source))?;
        fs::write(&path, raw)
            .map_err(|source| RuntimeError::io("writing image index", &path, source))
    }

    pub fn image_record(&self, reference: &str) -> Result<Option<ImageRecord>> {
        Ok(self.read_index()?.get(reference).cloned())
    }
}

/// Validate a `sha256:<hex>` digest and return the hex part. Digests become
/// file names, so anything that is not pure lowercase hex is rejected before
/// it can traverse paths.
pub fn digest_hex(digest: &str) -> Result<&str> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or(RuntimeError::BadDigest {
            digest: digest.to_owned(),
            reason: "only sha256: digests are supported",
        })?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(RuntimeError::BadDigest {
            digest: digest.to_owned(),
            reason: "expected 64 lowercase hex characters",
        });
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn digest_validation_guards_paths() {
        assert_eq!(digest_hex(DIGEST).unwrap(), &DIGEST["sha256:".len()..]);
        for bad in [
            "md5:abcd",
            "sha256:short",
            "sha256:../../../../etc/passwd0123456789abcdef0123456789abcdef0123456789",
            "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert!(digest_hex(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn blob_round_trip_is_atomic_per_digest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();

        assert!(!store.has_blob(DIGEST).unwrap());
        store.write_blob(DIGEST, b"layer-bytes").unwrap();
        assert!(store.has_blob(DIGEST).unwrap());
        assert_eq!(store.read_blob(DIGEST).unwrap(), b"layer-bytes");
    }

    #[test]
    fn index_round_trips_and_defaults_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();

        assert!(store.read_index().unwrap().is_empty());
        let mut index = ImageIndex::new();
        index.insert(
            "docker.io/library/nginx:latest".to_owned(),
            ImageRecord {
                manifest_digest: DIGEST.to_owned(),
                config_digest: DIGEST.to_owned(),
                layers: vec![DIGEST.to_owned()],
                pulled_at: "2026-06-11T00:00:00Z".to_owned(),
            },
        );
        store.write_index(&index).unwrap();
        let record = store
            .image_record("docker.io/library/nginx:latest")
            .unwrap()
            .unwrap();
        assert_eq!(record.layers.len(), 1);
    }
}
