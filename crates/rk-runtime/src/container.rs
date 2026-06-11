//! Container lifecycle orchestration: `run` prepares image, rootfs, cgroup
//! and network, then hands off to the shim (see [`crate::init`]); `stop`,
//! `remove`, `list` and `logs_path` manage what `run` left behind.

use std::fs;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sched::CloneFlags;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use oci_spec::image::ImageConfiguration;

use crate::error::{Result, RuntimeError};
use crate::image::ImageRef;
use crate::spec::{self, ContainerState, RuntimeSpec};
use crate::store::{ImageRecord, Store};
use crate::{cgroup, net, registry, rootfs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPolicy {
    /// Pull only when the image is not in the store.
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
pub struct RunOptions {
    pub name: String,
    pub image: String,
    pub limits: cgroup::Limits,
    pub env: Vec<String>,
    pub command: Vec<String>,
    pub pull: PullPolicy,
}

#[derive(Debug)]
pub struct RunSummary {
    pub name: String,
    pub id: String,
    pub ip: String,
    pub shim_pid: i32,
    pub pid1: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i32),
    /// The shim is gone but never recorded an exit — killed dead, host
    /// rebooted, or torn down externally.
    Unknown,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Running => write!(f, "Running"),
            Status::Exited(code) => write!(f, "Exited({code})"),
            Status::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug)]
pub struct ContainerRow {
    pub name: String,
    pub image: String,
    pub status: Status,
    pub ip: String,
    pub pid1: Option<i32>,
    pub created_at: String,
}

fn needs_root() -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        Err(RuntimeError::NeedsRoot)
    }
}

/// 12 hex chars from the kernel CSPRNG — the container id used in paths,
/// veth names and the cgroup directory.
fn random_id() -> Result<String> {
    let mut bytes = [0u8; 6];
    let path = "/dev/urandom";
    let mut file = fs::File::open(path)
        .map_err(|source| RuntimeError::io("opening /dev/urandom", path, source))?;
    std::io::Read::read_exact(&mut file, &mut bytes)
        .map_err(|source| RuntimeError::io("reading /dev/urandom", path, source))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn ensure_image(store: &Store, image: &ImageRef, policy: PullPolicy) -> Result<ImageRecord> {
    let key = image.to_string();
    let existing = store.image_record(&key)?;
    let record = match (policy, existing) {
        (PullPolicy::Never, None) => return Err(RuntimeError::ImageNotPulled { reference: key }),
        (PullPolicy::Never, Some(record)) | (PullPolicy::Auto, Some(record)) => record,
        (PullPolicy::Always, _) | (PullPolicy::Auto, None) => {
            registry::pull(store, image).await?;
            store
                .image_record(&key)?
                .ok_or(RuntimeError::ImageNotPulled { reference: key })?
        }
    };
    Ok(record)
}

pub async fn run(store: &Store, opts: RunOptions) -> Result<RunSummary> {
    needs_root()?;
    spec::validate_name(&opts.name)?;
    store.ensure_layout()?;

    let container_dir = store.container_dir(&opts.name);
    if container_dir.exists() {
        return Err(RuntimeError::AlreadyExists {
            name: opts.name.clone(),
        });
    }

    let image: ImageRef = opts.image.parse()?;
    let record = ensure_image(store, &image, opts.pull).await?;

    let config_blob = store.read_blob(&record.config_digest)?;
    let config_path = store.blob_path(&record.config_digest)?;
    let image_config: ImageConfiguration = serde_json::from_slice(&config_blob)
        .map_err(|source| RuntimeError::json("image config", config_path, source))?;
    let (entrypoint, cmd, image_env, working_dir) = match image_config.config() {
        Some(config) => (
            config.entrypoint().as_deref(),
            config.cmd().as_deref(),
            config.env().as_deref(),
            config.working_dir().clone().unwrap_or_default(),
        ),
        None => (None, None, None, String::new()),
    };
    let argv = spec::assemble_argv(entrypoint, cmd, &opts.command)?;
    let env = spec::assemble_env(image_env, &opts.env);

    let id = random_id()?;
    fs::create_dir_all(&container_dir)
        .map_err(|source| RuntimeError::io("creating container dir", &container_dir, source))?;

    // From here on, any failure must unwind what already exists.
    let result = start_prepared(
        store,
        &opts,
        &container_dir,
        &id,
        &record,
        argv,
        env,
        working_dir,
    )
    .await;
    if result.is_err() {
        teardown_best_effort(store, &opts.name, &container_dir);
    }
    result
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal seam for error unwinding"
)]
async fn start_prepared(
    store: &Store,
    opts: &RunOptions,
    container_dir: &Path,
    id: &str,
    record: &ImageRecord,
    argv: Vec<String>,
    env: Vec<String>,
    working_dir: String,
) -> Result<RunSummary> {
    let container_dir = container_dir.to_path_buf();
    let rootfs_path = rootfs::mount_rootfs(store, &container_dir, &record.layers)?;
    let pod_net = net::allocate(store)?;
    rootfs::write_etc_hosts(&rootfs_path, &opts.name, &pod_net.container_ip.to_string())?;

    let runtime_spec = RuntimeSpec {
        name: opts.name.clone(),
        id: id.to_owned(),
        rootfs: rootfs_path.clone(),
        container_dir: container_dir.clone(),
        hostname: opts.name.clone(),
        argv,
        env,
        cwd: working_dir,
    };
    let spec_path = container_dir.join("config.json");
    let spec_json = serde_json::to_vec_pretty(&runtime_spec)
        .map_err(|source| RuntimeError::json("runtime spec", &spec_path, source))?;
    fs::write(&spec_path, spec_json)
        .map_err(|source| RuntimeError::io("writing runtime spec", &spec_path, source))?;

    let log_path = container_dir.join("console.log");
    let log = fs::File::create(&log_path)
        .map_err(|source| RuntimeError::io("creating console log", &log_path, source))?;
    let log_stderr = log
        .try_clone()
        .map_err(|source| RuntimeError::io("cloning console log handle", &log_path, source))?;

    // The shim re-executes this same binary; pre_exec runs between fork and
    // exec in the child, where unshare drops it into fresh UTS/IPC/NET
    // namespaces and makes its future children members of a new PID
    // namespace. Stdin stays piped: it is the synchronization channel.
    let mut command = Command::new("/proc/self/exe");
    command
        .arg("shim")
        .arg(&spec_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_stderr));
    unsafe {
        command.pre_exec(|| {
            nix::sched::unshare(
                CloneFlags::CLONE_NEWUTS
                    | CloneFlags::CLONE_NEWIPC
                    | CloneFlags::CLONE_NEWNET
                    | CloneFlags::CLONE_NEWPID,
            )
            .map_err(std::io::Error::from)
        });
    }
    let mut child = command
        .spawn()
        .map_err(|source| RuntimeError::Spawn { source })?;
    let shim_pid = i32::try_from(child.id()).unwrap_or(i32::MAX);

    // Limits and wiring land while the shim blocks on the sync byte, so the
    // workload can never run outside its cgroup or before its network.
    let cgroup_dir = cgroup::create(id, opts.limits)?;
    cgroup::attach(&cgroup_dir, shim_pid)?;
    net::setup(shim_pid, id, pod_net).await?;

    let state = ContainerState {
        id: id.to_owned(),
        name: opts.name.clone(),
        image: opts.image.clone(),
        ip: pod_net.container_ip.to_string(),
        host_ip: pod_net.host_ip.to_string(),
        net_block: pod_net.block,
        shim_pid,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let state_path = container_dir.join("state.json");
    let state_json = serde_json::to_vec_pretty(&state)
        .map_err(|source| RuntimeError::json("container state", &state_path, source))?;
    fs::write(&state_path, state_json)
        .map_err(|source| RuntimeError::io("writing container state", &state_path, source))?;

    // Release the shim.
    let Some(mut stdin) = child.stdin.take() else {
        return Err(RuntimeError::FailedToStart {
            name: opts.name.clone(),
            detail: "shim stdin pipe vanished".to_owned(),
        });
    };
    stdin
        .write_all(&[1])
        .map_err(|source| RuntimeError::io("releasing shim", &spec_path, source))?;
    drop(stdin);

    // The shim now forks PID 1; wait briefly for evidence either way. The
    // shim is intentionally not waited on — it outlives this process as the
    // container's supervisor.
    let pid1_path = container_dir.join("pid1");
    let exit_path = container_dir.join("exit-status");
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid1 = loop {
        if let Ok(raw) = fs::read_to_string(&exit_path) {
            let tail = log_tail(&log_path, 5);
            return Err(RuntimeError::FailedToStart {
                name: opts.name.clone(),
                detail: format!(
                    "exited immediately with status {} — last log lines:\n{tail}",
                    raw.trim()
                ),
            });
        }
        if let Ok(raw) = fs::read_to_string(&pid1_path)
            && let Ok(pid) = raw.trim().parse::<i32>()
        {
            break pid;
        }
        if Instant::now() > deadline {
            return Err(RuntimeError::FailedToStart {
                name: opts.name.clone(),
                detail: "shim produced neither pid1 nor an exit status within 5s".to_owned(),
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    Ok(RunSummary {
        name: opts.name.clone(),
        id: id.to_owned(),
        ip: state.ip,
        shim_pid,
        pid1,
    })
}

fn log_tail(path: &Path, lines: usize) -> String {
    match fs::read_to_string(path) {
        Ok(content) => {
            let all: Vec<&str> = content.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(_) => String::from("(no log captured)"),
    }
}

fn read_state(store: &Store, name: &str) -> Result<ContainerState> {
    let path = store.container_dir(name).join("state.json");
    if !path.exists() {
        return Err(RuntimeError::NotFound {
            name: name.to_owned(),
        });
    }
    let raw = fs::read(&path)
        .map_err(|source| RuntimeError::io("reading container state", &path, source))?;
    serde_json::from_slice(&raw)
        .map_err(|source| RuntimeError::json("container state", &path, source))
}

fn pid_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn read_pid1(store: &Store, name: &str) -> Option<i32> {
    let raw = fs::read_to_string(store.container_dir(name).join("pid1")).ok()?;
    raw.trim().parse().ok()
}

pub fn status(store: &Store, name: &str) -> Result<Status> {
    let dir = store.container_dir(name);
    if let Ok(raw) = fs::read_to_string(dir.join("exit-status")) {
        return Ok(Status::Exited(raw.trim().parse().unwrap_or(-1)));
    }
    match read_pid1(store, name) {
        Some(pid) if pid_alive(pid) => Ok(Status::Running),
        _ => Ok(Status::Unknown),
    }
}

/// SIGTERM to PID 1, escalating to SIGKILL after `timeout`. Waits for the
/// shim to record the exit status so callers observe a settled container.
pub fn stop(store: &Store, name: &str, timeout: Duration) -> Result<Status> {
    needs_root()?;
    read_state(store, name)?;
    let Some(pid1) = read_pid1(store, name) else {
        return status(store, name);
    };
    if !pid_alive(pid1) {
        return status(store, name);
    }

    let target = Pid::from_raw(pid1);
    // ESRCH means it exited between the liveness check and the signal.
    match kill(target, Signal::SIGTERM) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(source) => return Err(RuntimeError::sys("kill(SIGTERM)", source)),
    }
    let deadline = Instant::now() + timeout;
    while pid_alive(pid1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if pid_alive(pid1) {
        tracing::warn!(name, pid1, "SIGTERM ignored, escalating to SIGKILL");
        match kill(target, Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(source) => return Err(RuntimeError::sys("kill(SIGKILL)", source)),
        }
    }

    // Give the shim a moment to reap and write the status file.
    let settle = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle {
        if store.container_dir(name).join("exit-status").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    status(store, name)
}

/// Remove a stopped container: unmount the overlay, drop the cgroup, return
/// the address block, delete the directory. `--force` stops it first.
pub fn remove(store: &Store, name: &str, force: bool) -> Result<()> {
    needs_root()?;
    let state = read_state(store, name)?;
    if status(store, name)? == Status::Running {
        if !force {
            return Err(RuntimeError::StillRunning {
                name: name.to_owned(),
                pid: read_pid1(store, name).unwrap_or_default(),
            });
        }
        stop(store, name, Duration::from_secs(2))?;
    }

    let container_dir = store.container_dir(name);
    rootfs::unmount_rootfs(&container_dir.join("rootfs"))?;
    cgroup::remove(&state.id)?;
    net::teardown(store, state.net_block)?;
    fs::remove_dir_all(&container_dir)
        .map_err(|source| RuntimeError::io("removing container dir", &container_dir, source))?;
    Ok(())
}

/// Failure unwinding for a partially created container. Every step tolerates
/// the others having never happened.
fn teardown_best_effort(store: &Store, name: &str, container_dir: &Path) {
    if let Some(pid1) = read_pid1(store, name) {
        let _ = kill(Pid::from_raw(pid1), Signal::SIGKILL);
    }
    if let Ok(state) = read_state(store, name) {
        if pid_alive(state.shim_pid) {
            let _ = kill(Pid::from_raw(state.shim_pid), Signal::SIGKILL);
        }
        let _ = cgroup::remove(&state.id);
        let _ = net::teardown(store, state.net_block);
    }
    let _ = rootfs::unmount_rootfs(&container_dir.join("rootfs"));
    let _ = fs::remove_dir_all(container_dir);
}

pub fn list(store: &Store) -> Result<Vec<ContainerRow>> {
    let dir = store.containers_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(&dir)
        .map_err(|source| RuntimeError::io("listing containers", &dir, source))?;
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RuntimeError::io("listing containers", &dir, source))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(state) = read_state(store, &name) else {
            continue;
        };
        rows.push(ContainerRow {
            status: status(store, &name)?,
            pid1: read_pid1(store, &name),
            name,
            image: state.image,
            ip: state.ip,
            created_at: state.created_at,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

pub fn logs_path(store: &Store, name: &str) -> Result<PathBuf> {
    let path = store.container_dir(name).join("console.log");
    if !path.exists() {
        return Err(RuntimeError::NotFound {
            name: name.to_owned(),
        });
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_are_12_lowercase_hex() {
        let id = random_id().unwrap();
        assert_eq!(id.len(), 12);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(random_id().unwrap(), id, "ids should not repeat");
    }

    #[test]
    fn listing_an_empty_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert!(list(&store).unwrap().is_empty());
        store.ensure_layout().unwrap();
        assert!(list(&store).unwrap().is_empty());
    }

    #[test]
    fn status_of_missing_pieces_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();
        fs::create_dir_all(store.container_dir("ghost")).unwrap();
        assert_eq!(status(&store, "ghost").unwrap(), Status::Unknown);

        fs::write(store.container_dir("ghost").join("exit-status"), "0").unwrap();
        assert_eq!(status(&store, "ghost").unwrap(), Status::Exited(0));
    }

    #[test]
    fn stop_and_remove_demand_an_existing_container() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();
        // Both root-gated operations check existence after the root check,
        // so as non-root we assert the root gate itself; under root (VM
        // integration) the NotFound path is covered by the e2e script.
        if nix::unistd::geteuid().is_root() {
            assert!(matches!(
                stop(&store, "nope", Duration::from_secs(1)),
                Err(RuntimeError::NotFound { .. })
            ));
        } else {
            assert!(matches!(
                stop(&store, "nope", Duration::from_secs(1)),
                Err(RuntimeError::NeedsRoot)
            ));
        }
    }
}
