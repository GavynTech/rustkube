//! cgroups v2 resource limits. Containers live under
//! `/sys/fs/cgroup/rustkube/<id>/` with `cpu.max` and `memory.max` set from
//! the run options; the shim PID is attached before the workload execs, so
//! the whole container tree inherits the limits.

use std::fs;
use std::path::{Path, PathBuf};

use rk_types::{Cpu, Memory};

use crate::error::{Result, RuntimeError};

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PARENT: &str = "rustkube";
/// Standard CFS period; quota scales linearly with millicores.
const CPU_PERIOD_USEC: u64 = 100_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
}

/// `cpu.max` line for a millicore quantity: 1000m == one full period. The
/// kernel rejects quotas under 1ms, so tiny requests clamp up to that.
pub fn cpu_max_line(cpu: Cpu) -> String {
    let quota = (cpu.as_millis() * CPU_PERIOD_USEC / 1000).max(1_000);
    format!("{quota} {CPU_PERIOD_USEC}")
}

fn cg_write(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).map_err(|source| RuntimeError::Cgroup {
        path: path.to_owned(),
        detail: format!("writing {value:?}: {source}"),
    })
}

/// Enable the controllers we use for child groups of `at`. Controllers
/// already enabled are left alone; the root cgroup is exempt from the
/// no-internal-processes rule, so this is safe even though root has procs.
fn enable_controllers(at: &Path) -> Result<()> {
    let control = at.join("cgroup.subtree_control");
    let current = fs::read_to_string(&control).map_err(|source| RuntimeError::Cgroup {
        path: control.clone(),
        detail: format!("reading: {source}"),
    })?;
    for controller in ["cpu", "memory", "pids"] {
        if !current.split_whitespace().any(|c| c == controller) {
            cg_write(&control, &format!("+{controller}"))?;
        }
    }
    Ok(())
}

fn parent_dir(root: &str) -> PathBuf {
    Path::new(root).join(PARENT)
}

pub fn container_cgroup(id: &str) -> PathBuf {
    container_cgroup_at(CGROUP_ROOT, id)
}

fn container_cgroup_at(root: &str, id: &str) -> PathBuf {
    parent_dir(root).join(id)
}

/// Create `rustkube/<id>`, enabling controllers down the chain, and apply
/// limits. Returns the cgroup directory.
pub fn create(id: &str, limits: Limits) -> Result<PathBuf> {
    create_at(CGROUP_ROOT, id, limits)
}

fn create_at(root: &str, id: &str, limits: Limits) -> Result<PathBuf> {
    enable_controllers(Path::new(root))?;
    let parent = parent_dir(root);
    if !parent.exists() {
        fs::create_dir(&parent).map_err(|source| RuntimeError::Cgroup {
            path: parent.clone(),
            detail: format!("creating: {source}"),
        })?;
    }
    enable_controllers(&parent)?;

    let dir = container_cgroup_at(root, id);
    fs::create_dir(&dir).map_err(|source| RuntimeError::Cgroup {
        path: dir.clone(),
        detail: format!("creating: {source}"),
    })?;
    if let Some(cpu) = limits.cpu {
        cg_write(&dir.join("cpu.max"), &cpu_max_line(cpu))?;
    }
    if let Some(memory) = limits.memory {
        cg_write(&dir.join("memory.max"), &memory.as_bytes().to_string())?;
    }
    Ok(dir)
}

/// Move a process (and with it, all its future descendants) into the cgroup.
pub fn attach(dir: &Path, pid: i32) -> Result<()> {
    cg_write(&dir.join("cgroup.procs"), &pid.to_string())
}

/// Remove a container's cgroup. Processes may take a moment to exit after
/// SIGKILL, so EBUSY is retried briefly before giving up.
pub fn remove(id: &str) -> Result<()> {
    let dir = container_cgroup(id);
    if !dir.exists() {
        return Ok(());
    }
    for attempt in 0..20 {
        match fs::remove_dir(&dir) {
            Ok(()) => return Ok(()),
            Err(err) if err.raw_os_error() == Some(libc_ebusy()) && attempt < 19 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(source) => {
                return Err(RuntimeError::Cgroup {
                    path: dir,
                    detail: format!("removing: {source}"),
                });
            }
        }
    }
    Ok(())
}

const fn libc_ebusy() -> i32 {
    nix::errno::Errno::EBUSY as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_scales_millicores_over_the_period() {
        let cases = [
            ("500m", "50000 100000"),
            ("1", "100000 100000"),
            ("2", "200000 100000"),
            ("250m", "25000 100000"),
            // Below the kernel's 1ms floor: clamps up.
            ("5m", "1000 100000"),
        ];
        for (input, expected) in cases {
            let cpu: Cpu = input.parse().unwrap();
            assert_eq!(cpu_max_line(cpu), expected, "{input}");
        }
    }

    #[test]
    fn cgroup_paths_nest_under_rustkube() {
        assert_eq!(
            container_cgroup("abc123"),
            Path::new("/sys/fs/cgroup/rustkube/abc123")
        );
    }

    #[test]
    fn memory_limit_writes_plain_bytes() {
        let memory: Memory = "256Mi".parse().unwrap();
        assert_eq!(memory.as_bytes().to_string(), "268435456");
    }
}
