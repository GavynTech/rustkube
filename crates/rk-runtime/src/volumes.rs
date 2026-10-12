//! Directory bind mounts. Mounts are installed only inside the workload's
//! private mount namespace. emptyDir lives outside the disposable rootfs.
use crate::{Result, RuntimeError, store::Store};
use nix::mount::{MsFlags, mount};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Source {
    HostPath(PathBuf),
    EmptyDir(String),
}
#[derive(Debug, Clone)]
pub struct Mount {
    pub source: Source,
    pub target: String,
    pub read_only: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: String,
    pub read_only: bool,
}
fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidVolume(message.into())
}

pub fn prepare(store: &Store, name: &str, mounts: &[Mount]) -> Result<Vec<BindMount>> {
    crate::spec::validate_name(name)?;
    let mut result = Vec::new();
    for m in mounts {
        rk_types::pod::validate_mount_path(&m.target).map_err(invalid)?;
        let source = match &m.source {
            Source::HostPath(path) => {
                rk_types::pod::validate_absolute_directory(&path.to_string_lossy())
                    .map_err(invalid)?;
                // Host directories must already exist: never create a misspelled
                // result location silently on a different node.
                fs::canonicalize(path)
                    .map_err(|e| RuntimeError::io("resolving hostPath", path, e))?
            }
            Source::EmptyDir(volume) => {
                if volume.is_empty()
                    || volume.len() > 63
                    || !volume
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                {
                    return Err(invalid("invalid emptyDir name"));
                }
                let path = store.root().join("volumes").join(name).join(volume);
                fs::create_dir_all(&path)
                    .map_err(|e| RuntimeError::io("creating emptyDir", &path, e))?;
                fs::canonicalize(&path)
                    .map_err(|e| RuntimeError::io("resolving emptyDir", path, e))?
            }
        };
        if !source.is_dir() {
            return Err(invalid("only directory volumes are supported"));
        }
        result.push(BindMount {
            source,
            target: m.target.clone(),
            read_only: m.read_only,
        });
    }
    Ok(result)
}

/// Refuse symlinks in image mount destinations, including intermediate
/// components. The image cannot redirect a mount outside its rootfs.
fn destination(root: &Path, target: &str) -> Result<PathBuf> {
    rk_types::pod::validate_mount_path(target).map_err(invalid)?;
    let mut path = root.to_path_buf();
    for component in target[1..].split('/') {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(invalid(format!(
                    "mount destination is not a real directory: {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)
                    .map_err(|e| RuntimeError::io("creating mount destination", &path, e))?;
            }
            Err(e) => return Err(RuntimeError::io("checking mount destination", &path, e)),
        }
    }
    Ok(path)
}

pub fn install(root: &Path, mounts: &[BindMount]) -> Result<()> {
    // Validate all paths before mounting: nested mounts could traverse a host
    // directory, so reject them even if the caller bypassed API admission.
    for (i, m) in mounts.iter().enumerate() {
        rk_types::pod::validate_mount_path(&m.target).map_err(invalid)?;
        for previous in &mounts[..i] {
            if Path::new(&m.target).starts_with(&previous.target)
                || Path::new(&previous.target).starts_with(&m.target)
            {
                return Err(invalid("overlapping volume mounts"));
            }
        }
    }
    for m in mounts {
        let target = destination(root, &m.target)?;
        mount(
            Some(m.source.as_path()),
            &target,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|source| RuntimeError::Mount {
            what: "volume bind",
            target: target.clone(),
            source,
        })?;
        if m.read_only {
            mount(
                None::<&str>,
                &target,
                None::<&str>,
                MsFlags::MS_BIND
                    | MsFlags::MS_REMOUNT
                    | MsFlags::MS_RDONLY
                    | MsFlags::MS_NOSUID
                    | MsFlags::MS_NODEV,
                None::<&str>,
            )
            .map_err(|source| RuntimeError::Mount {
                what: "read-only volume",
                target,
                source,
            })?;
        }
    }
    Ok(())
}

pub fn cleanup(store: &Store, name: &str) -> Result<()> {
    crate::spec::validate_name(name)?;
    let path = store.root().join("volumes").join(name);
    match fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RuntimeError::io("removing emptyDir", path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_symlink_destinations_and_traversal() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(destination(root.path(), "/escape/subdir").is_err());
        assert!(!outside.path().join("subdir").exists());
        assert!(destination(root.path(), "/../escape").is_err());
        assert!(
            destination(root.path(), "/results/nested")
                .unwrap()
                .is_dir()
        );
    }
    #[test]
    fn empty_dir_survives_container_recreation_and_cleanup_preserves_host_data() {
        let tmp = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let mounts = vec![
            Mount {
                source: Source::EmptyDir("scratch".into()),
                target: "/scratch".into(),
                read_only: false,
            },
            Mount {
                source: Source::HostPath(host.path().into()),
                target: "/results".into(),
                read_only: false,
            },
        ];
        let first = prepare(&store, "p123-sim", &mounts).unwrap();
        fs::write(first[0].source.join("checkpoint"), b"saved").unwrap();
        fs::write(host.path().join("result"), b"done").unwrap();
        let next = prepare(&store, "p123-sim", &mounts).unwrap();
        assert_eq!(
            fs::read(next[0].source.join("checkpoint")).unwrap(),
            b"saved"
        );
        cleanup(&store, "p123-sim").unwrap();
        assert!(!first[0].source.exists());
        assert_eq!(fs::read(host.path().join("result")).unwrap(), b"done");
    }
    #[test]
    fn missing_host_directory_fails_without_creating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let missing = tmp.path().join("missing");
        assert!(
            prepare(
                &store,
                "sim",
                &[Mount {
                    source: Source::HostPath(missing.clone()),
                    target: "/data".into(),
                    read_only: true
                }]
            )
            .is_err()
        );
        assert!(!missing.exists());
    }
}

#[cfg(test)]
mod privileged_tests {
    use super::*;
    /// Run on a Linux runtime host with CAP_SYS_ADMIN. All actual mounts live
    /// in a subprocess's private namespace; the parent owns the temp data.
    #[test]
    #[ignore = "requires Linux mount namespace privileges; run explicitly on the runtime VM"]
    fn bind_mounts_enforce_read_only_and_persist_results() {
        const CHILD: &str = "RK_VOLUME_TEST_DIR";
        if let Some(base) = std::env::var_os(CHILD) {
            let base = PathBuf::from(base);
            let root = base.join("root");
            install(
                &root,
                &[
                    BindMount {
                        source: base.join("host"),
                        target: "/results".into(),
                        read_only: false,
                    },
                    BindMount {
                        source: base.join("host"),
                        target: "/input".into(),
                        read_only: true,
                    },
                ],
            )
            .unwrap();
            fs::write(root.join("results/result"), b"saved").unwrap();
            assert_eq!(fs::read(root.join("input/result")).unwrap(), b"saved");
            assert_eq!(
                fs::write(root.join("input/forbidden"), b"no")
                    .unwrap_err()
                    .raw_os_error(),
                Some(30)
            );
            // Do not delete through the mounted paths. Process exit destroys
            // this namespace and the parent then removes the temporary tree.
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("host")).unwrap();
        fs::create_dir(temp.path().join("root")).unwrap();
        let status = std::process::Command::new("unshare")
            .args(["--mount", "--propagation", "private"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "volumes::privileged_tests::bind_mounts_enforce_read_only_and_persist_results",
                "--nocapture",
            ])
            .env(CHILD, temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read(temp.path().join("host/result")).unwrap(), b"saved");
        assert!(!temp.path().join("host/forbidden").exists());
        assert!(
            !temp.path().join("root/results/result").exists(),
            "mount did not leak to parent namespace"
        );
    }
}
