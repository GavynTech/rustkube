//! Layer extraction: compressed OCI tarballs become directory trees that
//! overlayfs can stack as lowerdirs.
//!
//! OCI layers encode deletions as AUFS-style whiteout entries; overlayfs uses
//! a different convention, so they are converted during extraction:
//!
//! - `.wh.<name>`      -> a 0:0 character device named `<name>` (overlay whiteout)
//! - `.wh..wh..opq`    -> `trusted.overlay.opaque=y` xattr on the directory
//!
//! Both conversions need root (mknod, trusted.* xattrs). Extracting as a
//! non-root user skips them with a warning — fine for host-side inspection,
//! never for actually running containers (that happens as root on the VM).

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, RuntimeError};
use crate::store::Store;

/// What a single tar entry means for the extracted tree.
#[derive(Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Everything in the directory below the layer must be hidden.
    OpaqueDir(PathBuf),
    /// The named path is deleted by this layer.
    Whiteout(PathBuf),
    Normal,
}

/// Classify a tar entry path by the AUFS whiteout convention.
pub fn classify_entry(path: &Path) -> EntryKind {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return EntryKind::Normal;
    };
    let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
    if name == ".wh..wh..opq" {
        return EntryKind::OpaqueDir(parent);
    }
    if let Some(target) = name.strip_prefix(".wh.") {
        return EntryKind::Whiteout(parent.join(target));
    }
    EntryKind::Normal
}

/// A relative tar path is safe when no component can escape the extraction
/// root. `tar`'s own `unpack_in` enforces this for normal entries; whiteout
/// conversions create paths ourselves, so they get the same check.
fn is_safe_relative(path: &Path) -> bool {
    path.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Extract `bytes` (a compressed or plain layer tarball) into the store's
/// layer directory for `digest`. Idempotent: an already-extracted layer is
/// left untouched. Extraction lands in a temp directory first and is renamed
/// into place, so a crash never leaves a half tree behind a valid digest.
pub fn extract_layer(store: &Store, digest: &str, media_type: &str, bytes: &[u8]) -> Result<()> {
    let final_dir = store.layer_dir(digest)?;
    if final_dir.exists() {
        return Ok(());
    }
    let tmp_dir = final_dir.with_extension(format!("tmp.{}", std::process::id()));
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)
            .map_err(|source| RuntimeError::io("clearing stale temp layer", &tmp_dir, source))?;
    }
    fs::create_dir_all(&tmp_dir)
        .map_err(|source| RuntimeError::io("creating temp layer dir", &tmp_dir, source))?;

    let reader: Box<dyn Read> = if media_type.ends_with("gzip") || media_type.ends_with("+gzip") {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else if media_type.ends_with("tar") {
        Box::new(bytes)
    } else {
        return Err(RuntimeError::UnsupportedLayer {
            media_type: media_type.to_owned(),
            digest: digest.to_owned(),
        });
    };

    unpack_tar(reader, &tmp_dir)?;

    fs::rename(&tmp_dir, &final_dir)
        .map_err(|source| RuntimeError::io("committing layer", &final_dir, source))?;
    tracing::debug!(digest, "layer extracted");
    Ok(())
}

fn unpack_tar(reader: impl Read, dest: &Path) -> Result<()> {
    let as_root = nix::unistd::geteuid().is_root();
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(as_root);
    archive.set_unpack_xattrs(as_root);

    let entries = archive
        .entries()
        .map_err(|source| RuntimeError::io("reading layer tar", dest, source))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|source| RuntimeError::io("reading layer tar entry", dest, source))?;
        let path = entry
            .path()
            .map_err(|source| RuntimeError::io("decoding tar entry path", dest, source))?
            .into_owned();

        match classify_entry(&path) {
            EntryKind::Normal => {
                entry
                    .unpack_in(dest)
                    .map_err(|source| RuntimeError::io("unpacking tar entry", dest, source))?;
            }
            EntryKind::OpaqueDir(dir) => {
                if !is_safe_relative(&dir) {
                    continue;
                }
                let target = dest.join(&dir);
                fs::create_dir_all(&target).map_err(|source| {
                    RuntimeError::io("creating opaque directory", &target, source)
                })?;
                if as_root {
                    xattr::set(&target, "trusted.overlay.opaque", b"y").map_err(|source| {
                        RuntimeError::io("marking opaque directory", &target, source)
                    })?;
                } else {
                    tracing::warn!(dir = %dir.display(), "skipping opaque marker (not root)");
                }
            }
            EntryKind::Whiteout(deleted) => {
                if !is_safe_relative(&deleted) {
                    continue;
                }
                let target = dest.join(&deleted);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|source| {
                        RuntimeError::io("creating whiteout parent", parent, source)
                    })?;
                }
                if as_root {
                    // Overlayfs marks deletions as 0:0 character devices.
                    nix::sys::stat::mknod(
                        &target,
                        nix::sys::stat::SFlag::S_IFCHR,
                        nix::sys::stat::Mode::empty(),
                        0,
                    )
                    .map_err(|source| RuntimeError::sys("mknod (whiteout)", source))?;
                } else {
                    tracing::warn!(path = %deleted.display(), "skipping whiteout (not root)");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real layer tars always carry uid/gid/mtime; hand-built headers must
    /// too, or extraction as root (which preserves ownership) chokes on the
    /// empty numeric fields.
    fn full_header(size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        header
    }

    #[test]
    fn whiteout_names_classify_correctly() {
        let cases: [(&str, EntryKind); 4] = [
            ("usr/bin/nginx", EntryKind::Normal),
            (
                "usr/lib/.wh.libfoo.so",
                EntryKind::Whiteout(PathBuf::from("usr/lib/libfoo.so")),
            ),
            (
                "etc/nginx/.wh..wh..opq",
                EntryKind::OpaqueDir(PathBuf::from("etc/nginx")),
            ),
            (
                ".wh.removed-at-root",
                EntryKind::Whiteout(PathBuf::from("removed-at-root")),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(classify_entry(Path::new(input)), expected, "{input}");
        }
    }

    #[test]
    fn escaping_paths_are_rejected() {
        assert!(is_safe_relative(Path::new("usr/bin")));
        assert!(is_safe_relative(Path::new("./etc")));
        assert!(!is_safe_relative(Path::new("../escape")));
        assert!(!is_safe_relative(Path::new("/absolute")));
        assert!(!is_safe_relative(Path::new("a/../../b")));
    }

    #[test]
    fn plain_tar_layer_extracts_files_and_dirs() {
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut full_header(5), "etc/motd", &b"hello"[..])
            .unwrap();
        let tarball = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        extract_layer(
            &store,
            digest,
            "application/vnd.oci.image.layer.v1.tar",
            &tarball,
        )
        .unwrap();

        let extracted = store.layer_dir(digest).unwrap().join("etc/motd");
        assert_eq!(fs::read(extracted).unwrap(), b"hello");

        // Re-extracting an existing layer is a no-op, not an error.
        extract_layer(
            &store,
            digest,
            "application/vnd.oci.image.layer.v1.tar",
            &tarball,
        )
        .unwrap();
    }

    #[test]
    fn gzip_layers_are_decompressed() {
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut full_header(2), "ok", &b"42"[..])
            .unwrap();
        let tarball = builder.into_inner().unwrap();

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tarball).unwrap();
        let gzipped = gz.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();
        let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        extract_layer(
            &store,
            digest,
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
            &gzipped,
        )
        .unwrap();
        assert_eq!(
            fs::read(store.layer_dir(digest).unwrap().join("ok")).unwrap(),
            b"42"
        );
    }

    #[test]
    fn unknown_media_types_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();
        let digest = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
        let result = extract_layer(
            &store,
            digest,
            "application/vnd.oci.image.layer.v1.tar+zstd",
            b"",
        );
        assert!(matches!(result, Err(RuntimeError::UnsupportedLayer { .. })));
    }
}
