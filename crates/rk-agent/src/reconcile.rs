//! The reconcile loop's brain, kept pure for testability: given the desired
//! pods, the containers actually on the node, and per-container restart
//! trackers, decide what to do ([`plan`]) and what each pod's status is
//! ([`pod_status`]). The impure shell in `main.rs` feeds it views and
//! executes the [`Op`]s through `rk-runtime`.

use std::collections::{BTreeMap, HashMap};

use rk_runtime::cgroup::Limits;
use rk_runtime::container::Status as RuntimeStatus;
use rk_types::pod::{
    ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    ContainerStatus, EnvVar, Pod, PodPhase, PodStatus,
};
use rk_types::{ConfigMap, RestartPolicy, Secret};

use crate::names;

/// Restart backoff: 1s doubling to a 60s cap.
const BACKOFF_BASE_SECS: u64 = 1;
const BACKOFF_CAP_SECS: u64 = 60;
/// A container that has run this long gets its backoff history forgiven.
const STABLE_RUN_SECS: i64 = 60;

/// What the agent wants to exist for one pod — or why it can't.
#[derive(Debug)]
pub struct DesiredPod {
    pub uid: String,
    pub uid12: String,
    pub namespace: String,
    pub name: String,
    pub restart_policy: RestartPolicy,
    /// `Err` carries the reason the pod is unrunnable (bad spec, unresolvable
    /// config); it surfaces as a Failed/Pending status, never a start.
    pub plan: Result<ContainerToRun, String>,
    /// Non-fatal notes appended to the status message (e.g. ignored probes).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ContainerToRun {
    pub runtime_name: String,
    pub container_name: String,
    pub image: String,
    pub limits: Limits,
    pub env: Vec<String>,
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    pub liveness_probe: Option<rk_types::pod::Probe>,
    pub readiness_probe: Option<rk_types::pod::Probe>,
}

/// One container as the runtime reports it.
#[derive(Debug, Clone)]
pub struct ActualContainer {
    pub runtime_name: String,
    pub uid12: String,
    pub image: String,
    pub status: RuntimeStatus,
    pub ip: Option<String>,
    pub pid1: Option<i32>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Per-runtime-container restart bookkeeping, owned by the agent loop.
#[derive(Debug, Default, Clone)]
pub struct Tracker {
    pub restarts: u32,
    /// Unix seconds before which no restart attempt is made.
    pub not_before: u64,
    /// `created_at` of the run the tracker last credited as stable.
    pub credited_run: Option<chrono::DateTime<chrono::Utc>>,
}

pub type Trackers = HashMap<String, Tracker>;

#[derive(Debug, PartialEq)]
pub enum Op {
    /// Start the container (it does not exist on the node).
    Start { uid: String },
    /// Remove the exited container and start it again (restart policy).
    Restart { uid: String, runtime_name: String },
    /// Stop and remove a container no pod wants anymore.
    Teardown { runtime_name: String },
}

pub fn backoff_delay_secs(restarts: u32) -> u64 {
    BACKOFF_BASE_SECS
        .saturating_mul(1_u64 << restarts.min(6))
        .min(BACKOFF_CAP_SECS)
}

fn should_restart(policy: RestartPolicy, exit: &RuntimeStatus) -> bool {
    match (policy, exit) {
        (RestartPolicy::Always, _) => true,
        (RestartPolicy::OnFailure, RuntimeStatus::Exited(0)) => false,
        (RestartPolicy::OnFailure, _) => true,
        (RestartPolicy::Never, _) => false,
    }
}

/// Decide the node's next moves. Pure: trackers are read here and mutated by
/// the caller via [`note_started`]/[`note_observed`] as ops execute.
pub fn plan(
    desired: &[DesiredPod],
    actual: &[ActualContainer],
    trackers: &Trackers,
    now_epoch: u64,
) -> Vec<Op> {
    let mut ops = Vec::new();
    let by_name: HashMap<&str, &ActualContainer> = actual
        .iter()
        .map(|c| (c.runtime_name.as_str(), c))
        .collect();

    let mut wanted_names: HashMap<&str, ()> = HashMap::new();
    for pod in desired {
        let Ok(run) = &pod.plan else {
            continue; // unrunnable: status-only, and any old container is
            // torn down by the orphan sweep below
        };
        wanted_names.insert(run.runtime_name.as_str(), ());
        let gate_open = trackers
            .get(&run.runtime_name)
            .is_none_or(|t| now_epoch >= t.not_before);

        match by_name.get(run.runtime_name.as_str()) {
            None if gate_open => ops.push(Op::Start {
                uid: pod.uid.clone(),
            }),
            None => {} // backing off
            Some(existing) => match &existing.status {
                RuntimeStatus::Running => {
                    // Spec drift (image edits) is reconciled by recreation —
                    // our API allows pod edits Kubernetes would reject.
                    if existing.image != run.image && gate_open {
                        ops.push(Op::Restart {
                            uid: pod.uid.clone(),
                            runtime_name: run.runtime_name.clone(),
                        });
                    }
                }
                status @ (RuntimeStatus::Exited(_) | RuntimeStatus::Unknown) => {
                    if should_restart(pod.restart_policy, status) && gate_open {
                        ops.push(Op::Restart {
                            uid: pod.uid.clone(),
                            runtime_name: run.runtime_name.clone(),
                        });
                    }
                    // Terminal exits keep their container around (state for
                    // status, logs in Phase 7) until the pod is deleted.
                }
            },
        }
    }

    // Orphans: on-node containers no desired pod accounts for.
    for container in actual {
        if !wanted_names.contains_key(container.runtime_name.as_str()) {
            ops.push(Op::Teardown {
                runtime_name: container.runtime_name.clone(),
            });
        }
    }
    ops
}

/// Record a (re)start attempt: bump the count and arm the backoff gate.
pub fn note_started(trackers: &mut Trackers, runtime_name: &str, now_epoch: u64) {
    let tracker = trackers.entry(runtime_name.to_owned()).or_default();
    tracker.restarts += 1;
    tracker.not_before = now_epoch + backoff_delay_secs(tracker.restarts.saturating_sub(1));
}

/// Observe steady state: a container running longer than [`STABLE_RUN_SECS`]
/// forgives its backoff history. Trackers are kept while their container is
/// on the node *or still desired* — a start that failed outright leaves no
/// container behind, and dropping its tracker would reset the backoff every
/// pass. Only trackers nobody references anymore are pruned.
pub fn note_observed(
    trackers: &mut Trackers,
    desired: &[DesiredPod],
    actual: &[ActualContainer],
    now: chrono::DateTime<chrono::Utc>,
) {
    for container in actual {
        if let RuntimeStatus::Running = container.status
            && let Some(created) = container.created_at
            && (now - created).num_seconds() > STABLE_RUN_SECS
            && let Some(tracker) = trackers.get_mut(&container.runtime_name)
            && tracker.credited_run != Some(created)
        {
            tracker.restarts = 0;
            tracker.not_before = 0;
            tracker.credited_run = Some(created);
        }
    }
    let mut referenced: HashMap<&str, ()> = actual
        .iter()
        .map(|c| (c.runtime_name.as_str(), ()))
        .collect();
    for pod in desired {
        if let Ok(run) = &pod.plan {
            referenced.insert(run.runtime_name.as_str(), ());
        }
    }
    trackers.retain(|name, _| referenced.contains_key(name.as_str()));
}

/// Build one pod's [`PodStatus`] from what the node observes.
pub fn pod_status(
    pod: &DesiredPod,
    actual: Option<&ActualContainer>,
    tracker: Option<&Tracker>,
    problem: Option<&str>,
    readiness: Option<bool>,
) -> PodStatus {
    let mut message_parts: Vec<String> = pod.notes.clone();
    let restart_count = tracker.map_or(0, |t| t.restarts.saturating_sub(1));

    let run = match &pod.plan {
        Err(reason) => {
            return PodStatus {
                phase: PodPhase::Failed,
                pod_ip: None,
                message: Some(reason.clone()),
                container_statuses: Vec::new(),
            };
        }
        Ok(run) => run,
    };

    let (phase, pod_ip, state, ready) = match actual.map(|a| (&a.status, a)) {
        None => {
            let reason = problem.unwrap_or("ContainerCreating").to_owned();
            if let Some(problem) = problem {
                message_parts.push(problem.to_owned());
            }
            (
                PodPhase::Pending,
                None,
                ContainerState {
                    waiting: Some(ContainerStateWaiting { reason }),
                    ..ContainerState::default()
                },
                false,
            )
        }
        Some((RuntimeStatus::Running, actual)) => (
            PodPhase::Running,
            actual.ip.as_deref().and_then(|ip| ip.parse().ok()),
            ContainerState {
                running: Some(ContainerStateRunning {
                    started_at: actual.created_at,
                }),
                ..ContainerState::default()
            },
            // A readiness probe's verdict gates readiness; without one, a
            // running container is ready.
            readiness.unwrap_or(true),
        ),
        Some((status @ (RuntimeStatus::Exited(_) | RuntimeStatus::Unknown), actual)) => {
            let exit_code = match status {
                RuntimeStatus::Exited(code) => *code,
                _ => -1,
            };
            let restarting = should_restart(pod.restart_policy, status);
            let phase = if restarting {
                // Kubernetes keeps the pod Running while it crash-loops; the
                // container state below says CrashLoopBackOff.
                PodPhase::Running
            } else if exit_code == 0 {
                PodPhase::Succeeded
            } else {
                PodPhase::Failed
            };
            let state = if restarting {
                ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: "CrashLoopBackOff".to_owned(),
                    }),
                    ..ContainerState::default()
                }
            } else {
                ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code,
                        finished_at: None,
                    }),
                    ..ContainerState::default()
                }
            };
            (
                phase,
                actual.ip.as_deref().and_then(|ip| ip.parse().ok()),
                state,
                false,
            )
        }
    };

    PodStatus {
        phase,
        pod_ip,
        message: (!message_parts.is_empty()).then(|| message_parts.join("; ")),
        container_statuses: vec![ContainerStatus {
            name: run.container_name.clone(),
            ready,
            restart_count,
            state,
        }],
    }
}

/// Validate a pod into a runnable plan. Resolution of `valueFrom` env uses
/// the prefetched maps (name -> object, same namespace as the pod).
pub fn desire(
    pod: &Pod,
    config_maps: &BTreeMap<String, ConfigMap>,
    secrets: &BTreeMap<String, Secret>,
) -> Option<DesiredPod> {
    let uid = pod.metadata.uid.clone()?;
    let uid12 = names::uid12(&uid);
    let namespace = pod.metadata.namespace_or_default().to_owned();
    let name = pod.metadata.name.clone();
    let notes = Vec::new();

    let plan = (|| -> Result<ContainerToRun, String> {
        let [container] = pod.spec.containers.as_slice() else {
            return Err(format!(
                "unsupported: {} containers — multi-container pods need the shared pod sandbox (Phase 6)",
                pod.spec.containers.len()
            ));
        };
        if container.name.len() > names::MAX_CONTAINER_NAME {
            return Err(format!(
                "container name {:?} exceeds {} characters",
                container.name,
                names::MAX_CONTAINER_NAME
            ));
        }
        if !pod.spec.volumes.is_empty() || !container.volume_mounts.is_empty() {
            return Err("unsupported: volumes arrive later".to_owned());
        }

        Ok(ContainerToRun {
            runtime_name: names::runtime_name(&uid, &container.name),
            container_name: container.name.clone(),
            image: container.image.clone(),
            limits: Limits {
                cpu: container.resources.limits.cpu,
                memory: container.resources.limits.memory,
            },
            env: resolve_env(&container.env, config_maps, secrets)?,
            command: container.command.clone(),
            args: container.args.clone(),
            liveness_probe: container.liveness_probe.clone(),
            readiness_probe: container.readiness_probe.clone(),
        })
    })();

    Some(DesiredPod {
        uid,
        uid12,
        namespace,
        name,
        restart_policy: pod.spec.restart_policy,
        plan,
        notes,
    })
}

/// Flatten pod env to `KEY=value` pairs, resolving `valueFrom` references
/// against the prefetched ConfigMaps/Secrets of the pod's namespace.
pub fn resolve_env(
    env: &[EnvVar],
    config_maps: &BTreeMap<String, ConfigMap>,
    secrets: &BTreeMap<String, Secret>,
) -> Result<Vec<String>, String> {
    let mut resolved = Vec::with_capacity(env.len());
    for var in env {
        let value = match (&var.value, &var.value_from) {
            (Some(value), _) => value.clone(),
            (None, Some(source)) => match (&source.config_map_key_ref, &source.secret_key_ref) {
                (Some(key_ref), None) => config_maps
                    .get(&key_ref.name)
                    .ok_or_else(|| format!("configmap {:?} not found", key_ref.name))?
                    .data
                    .get(&key_ref.key)
                    .ok_or_else(|| {
                        format!("configmap {:?} has no key {:?}", key_ref.name, key_ref.key)
                    })?
                    .clone(),
                (None, Some(key_ref)) => {
                    let secret = secrets
                        .get(&key_ref.name)
                        .ok_or_else(|| format!("secret {:?} not found", key_ref.name))?;
                    let bytes = secret
                        .decode(&key_ref.key)
                        .map_err(|err| format!("secret {:?}: {err}", key_ref.name))?;
                    String::from_utf8(bytes).map_err(|_| {
                        format!(
                            "secret {:?} key {:?} is not UTF-8",
                            key_ref.name, key_ref.key
                        )
                    })?
                }
                _ => {
                    return Err(format!(
                        "env {:?}: valueFrom needs exactly one of configMapKeyRef or secretKeyRef",
                        var.name
                    ));
                }
            },
            (None, None) => String::new(),
        };
        resolved.push(format!("{}={value}", var.name));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use rk_types::Container;
    use rk_types::pod::PodSpec;
    use rk_types::quantity::{ResourceList, Resources};

    use super::*;

    fn simple_pod(uid: &str, image: &str) -> Pod {
        Pod {
            api_version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            metadata: rk_types::ObjectMeta {
                uid: Some(uid.to_owned()),
                ..rk_types::ObjectMeta::named("web")
            },
            spec: PodSpec {
                containers: vec![Container {
                    name: "nginx".to_owned(),
                    image: image.to_owned(),
                    command: None,
                    args: None,
                    env: Vec::new(),
                    ports: Vec::new(),
                    resources: Resources::default(),
                    volume_mounts: Vec::new(),
                    liveness_probe: None,
                    readiness_probe: None,
                }],
                ..PodSpec::default()
            },
            status: Default::default(),
        }
    }

    fn desired(uid: &str, image: &str) -> DesiredPod {
        desire(&simple_pod(uid, image), &BTreeMap::new(), &BTreeMap::new()).unwrap()
    }

    fn running(name: &str, uid12: &str, image: &str) -> ActualContainer {
        ActualContainer {
            runtime_name: name.to_owned(),
            uid12: uid12.to_owned(),
            image: image.to_owned(),
            status: RuntimeStatus::Running,
            ip: Some("10.42.0.6".to_owned()),
            pid1: Some(1234),
            created_at: Some(chrono::Utc::now()),
        }
    }

    const UID: &str = "4f9d2c81aa55b2e7c01d9f33ab12cd34";
    const RT_NAME: &str = "p4f9d2c81aa55-nginx";

    #[test]
    fn missing_container_starts_and_running_container_rests() {
        let desired = [desired(UID, "nginx")];
        let ops = plan(&desired, &[], &Trackers::new(), 100);
        assert_eq!(
            ops,
            vec![Op::Start {
                uid: UID.to_owned()
            }]
        );

        let actual = [running(RT_NAME, "4f9d2c81aa55", "nginx")];
        assert!(plan(&desired, &actual, &Trackers::new(), 100).is_empty());
    }

    #[test]
    fn orphans_are_torn_down_and_foreign_uids_count_as_orphans() {
        let actual = [running("p999999999999-old", "999999999999", "nginx")];
        let ops = plan(&[], &actual, &Trackers::new(), 100);
        assert_eq!(
            ops,
            vec![Op::Teardown {
                runtime_name: "p999999999999-old".to_owned()
            }]
        );
    }

    #[test]
    fn restart_policy_drives_exited_containers() {
        let mut actual = running(RT_NAME, "4f9d2c81aa55", "nginx");
        actual.status = RuntimeStatus::Exited(1);

        // Always restarts.
        let pod = desired(UID, "nginx");
        let ops = plan(
            std::slice::from_ref(&pod),
            std::slice::from_ref(&actual),
            &Trackers::new(),
            100,
        );
        assert!(matches!(ops.as_slice(), [Op::Restart { .. }]));

        // Never leaves the exited container in place (terminal).
        let mut never = desired(UID, "nginx");
        never.restart_policy = RestartPolicy::Never;
        assert!(
            plan(
                std::slice::from_ref(&never),
                std::slice::from_ref(&actual),
                &Trackers::new(),
                100
            )
            .is_empty()
        );

        // OnFailure: restart on non-zero only.
        let mut on_failure = desired(UID, "nginx");
        on_failure.restart_policy = RestartPolicy::OnFailure;
        assert!(
            !plan(
                std::slice::from_ref(&on_failure),
                std::slice::from_ref(&actual),
                &Trackers::new(),
                100
            )
            .is_empty()
        );
        actual.status = RuntimeStatus::Exited(0);
        assert!(
            plan(
                std::slice::from_ref(&on_failure),
                std::slice::from_ref(&actual),
                &Trackers::new(),
                100
            )
            .is_empty()
        );
    }

    #[test]
    fn backoff_gates_restarts_until_the_deadline() {
        let mut actual = running(RT_NAME, "4f9d2c81aa55", "nginx");
        actual.status = RuntimeStatus::Exited(1);
        let pod = desired(UID, "nginx");

        let mut trackers = Trackers::new();
        note_started(&mut trackers, RT_NAME, 100); // first start: gate at 100+1
        assert!(
            plan(
                std::slice::from_ref(&pod),
                std::slice::from_ref(&actual),
                &trackers,
                100
            )
            .is_empty()
        );
        assert!(
            !plan(
                std::slice::from_ref(&pod),
                std::slice::from_ref(&actual),
                &trackers,
                101
            )
            .is_empty()
        );

        // Delays double per restart and cap at 60s.
        assert_eq!(backoff_delay_secs(0), 1);
        assert_eq!(backoff_delay_secs(3), 8);
        assert_eq!(backoff_delay_secs(10), 60);
    }

    #[test]
    fn stable_runs_forgive_backoff_history() {
        let mut trackers = Trackers::new();
        note_started(&mut trackers, RT_NAME, 100);
        note_started(&mut trackers, RT_NAME, 200);
        assert_eq!(trackers[RT_NAME].restarts, 2);

        let pod = desired(UID, "nginx");
        let mut actual = running(RT_NAME, "4f9d2c81aa55", "nginx");
        actual.created_at = Some(chrono::Utc::now() - chrono::Duration::seconds(120));
        note_observed(
            &mut trackers,
            std::slice::from_ref(&pod),
            std::slice::from_ref(&actual),
            chrono::Utc::now(),
        );
        assert_eq!(trackers[RT_NAME].restarts, 0);

        // Trackers survive while still desired even with no container on the
        // node — a failed start must keep backing off, not reset every pass.
        note_started(&mut trackers, RT_NAME, 300);
        note_observed(
            &mut trackers,
            std::slice::from_ref(&pod),
            &[],
            chrono::Utc::now(),
        );
        assert_eq!(trackers[RT_NAME].restarts, 1);

        // Gone from both desired and actual: pruned.
        note_observed(&mut trackers, &[], &[], chrono::Utc::now());
        assert!(trackers.is_empty());
    }

    #[test]
    fn image_drift_triggers_recreation() {
        let desired = [desired(UID, "nginx:1.28")];
        let actual = [running(RT_NAME, "4f9d2c81aa55", "nginx:1.27")];
        let ops = plan(&desired, &actual, &Trackers::new(), 100);
        assert!(matches!(ops.as_slice(), [Op::Restart { .. }]));
    }

    #[test]
    fn statuses_cover_the_lifecycle() {
        let pod = desired(UID, "nginx");

        // Not started yet.
        let status = pod_status(&pod, None, None, None, None);
        assert_eq!(status.phase, PodPhase::Pending);
        assert_eq!(
            status.container_statuses[0]
                .state
                .waiting
                .as_ref()
                .unwrap()
                .reason,
            "ContainerCreating"
        );

        // Pull failure surfaces as the waiting reason.
        let status = pod_status(
            &pod,
            None,
            None,
            Some("ErrImagePull: manifest unknown"),
            None,
        );
        assert!(
            status
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("ErrImagePull")
        );

        // Running.
        let actual = running(RT_NAME, "4f9d2c81aa55", "nginx");
        let status = pod_status(&pod, Some(&actual), None, None, None);
        assert_eq!(status.phase, PodPhase::Running);
        assert_eq!(status.pod_ip.unwrap().to_string(), "10.42.0.6");
        assert!(status.container_statuses[0].ready);

        // A readiness probe that hasn't passed keeps a running pod not-ready.
        let status = pod_status(&pod, Some(&actual), None, None, Some(false));
        assert_eq!(status.phase, PodPhase::Running);
        assert!(
            !status.container_statuses[0].ready,
            "readiness gate applies"
        );

        // Crash-looping: phase stays Running, state says CrashLoopBackOff.
        let mut crashed = actual.clone();
        crashed.status = RuntimeStatus::Exited(1);
        let tracker = Tracker {
            restarts: 3,
            ..Tracker::default()
        };
        let status = pod_status(&pod, Some(&crashed), Some(&tracker), None, None);
        assert_eq!(status.phase, PodPhase::Running);
        assert_eq!(status.container_statuses[0].restart_count, 2);
        assert_eq!(
            status.container_statuses[0]
                .state
                .waiting
                .as_ref()
                .unwrap()
                .reason,
            "CrashLoopBackOff"
        );

        // Terminal success under Never.
        let mut once = desired(UID, "busybox");
        once.restart_policy = RestartPolicy::Never;
        let mut done = actual.clone();
        done.status = RuntimeStatus::Exited(0);
        let status = pod_status(&once, Some(&done), None, None, None);
        assert_eq!(status.phase, PodPhase::Succeeded);

        // Invalid pods are Failed with the reason.
        let mut multi = simple_pod(UID, "nginx");
        multi.spec.containers.push(multi.spec.containers[0].clone());
        let invalid = desire(&multi, &BTreeMap::new(), &BTreeMap::new()).unwrap();
        let status = pod_status(&invalid, None, None, None, None);
        assert_eq!(status.phase, PodPhase::Failed);
        assert!(status.message.unwrap().contains("Phase 6"));
    }

    #[test]
    fn env_resolves_literals_configmaps_and_secrets() {
        let mut cm = ConfigMap {
            api_version: "v1".to_owned(),
            kind: "ConfigMap".to_owned(),
            metadata: rk_types::ObjectMeta::named("web-config"),
            data: BTreeMap::new(),
        };
        cm.data.insert("MODE".to_owned(), "production".to_owned());
        let mut secret = Secret {
            api_version: "v1".to_owned(),
            kind: "Secret".to_owned(),
            metadata: rk_types::ObjectMeta::named("creds"),
            data: BTreeMap::new(),
            string_data: BTreeMap::new(),
        };
        secret
            .string_data
            .insert("token".to_owned(), "hunter2".to_owned());
        secret.normalize();

        let cms = BTreeMap::from([("web-config".to_owned(), cm)]);
        let secrets = BTreeMap::from([("creds".to_owned(), secret)]);

        let env = vec![
            EnvVar {
                name: "PLAIN".to_owned(),
                value: Some("x".to_owned()),
                value_from: None,
            },
            EnvVar {
                name: "MODE".to_owned(),
                value: None,
                value_from: Some(rk_types::pod::EnvVarSource {
                    config_map_key_ref: Some(rk_types::pod::KeyRef {
                        name: "web-config".to_owned(),
                        key: "MODE".to_owned(),
                    }),
                    secret_key_ref: None,
                }),
            },
            EnvVar {
                name: "TOKEN".to_owned(),
                value: None,
                value_from: Some(rk_types::pod::EnvVarSource {
                    config_map_key_ref: None,
                    secret_key_ref: Some(rk_types::pod::KeyRef {
                        name: "creds".to_owned(),
                        key: "token".to_owned(),
                    }),
                }),
            },
        ];
        let resolved = resolve_env(&env, &cms, &secrets).unwrap();
        assert_eq!(
            resolved,
            vec!["PLAIN=x", "MODE=production", "TOKEN=hunter2"]
        );

        let missing = vec![EnvVar {
            name: "X".to_owned(),
            value: None,
            value_from: Some(rk_types::pod::EnvVarSource {
                config_map_key_ref: Some(rk_types::pod::KeyRef {
                    name: "nope".to_owned(),
                    key: "k".to_owned(),
                }),
                secret_key_ref: None,
            }),
        }];
        assert!(
            resolve_env(&missing, &cms, &secrets)
                .unwrap_err()
                .contains("not found")
        );
    }

    #[test]
    fn resource_limits_flow_into_the_plan() {
        let mut pod = simple_pod(UID, "nginx");
        pod.spec.containers[0].resources.limits = ResourceList {
            cpu: Some("500m".parse().unwrap()),
            memory: Some("256Mi".parse().unwrap()),
        };
        let desired = desire(&pod, &BTreeMap::new(), &BTreeMap::new()).unwrap();
        let run = desired.plan.unwrap();
        assert_eq!(run.limits.cpu.unwrap().as_millis(), 500);
        assert_eq!(run.limits.memory.unwrap().as_bytes(), 256 << 20);
        assert_eq!(run.runtime_name, RT_NAME);
    }
}
