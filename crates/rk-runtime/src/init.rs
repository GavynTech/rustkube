//! The container shim: the bridge between `rkrt run` and the workload.
//!
//! `rkrt run` spawns `/proc/self/exe shim <config.json>` with a `pre_exec`
//! hook that unshares UTS, IPC, NET and PID namespaces (PID membership
//! applies to *children* of the unshared process, so the shim itself stays
//! visible in the host PID namespace — that is what lets the parent attach
//! it to the cgroup and address its netns through `/proc/<pid>/ns/net`).
//!
//! The shim then:
//!  1. blocks reading one byte from stdin — the parent writes it only after
//!     the cgroup is attached and the veth is wired, so nothing the workload
//!     does can outrun its limits;
//!  2. forks: the child is PID 1 of the new PID namespace, unshares its own
//!     mount namespace, mounts /proc (correct now, inside the new PID ns),
//!     builds a minimal /dev, pivots into the overlay rootfs and execs the
//!     workload;
//!  3. waits for PID 1, records the exit status, and exits — a daemonless
//!     supervisor, one shim per container, parented to init after `rkrt run`
//!     returns.
//!
//! Everything here is synchronous and single-threaded on purpose: the shim
//! forks, and forking a process that has spawned runtime threads is how you
//! get deadlocks. `main` routes `rkrt shim` here before tokio exists.

use std::ffi::CString;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix::mount::MsFlags;
use nix::sys::stat::{Mode, SFlag, makedev, mknod};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, dup2_stdin, execve, fork, pivot_root, sethostname};

use crate::error::{Result, RuntimeError};
use crate::spec::RuntimeSpec;

/// Exit code PID 1 uses when container setup fails before exec. Picked out
/// of the way of common workload codes so failures read clearly in `ps`.
const SETUP_FAILED: i32 = 86;

/// Entry point for `rkrt shim <config.json>`. Never returns to clap or tokio.
pub fn shim_main() -> ExitCode {
    let Some(config_path) = std::env::args().nth(2) else {
        eprintln!("rkrt-shim: missing config path");
        return ExitCode::FAILURE;
    };
    match run_shim(Path::new(&config_path)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rkrt-shim: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_shim(config_path: &Path) -> Result<ExitCode> {
    let raw = fs::read(config_path)
        .map_err(|source| RuntimeError::io("reading runtime spec", config_path, source))?;
    let spec: RuntimeSpec = serde_json::from_slice(&raw)
        .map_err(|source| RuntimeError::json("runtime spec", config_path, source))?;

    // Hold until the parent finishes cgroup + network setup. EOF means the
    // parent died mid-setup; bail rather than start an unconfined workload.
    let mut byte = [0u8; 1];
    let got = std::io::stdin()
        .read(&mut byte)
        .map_err(|source| RuntimeError::io("reading sync byte", config_path, source))?;
    if got == 0 {
        return Err(RuntimeError::FailedToStart {
            name: spec.name.clone(),
            detail: "parent exited before releasing the shim".to_owned(),
        });
    }

    // SAFETY: the shim is single-threaded (no tokio here, no spawned
    // threads), so fork only has to be async-signal-safe for the window
    // until the child execs — and the child path uses plain syscalls.
    match unsafe { fork() }.map_err(|source| RuntimeError::sys("fork", source))? {
        ForkResult::Child => {
            // PID 1 of the new namespace. Any error before exec is reported
            // into the console log (stderr already points there).
            let err = container_init(&spec);
            eprintln!("rkrt-shim: container init failed: {err:#}");
            std::process::exit(SETUP_FAILED);
        }
        ForkResult::Parent { child } => {
            let pid1_path = spec.container_dir.join("pid1");
            fs::write(&pid1_path, child.as_raw().to_string())
                .map_err(|source| RuntimeError::io("writing pid1 file", &pid1_path, source))?;

            let status = loop {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => break code,
                    Ok(WaitStatus::Signaled(_, signal, _)) => break 128 + signal as i32,
                    Ok(_) => continue,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(source) => return Err(RuntimeError::sys("waitpid", source)),
                }
            };

            let exit_path = spec.container_dir.join("exit-status");
            fs::write(&exit_path, status.to_string())
                .map_err(|source| RuntimeError::io("writing exit status", &exit_path, source))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Everything PID 1 does between fork and exec. Only returns on failure.
fn container_init(spec: &RuntimeSpec) -> RuntimeError {
    match setup_and_exec(spec) {
        Ok(never) => match never {},
        Err(err) => err,
    }
}

/// The void type makes the signature say what exec means: success never
/// comes back.
enum Never {}

fn setup_and_exec(spec: &RuntimeSpec) -> Result<Never> {
    // A fresh mount namespace, private so the pivot and /proc, /dev mounts
    // never leak back into the host. The shim deliberately did NOT unshare
    // NEWNS — pivot_root rewrites the root of every process in the mount
    // namespace, and the shim must keep the host view to write state files.
    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS)
        .map_err(|source| RuntimeError::sys("unshare(CLONE_NEWNS)", source))?;
    mount_flags(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, "/")?;

    sethostname(&spec.hostname).map_err(|source| RuntimeError::sys("sethostname", source))?;

    // /proc must be mounted by a member of the new PID namespace — that is
    // this process — or the container would see host PIDs.
    let rootfs = &spec.rootfs;
    mount_fs("proc", &rootfs.join("proc"), "proc", MsFlags::empty(), None)?;
    mount_fs(
        "sysfs",
        &rootfs.join("sys"),
        "sysfs",
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None,
    )?;
    populate_dev(rootfs)?;
    crate::volumes::install(rootfs, &spec.mounts)?;

    // The pivot_root(".", ".") idiom: with new and old root the same mount,
    // the old root ends up stacked underneath and one lazy unmount drops it
    // — no scratch put_old directory inside the image needed.
    chdir(rootfs).map_err(|source| RuntimeError::sys("chdir(rootfs)", source))?;
    pivot_root(".", ".").map_err(|source| RuntimeError::sys("pivot_root", source))?;
    nix::mount::umount2(".", nix::mount::MntFlags::MNT_DETACH)
        .map_err(|source| RuntimeError::sys("umount2(old root)", source))?;
    chdir("/").map_err(|source| RuntimeError::sys("chdir(/)", source))?;

    // Stdin served as the sync pipe; the workload gets /dev/null instead.
    let devnull = fs::File::open("/dev/null")
        .map_err(|source| RuntimeError::io("opening /dev/null", "/dev/null", source))?;
    dup2_stdin(&devnull).map_err(|source| RuntimeError::sys("dup2(stdin)", source))?;

    let cwd = if spec.cwd.is_empty() { "/" } else { &spec.cwd };
    fs::create_dir_all(cwd)
        .map_err(|source| RuntimeError::io("creating working directory", cwd, source))?;
    chdir(Path::new(cwd)).map_err(|source| RuntimeError::sys("chdir(cwd)", source))?;

    let program = resolve_program(&spec.argv[0], &spec.env, Path::new("/"))?;
    let argv = to_cstrings(&spec.argv)?;
    let env = to_cstrings(&spec.env)?;
    let program_c = CString::new(program.into_os_string().into_encoded_bytes()).map_err(|_| {
        RuntimeError::FailedToStart {
            name: spec.name.clone(),
            detail: "program path contains a NUL byte".to_owned(),
        }
    })?;
    // Exec replaces this process; PID 1 of the namespace is now the workload.
    let err = execve(&program_c, &argv, &env)
        .map_err(|source| RuntimeError::sys("execve", source))
        .unwrap_err();
    Err(err)
}

fn mount_fs(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(target)
        .map_err(|src| RuntimeError::io("creating mount target", target, src))?;
    nix::mount::mount(Some(source), target, Some(fstype), flags, data).map_err(|source| {
        RuntimeError::Mount {
            what: "container filesystem",
            target: target.to_owned(),
            source,
        }
    })
}

fn mount_flags(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: MsFlags,
    what: &str,
) -> Result<()> {
    nix::mount::mount(source, target, fstype, flags, None::<&str>).map_err(|source| {
        RuntimeError::Mount {
            what: "mount propagation",
            target: PathBuf::from(what),
            source,
        }
    })
}

/// A minimal /dev: tmpfs with the standard character devices, pts, shm and
/// the /proc/self/fd symlinks. Real root, no user namespace, so mknod works.
/// nginx's official image logs through /dev/stdout — that symlink chain ends
/// at the console.log file the parent opened.
fn populate_dev(rootfs: &Path) -> Result<()> {
    let dev = rootfs.join("dev");
    mount_fs(
        "tmpfs",
        &dev,
        "tmpfs",
        MsFlags::MS_NOSUID,
        Some("mode=0755,size=65536k"),
    )?;

    let nodes: [(&str, u64, u64); 6] = [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ];
    for (name, major, minor) in nodes {
        let path = dev.join(name);
        mknod(
            &path,
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(0o666),
            makedev(major, minor),
        )
        .map_err(|source| RuntimeError::sys("mknod(/dev node)", source))?;
        // mknod honors umask; the standard nodes must be world-usable.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))
            .map_err(|source| RuntimeError::io("chmod /dev node", &path, source))?;
    }

    mount_fs(
        "devpts",
        &dev.join("pts"),
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )?;
    mount_fs(
        "tmpfs",
        &dev.join("shm"),
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777,size=65536k"),
    )?;

    let links = [
        ("pts/ptmx", "ptmx"),
        ("/proc/self/fd", "fd"),
        ("/proc/self/fd/0", "stdin"),
        ("/proc/self/fd/1", "stdout"),
        ("/proc/self/fd/2", "stderr"),
    ];
    for (target, name) in links {
        let path = dev.join(name);
        symlink(target, &path)
            .map_err(|source| RuntimeError::io("creating /dev symlink", &path, source))?;
    }
    Ok(())
}

/// `execve` does not search PATH, so resolution happens here, against the
/// container's own env. `root` parameterizes the lookup for tests; PID 1
/// passes `/` because it has already pivoted.
pub fn resolve_program(program: &str, env: &[String], root: &Path) -> Result<PathBuf> {
    let not_found = || RuntimeError::FailedToStart {
        name: program.to_owned(),
        detail: "executable not found in the container (checked PATH)".to_owned(),
    };

    if program.contains('/') {
        let candidate = root.join(program.trim_start_matches('/'));
        return if is_executable(&candidate) {
            Ok(PathBuf::from(program))
        } else {
            Err(not_found())
        };
    }

    let path_var = env
        .iter()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let candidate = root.join(dir.trim_start_matches('/')).join(program);
        if is_executable(&candidate) {
            return Ok(Path::new(dir).join(program));
        }
    }
    Err(not_found())
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn to_cstrings(items: &[String]) -> Result<Vec<CString>> {
    items
        .iter()
        .map(|item| {
            CString::new(item.as_bytes()).map_err(|_| RuntimeError::FailedToStart {
                name: item.clone(),
                detail: "argv/env entry contains a NUL byte".to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_executable(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn absolute_programs_check_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        touch_executable(dir.path(), "docker-entrypoint.sh");

        let resolved = resolve_program("/docker-entrypoint.sh", &[], dir.path()).unwrap();
        assert_eq!(resolved, PathBuf::from("/docker-entrypoint.sh"));

        assert!(resolve_program("/missing", &[], dir.path()).is_err());
    }

    #[test]
    fn bare_programs_resolve_through_the_env_path() {
        let dir = tempfile::tempdir().unwrap();
        touch_executable(dir.path(), "usr/sbin/nginx");

        let env = vec!["PATH=/usr/local/sbin:/usr/sbin".to_owned()];
        let resolved = resolve_program("nginx", &env, dir.path()).unwrap();
        assert_eq!(resolved, PathBuf::from("/usr/sbin/nginx"));

        let empty_env: Vec<String> = Vec::new();
        let fallback = resolve_program("nginx", &empty_env, dir.path()).unwrap();
        assert_eq!(fallback, PathBuf::from("/usr/sbin/nginx"));

        assert!(resolve_program("apachectl", &env, dir.path()).is_err());
    }

    #[test]
    fn non_executable_files_do_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usr/bin/data");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not a program").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let env = vec!["PATH=/usr/bin".to_owned()];
        assert!(resolve_program("data", &env, dir.path()).is_err());
    }

    #[test]
    fn nul_bytes_are_rejected_not_truncated() {
        assert!(to_cstrings(&["ok".to_owned()]).is_ok());
        assert!(to_cstrings(&["bad\0byte".to_owned()]).is_err());
    }
}
