//! Liveness and readiness probes. The pure part is the state machine — when a
//! probe is due and what a result means (thresholds → ready/not-ready, or a
//! restart) — and is unit-tested; [`execute`] is the thin async I/O that runs
//! an HTTP GET, a TCP connect, or a command inside the pod.
//!
//! Readiness gates a pod's `ready` status (and therefore whether services
//! route to it); liveness failures restart the container.

use std::net::Ipv4Addr;
use std::time::Duration;

use rk_types::pod::Probe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    Liveness,
    Readiness,
}

/// Both probe states for one pod, owned by the agent across reconcile passes.
#[derive(Debug, Default)]
pub struct PodProbes {
    pub liveness: Option<ProbeState>,
    pub readiness: Option<ProbeState>,
}

/// Per-probe bookkeeping the agent owns across reconcile passes.
#[derive(Debug, Clone)]
pub struct ProbeState {
    pub consecutive_failures: u32,
    /// Readiness verdict (meaningless for liveness). A pod with a readiness
    /// probe starts not-ready until the first success.
    pub ready: bool,
    /// Monotonic seconds of the last run, for period pacing.
    pub last_run: Option<u64>,
}

impl ProbeState {
    /// Liveness assumes healthy until proven otherwise; readiness assumes
    /// not-ready until the first success.
    pub fn new(kind: ProbeKind) -> Self {
        ProbeState {
            consecutive_failures: 0,
            ready: kind == ProbeKind::Liveness,
            last_run: None,
        }
    }
}

/// Whether a probe should run now, given seconds since the container started
/// and the configured `initialDelaySeconds`/`periodSeconds`.
pub fn is_due(state: &ProbeState, now: u64, container_age: u64, probe: &Probe) -> bool {
    if container_age < u64::from(probe.initial_delay_seconds) {
        return false;
    }
    match state.last_run {
        None => true,
        Some(last) => now.saturating_sub(last) >= u64::from(probe.period_seconds.max(1)),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// `Some(new_ready)` when a readiness verdict flipped.
    pub ready_changed: Option<bool>,
    /// A liveness probe failed its threshold — restart the container.
    pub restart: bool,
}

/// Fold a probe result into the state and report what changed.
pub fn record(
    state: &mut ProbeState,
    kind: ProbeKind,
    success: bool,
    now: u64,
    probe: &Probe,
) -> ProbeOutcome {
    state.last_run = Some(now);
    let threshold = probe.failure_threshold.max(1);
    let mut outcome = ProbeOutcome::default();

    if success {
        state.consecutive_failures = 0;
        if kind == ProbeKind::Readiness && !state.ready {
            state.ready = true;
            outcome.ready_changed = Some(true);
        }
        return outcome;
    }

    state.consecutive_failures += 1;
    if state.consecutive_failures >= threshold {
        match kind {
            ProbeKind::Readiness => {
                if state.ready {
                    state.ready = false;
                    outcome.ready_changed = Some(false);
                }
            }
            ProbeKind::Liveness => {
                outcome.restart = true;
                state.consecutive_failures = 0; // armed again after the restart
            }
        }
    }
    outcome
}

/// Run a probe once. `pid1` is the container's PID 1, used to enter its
/// namespaces for `exec` probes. Any failure (connection refused, non-2xx/3xx,
/// non-zero exit, timeout) is a probe failure, not an error.
pub async fn execute(probe: &Probe, pod_ip: Ipv4Addr, pid1: i32) -> bool {
    let timeout = Duration::from_secs(u64::from(probe.timeout_seconds.max(1)));
    if let Some(http) = &probe.http_get {
        return http_probe(pod_ip, http.port, &http.path, timeout).await;
    }
    if let Some(tcp) = &probe.tcp_socket {
        return tcp_probe(pod_ip, tcp.port, timeout).await;
    }
    if let Some(exec) = &probe.exec {
        return exec_probe(pid1, &exec.command, timeout).await;
    }
    // A probe with no action configured can't fail anything.
    true
}

async fn tcp_probe(ip: Ipv4Addr, port: u16, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect((ip, port))).await,
        Ok(Ok(_))
    )
}

async fn http_probe(ip: Ipv4Addr, port: u16, path: &str, timeout: Duration) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect((ip, port)).await.ok()?;
        let request = format!("GET {path} HTTP/1.0\r\nHost: {ip}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.ok()?;
        // Parse "HTTP/1.x NNN": success on a 2xx/3xx status.
        let line = String::from_utf8_lossy(&buf[..n]);
        let code: u16 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some((200..400).contains(&code))
    };
    matches!(tokio::time::timeout(timeout, attempt).await, Ok(Some(true)))
}

async fn exec_probe(pid1: i32, command: &[String], timeout: Duration) -> bool {
    if command.is_empty() {
        return false;
    }
    // Enter the container's namespaces and run the command; exit 0 is success.
    let mut cmd = tokio::process::Command::new("nsenter");
    cmd.args(["-t", &pid1.to_string(), "-m", "-n", "-u", "-i", "-p", "--"]);
    cmd.args(command);
    let run = async {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .ok()
    };
    matches!(tokio::time::timeout(timeout, run).await, Ok(Some(status)) if status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(threshold: u32, period: u32, initial: u32) -> Probe {
        Probe {
            http_get: None,
            tcp_socket: None,
            exec: None,
            initial_delay_seconds: initial,
            period_seconds: period,
            timeout_seconds: 1,
            failure_threshold: threshold,
        }
    }

    #[test]
    fn due_respects_initial_delay_and_period() {
        let mut state = ProbeState::new(ProbeKind::Readiness);
        let p = probe(3, 10, 5);
        assert!(!is_due(&state, 100, 3, &p), "before initialDelay");
        assert!(is_due(&state, 100, 5, &p), "at initialDelay, never run");
        state.last_run = Some(100);
        assert!(!is_due(&state, 105, 30, &p), "within period");
        assert!(is_due(&state, 110, 30, &p), "period elapsed");
    }

    #[test]
    fn readiness_flips_on_first_success_and_after_threshold_failures() {
        let mut state = ProbeState::new(ProbeKind::Readiness);
        assert!(!state.ready, "starts not ready");

        let p = probe(3, 10, 0);
        let out = record(&mut state, ProbeKind::Readiness, true, 1, &p);
        assert_eq!(out.ready_changed, Some(true));
        assert!(state.ready);

        // Two failures: still ready (threshold is 3).
        record(&mut state, ProbeKind::Readiness, false, 11, &p);
        let out = record(&mut state, ProbeKind::Readiness, false, 21, &p);
        assert_eq!(out.ready_changed, None);
        assert!(state.ready);
        // Third failure: not ready.
        let out = record(&mut state, ProbeKind::Readiness, false, 31, &p);
        assert_eq!(out.ready_changed, Some(false));
        assert!(!state.ready);

        // Recovery on the next success.
        let out = record(&mut state, ProbeKind::Readiness, true, 41, &p);
        assert_eq!(out.ready_changed, Some(true));
    }

    #[test]
    fn liveness_restarts_after_consecutive_failures_then_rearms() {
        let mut state = ProbeState::new(ProbeKind::Liveness);
        let p = probe(2, 10, 0);
        assert!(!record(&mut state, ProbeKind::Liveness, false, 1, &p).restart);
        let out = record(&mut state, ProbeKind::Liveness, false, 11, &p);
        assert!(out.restart, "two failures hit the threshold");
        assert_eq!(
            state.consecutive_failures, 0,
            "re-armed for the new container"
        );

        // A success in between resets the count.
        record(&mut state, ProbeKind::Liveness, false, 21, &p);
        record(&mut state, ProbeKind::Liveness, true, 31, &p);
        assert!(!record(&mut state, ProbeKind::Liveness, false, 41, &p).restart);
    }
}
