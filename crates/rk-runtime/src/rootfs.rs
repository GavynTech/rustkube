//! Rootfs assembly: extracted layers stack read-only under an overlayfs
//! mount, with a per-container upper/work pair catching writes.

use std::fs;
use std::path::{Path, PathBuf};

use nix::mount::{MntFlags, MsFlags};

use crate::error::{Result, RuntimeError};
use crate::store::Store;

/// Overlayfs reads `lowerdir` left-to-right as top-to-bottom, while OCI
/// manifests list layers base-first — so the manifest order is reversed here.
/// Paths containing the option separators (`:`, `,`) are rejected outright;
/// store paths are digest-derived so this only guards exotic `--root` values.
pub fn lowerdir_option(layer_dirs: &[PathBuf]) -> Result<String> {
    let mut parts = Vec::with_capacity(layer_dirs.len());
    for dir in layer_dirs.iter().rev() {
        let text = dir
            .to_str()
            .filter(|t| !t.contains([':', ',']))
            .ok_or_else(|| {
                RuntimeError::io(
                    "layer path unusable in overlay options",
                    dir,
                    std::io::Error::other("path contains ':' or ',' or is not UTF-8"),
                )
            })?;
        parts.push(text);
    }
    Ok(parts.join(":"))
}

/// Mount the overlay rootfs for a container and return its path. The mount
/// lives in the host mount namespace; the container's PID 1 sees it through
/// its own namespace copy and pivots into it.
pub fn mount_rootfs(
    store: &Store,
    container_dir: &Path,
    layer_digests: &[String],
) -> Result<PathBuf> {
    let mut layer_dirs = Vec::with_capacity(layer_digests.len());
    for digest in layer_digests {
        let dir = store.layer_dir(digest)?;
        if !dir.exists() {
            return Err(RuntimeError::io(
                "extracted layer missing (re-pull the image)",
                &dir,
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ));
        }
        layer_dirs.push(dir);
    }

    let rootfs = container_dir.join("rootfs");
    let upper = container_dir.join("upper");
    let work = container_dir.join("work");
    for dir in [&rootfs, &upper, &work] {
        fs::create_dir_all(dir)
            .map_err(|source| RuntimeError::io("creating rootfs scaffolding", dir, source))?;
    }

    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lowerdir_option(&layer_dirs)?,
        upper.display(),
        work.display(),
    );
    nix::mount::mount(
        Some("overlay"),
        &rootfs,
        Some("overlay"),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .map_err(|source| RuntimeError::Mount {
        what: "overlay rootfs",
        target: rootfs.clone(),
        source,
    })?;
    Ok(rootfs)
}

/// Lazy-detach the overlay. Already-unmounted (or never-mounted) targets are
/// fine — teardown must be idempotent.
pub fn unmount_rootfs(rootfs: &Path) -> Result<()> {
    match nix::mount::umount2(rootfs, MntFlags::MNT_DETACH) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::EINVAL | nix::errno::Errno::ENOENT) => Ok(()),
        Err(source) => Err(RuntimeError::Mount {
            what: "overlay rootfs (unmount)",
            target: rootfs.to_owned(),
            source,
        }),
    }
}

/// Containers get a minimal `/etc/hosts` so the workload can resolve itself.
/// Written through the overlay (host side) before the workload starts.
pub fn write_etc_hosts(rootfs: &Path, name: &str, ip: &str) -> Result<()> {
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc)
        .map_err(|source| RuntimeError::io("creating /etc in rootfs", &etc, source))?;
    let hosts = etc.join("hosts");
    let content = format!("127.0.0.1\tlocalhost\n{ip}\t{name}\n");
    fs::write(&hosts, content)
        .map_err(|source| RuntimeError::io("writing /etc/hosts", &hosts, source))
}

/// Point the pod at the cluster DNS server (its node's gateway) and set the
/// Kubernetes search list, so `curl http://service/` resolves to a VIP.
/// `ndots:5` makes the resolver try the search domains for short names first.
pub fn write_resolv_conf(rootfs: &Path, nameserver: &str, namespace: &str) -> Result<()> {
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc)
        .map_err(|source| RuntimeError::io("creating /etc in rootfs", &etc, source))?;
    let resolv = etc.join("resolv.conf");
    let content = format!(
        "nameserver {nameserver}\n\
         search {namespace}.svc.cluster.local svc.cluster.local cluster.local\n\
         options ndots:5\n"
    );
    fs::write(&resolv, content)
        .map_err(|source| RuntimeError::io("writing /etc/resolv.conf", &resolv, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowerdir_reverses_manifest_order() {
        let dirs = vec![
            PathBuf::from("/var/lib/rustkube/layers/sha256/base"),
            PathBuf::from("/var/lib/rustkube/layers/sha256/middle"),
            PathBuf::from("/var/lib/rustkube/layers/sha256/top"),
        ];
        assert_eq!(
            lowerdir_option(&dirs).unwrap(),
            "/var/lib/rustkube/layers/sha256/top:/var/lib/rustkube/layers/sha256/middle:/var/lib/rustkube/layers/sha256/base"
        );
    }

    #[test]
    fn separator_laden_paths_are_rejected() {
        let dirs = vec![PathBuf::from("/tmp/evil:dir")];
        assert!(lowerdir_option(&dirs).is_err());
    }

    #[test]
    fn etc_hosts_lists_localhost_and_the_container() {
        let dir = tempfile::tempdir().unwrap();
        write_etc_hosts(dir.path(), "web", "10.42.0.6").unwrap();
        let content = fs::read_to_string(dir.path().join("etc/hosts")).unwrap();
        assert!(content.contains("127.0.0.1\tlocalhost"));
        assert!(content.contains("10.42.0.6\tweb"));
    }
}
