//! The contract between `rkrt run` (which prepares everything) and the shim
//! process (which enters the namespaces and execs the workload), plus the
//! argv/env assembly rules shared with later phases.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Result, RuntimeError};

/// Everything the shim needs to take a prepared container the last mile.
/// Written by the parent as `containers/<name>/config.json`; read by the shim
/// after it has already been placed in its UTS/IPC/NET/PID namespaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSpec {
    pub name: String,
    pub id: String,
    /// Host path of the mounted overlay rootfs.
    pub rootfs: PathBuf,
    /// Host path of the container directory (shim writes pid1/exit-status here).
    pub container_dir: PathBuf,
    pub hostname: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
}

/// Parent-side record of a created container, `containers/<name>/state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerState {
    pub id: String,
    pub name: String,
    pub image: String,
    pub ip: String,
    pub host_ip: String,
    pub net_block: u32,
    pub shim_pid: i32,
    pub created_at: String,
}

/// OCI image config exec rules: ENTRYPOINT is fixed, CMD provides default
/// arguments, and a command given at run time replaces CMD (never ENTRYPOINT)
/// — exactly Docker's semantics.
pub fn assemble_argv(
    entrypoint: Option<&[String]>,
    cmd: Option<&[String]>,
    run_command: &[String],
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = entrypoint.unwrap_or_default().to_vec();
    if run_command.is_empty() {
        argv.extend(cmd.unwrap_or_default().iter().cloned());
    } else {
        argv.extend(run_command.iter().cloned());
    }
    if argv.is_empty() {
        return Err(RuntimeError::NoCommand);
    }
    Ok(argv)
}

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Merge image env with `--env` overrides. Later entries win per key, and a
/// PATH is guaranteed so binaries resolve even in images with bare configs.
pub fn assemble_env(image_env: Option<&[String]>, extra: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for entry in image_env.unwrap_or_default().iter().chain(extra) {
        let key = entry.split('=').next().unwrap_or(entry);
        merged.retain(|existing: &String| existing.split('=').next().unwrap_or(existing) != key);
        merged.push(entry.clone());
    }
    if !merged.iter().any(|e| e.starts_with("PATH=")) {
        merged.push(format!("PATH={DEFAULT_PATH}"));
    }
    merged
}

/// Container names become directory names, veth suffixes and hostnames, so
/// they stay deliberately strict: lowercase DNS-label-ish, max 40 chars.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(RuntimeError::BadName {
            name: name.to_owned(),
            reason: "use lowercase letters, digits and '-', start alphanumeric, max 40 chars",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn entrypoint_plus_cmd_concatenate() {
        let argv = assemble_argv(
            Some(&strings(&["/docker-entrypoint.sh"])),
            Some(&strings(&["nginx", "-g", "daemon off;"])),
            &[],
        )
        .unwrap();
        assert_eq!(
            argv,
            strings(&["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"])
        );
    }

    #[test]
    fn run_command_replaces_cmd_not_entrypoint() {
        let argv = assemble_argv(
            Some(&strings(&["/docker-entrypoint.sh"])),
            Some(&strings(&["nginx"])),
            &strings(&["nginx-debug", "-g", "daemon off;"]),
        )
        .unwrap();
        assert_eq!(
            argv,
            strings(&["/docker-entrypoint.sh", "nginx-debug", "-g", "daemon off;"])
        );

        let bare = assemble_argv(None, None, &strings(&["/bin/sh"])).unwrap();
        assert_eq!(bare, strings(&["/bin/sh"]));
    }

    #[test]
    fn empty_exec_is_an_error() {
        assert!(matches!(
            assemble_argv(None, None, &[]),
            Err(RuntimeError::NoCommand)
        ));
    }

    #[test]
    fn env_overrides_win_and_path_is_guaranteed() {
        let env = assemble_env(
            Some(&strings(&["PATH=/usr/sbin", "NGINX_VERSION=1.27"])),
            &strings(&["NGINX_VERSION=override", "EXTRA=1"]),
        );
        assert_eq!(
            env,
            strings(&["PATH=/usr/sbin", "NGINX_VERSION=override", "EXTRA=1"])
        );

        let defaulted = assemble_env(None, &strings(&["A=1"]));
        assert_eq!(defaulted.len(), 2);
        assert!(defaulted[1].starts_with("PATH=/usr/local/sbin:"));
    }

    #[test]
    fn names_are_strictly_validated() {
        for good in ["web", "web-1", "0db"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", "-web", "Web", "web_1", "web.1", &"x".repeat(41)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }
}
