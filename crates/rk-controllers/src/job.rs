//! Indexed Jobs retain attempt pods for history. Deterministic attempt names
//! make repeated reconciliation safe, including after controller restarts.
use crate::support;
use rk_types::pod::EnvVar;
use rk_types::{Job, JobPhase, JobStatus, ObjectMeta, Pod, PodPhase, ResourceKind};
use std::collections::BTreeMap;

pub const INDEX: &str = "rustkube.io/job-index";
pub const ATTEMPT: &str = "rustkube.io/job-attempt";
#[derive(Debug, PartialEq)]
pub struct JobPlan {
    pub create: Vec<(u32, u32)>,
    pub cancel: Vec<String>,
    pub status: JobStatus,
}

pub fn reconcile(job: &Job, pods: &[Pod]) -> JobPlan {
    let mut plan = JobPlan {
        create: vec![],
        cancel: vec![],
        status: job.status.clone(),
    };
    let Some(uid) = job.metadata.uid.as_deref() else {
        return plan;
    };
    let owned: Vec<_> = pods
        .iter()
        .filter(|p| {
            p.metadata.namespace_or_default() == job.metadata.namespace_or_default()
                && support::owned_by(&p.metadata, ResourceKind::Job, uid)
        })
        .collect();
    let mut active = BTreeMap::<u32, u32>::new();
    for pod in &owned {
        let Some(index) = pod
            .metadata
            .labels
            .get(INDEX)
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        match pod.status.phase {
            PodPhase::Succeeded => {
                plan.status.completed_indexes.insert(index);
            }
            PodPhase::Failed => {
                if let Some(attempt) = pod
                    .metadata
                    .labels
                    .get(ATTEMPT)
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    plan.status
                        .failed_attempts
                        .entry(index)
                        .or_default()
                        .insert(attempt);
                }
            }
            _ => {
                *active.entry(index).or_default() += 1;
            }
        }
    }
    let failures: BTreeMap<u32, u32> = plan
        .status
        .failed_attempts
        .iter()
        .map(|(index, attempts)| (*index, attempts.len() as u32))
        .collect();
    plan.status.active = active.values().sum();
    plan.status.failed = failures.values().sum::<u32>().max(job.status.failed);
    plan.status.succeeded = plan.status.completed_indexes.len() as u32;
    if plan.status.phase == JobPhase::Running {
        if plan.status.succeeded >= job.spec.completions {
            plan.status.phase = JobPhase::Complete;
        } else if failures.iter().any(|(i, count)| {
            !plan.status.completed_indexes.contains(i) && *count > job.spec.backoff_limit
        }) {
            plan.status.phase = JobPhase::Failed;
        }
    }
    if plan.status.phase != JobPhase::Running {
        plan.cancel = owned
            .iter()
            .filter(|p| !support::is_terminal(p))
            .map(|p| p.metadata.name.clone())
            .collect();
        return plan;
    }
    let mut slots = job.spec.parallelism.saturating_sub(plan.status.active);
    for index in 0..job.spec.completions {
        if slots == 0 {
            break;
        }
        if plan.status.completed_indexes.contains(&index) || active.contains_key(&index) {
            continue;
        }
        // Failed attempt pods are deliberately retained. Their count is the next
        // attempt number, so re-listing before create never overfills a batch.
        plan.create
            .push((index, failures.get(&index).copied().unwrap_or(0)));
        slots -= 1;
    }
    plan
}

pub fn build_pod(job: &Job, index: u32, attempt: u32) -> Pod {
    let mut spec = job.spec.template.spec.clone();
    for container in &mut spec.containers {
        container.env.retain(|e| {
            !matches!(
                e.name.as_str(),
                "RK_JOB_INDEX" | "RK_JOB_ATTEMPT" | "RK_JOB_UID"
            )
        });
        for (name, value) in [
            ("RK_JOB_INDEX", index.to_string()),
            ("RK_JOB_ATTEMPT", attempt.to_string()),
            ("RK_JOB_UID", job.metadata.uid.clone().unwrap_or_default()),
        ] {
            container.env.push(EnvVar {
                name: name.into(),
                value: Some(value),
                value_from: None,
            });
        }
    }
    let mut labels = job.spec.template.metadata.labels.clone();
    labels.insert(INDEX.into(), index.to_string());
    labels.insert(ATTEMPT.into(), attempt.to_string());
    Pod {
        api_version: "v1".into(),
        kind: "Pod".into(),
        metadata: ObjectMeta {
            name: format!(
                "job-{}-{index}-{attempt}",
                job.metadata.uid.as_deref().unwrap_or(&job.metadata.name)
            ),
            namespace: Some(job.metadata.namespace_or_default().into()),
            labels,
            annotations: job.spec.template.metadata.annotations.clone(),
            owner_references: vec![support::owner_ref(ResourceKind::Job, &job.metadata)],
            ..Default::default()
        },
        spec,
        status: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> Job {
        serde_yaml::from_str("metadata:\n  name: sweep\n  uid: abc\nspec:\n  completions: 3\n  parallelism: 2\n  backoffLimit: 1\n").unwrap()
    }
    #[test]
    fn bounded_batch_and_stable_names() {
        let job = job();
        assert_eq!(reconcile(&job, &[]).create, vec![(0, 0), (1, 0)]);
        let first = build_pod(&job, 0, 0);
        assert_eq!(first, build_pod(&job, 0, 0));
        assert_eq!(reconcile(&job, &[first]).create, vec![(1, 0)]);
    }
    #[test]
    fn retries_only_failed_indexes_and_stops_at_limit() {
        let job = job();
        let mut a = build_pod(&job, 0, 0);
        a.status.phase = PodPhase::Succeeded;
        let mut b = build_pod(&job, 1, 0);
        b.status.phase = PodPhase::Failed;
        let plan = reconcile(&job, &[a.clone(), b.clone()]);
        assert_eq!(plan.create, vec![(1, 1), (2, 0)]);
        let mut retry = build_pod(&job, 1, 1);
        retry.status.phase = PodPhase::Failed;
        let running = build_pod(&job, 2, 0);
        let plan = reconcile(&job, &[a, b, retry, running.clone()]);
        assert_eq!(plan.status.phase, JobPhase::Failed);
        assert!(plan.create.is_empty());
        assert_eq!(plan.cancel, vec![running.metadata.name]);
    }
    #[test]
    fn completion_survives_missing_pods_and_terminal_jobs_never_restart() {
        let mut job = job();
        let pods: Vec<_> = (0..3)
            .map(|i| {
                let mut p = build_pod(&job, i, 0);
                p.status.phase = PodPhase::Succeeded;
                p
            })
            .collect();
        job.status = reconcile(&job, &pods).status;
        assert_eq!(job.status.phase, JobPhase::Complete);
        assert!(reconcile(&job, &[]).create.is_empty());
        assert_eq!(reconcile(&job, &[]).status.succeeded, 3);
    }
    #[test]
    fn unrelated_pods_do_not_count() {
        let job = job();
        let mut pod = build_pod(&job, 0, 0);
        pod.metadata.namespace = Some("other".into());
        pod.status.phase = PodPhase::Succeeded;
        assert_eq!(reconcile(&job, &[pod]).status.succeeded, 0);
    }
}
