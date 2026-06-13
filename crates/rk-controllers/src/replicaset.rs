//! ReplicaSet controller: keep exactly `spec.replicas` live pods. Pure — it
//! reads the ReplicaSet and the pods it owns and returns the create/delete
//! ops plus the recomputed status; the loop in `main.rs` executes them.

use std::cmp::Reverse;

use chrono::{DateTime, Utc};
use rk_types::ResourceKind;
use rk_types::pod::Pod;
use rk_types::workloads::{ReplicaSet, ReplicaSetStatus};

use crate::support;

#[derive(Debug, PartialEq, Eq)]
pub enum RsOp {
    /// Create one pod from the ReplicaSet's template.
    CreatePod,
    DeletePod {
        name: String,
    },
}

#[derive(Debug)]
pub struct RsPlan {
    pub ops: Vec<RsOp>,
    pub status: ReplicaSetStatus,
}

/// The pods this ReplicaSet controls, by owner reference (not selector) — so
/// two ReplicaSets that happen to share labels never fight over pods.
pub fn owned_pods<'a>(rs: &ReplicaSet, pods: &'a [Pod]) -> Vec<&'a Pod> {
    let Some(uid) = rs.metadata.uid.as_deref() else {
        return Vec::new();
    };
    pods.iter()
        .filter(|pod| support::owned_by(&pod.metadata, ResourceKind::ReplicaSet, uid))
        .collect()
}

/// Delete order for excess pods: least valuable first — not-ready before
/// ready, and among equals the newest (it has invested the least).
fn victim_key(pod: &Pod) -> (bool, Reverse<Option<DateTime<Utc>>>) {
    (
        support::is_ready(pod),
        Reverse(pod.metadata.creation_timestamp),
    )
}

pub fn reconcile(rs: &ReplicaSet, pods: &[Pod]) -> RsPlan {
    let desired = rs.spec.replicas as usize;
    let mut active: Vec<&Pod> = Vec::new();
    let mut terminal: Vec<&Pod> = Vec::new();
    for pod in owned_pods(rs, pods) {
        if support::is_terminal(pod) {
            terminal.push(pod);
        } else {
            active.push(pod);
        }
    }
    let ready = active.iter().filter(|pod| support::is_ready(pod)).count();

    let mut ops = Vec::new();
    // Terminal pods are done; clear them, and the active top-up replaces them.
    for pod in &terminal {
        ops.push(RsOp::DeletePod {
            name: pod.metadata.name.clone(),
        });
    }

    if active.len() < desired {
        for _ in 0..(desired - active.len()) {
            ops.push(RsOp::CreatePod);
        }
    } else if active.len() > desired {
        active.sort_by_key(|pod| victim_key(pod));
        for pod in active.iter().take(active.len() - desired) {
            ops.push(RsOp::DeletePod {
                name: pod.metadata.name.clone(),
            });
        }
    }

    RsPlan {
        ops,
        status: ReplicaSetStatus {
            replicas: active.len() as u32,
            ready_replicas: ready as u32,
        },
    }
}

#[cfg(test)]
mod tests {
    use rk_types::pod::{ContainerStatus, PodPhase, PodSpec, PodStatus};
    use rk_types::workloads::ReplicaSetSpec;
    use rk_types::{ObjectMeta, OwnerReference};

    use super::*;

    const RS_UID: &str = "rs-uid-1";

    fn rs(replicas: u32) -> ReplicaSet {
        ReplicaSet {
            api_version: "v1".to_owned(),
            kind: "ReplicaSet".to_owned(),
            metadata: ObjectMeta {
                uid: Some(RS_UID.to_owned()),
                ..ObjectMeta::named("web")
            },
            spec: ReplicaSetSpec {
                replicas,
                ..ReplicaSetSpec::default()
            },
            status: Default::default(),
        }
    }

    fn pod(name: &str, owner: Option<&str>, phase: PodPhase, ready: bool, age_secs: i64) -> Pod {
        let mut meta = ObjectMeta {
            creation_timestamp: Some(Utc::now() - chrono::Duration::seconds(age_secs)),
            ..ObjectMeta::named(name)
        };
        if let Some(uid) = owner {
            meta.owner_references.push(OwnerReference {
                kind: ResourceKind::ReplicaSet,
                name: "web".to_owned(),
                uid: Some(uid.to_owned()),
                controller: true,
            });
        }
        Pod {
            api_version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            metadata: meta,
            spec: PodSpec::default(),
            status: PodStatus {
                phase,
                container_statuses: vec![ContainerStatus {
                    name: "c".to_owned(),
                    ready,
                    restart_count: 0,
                    state: Default::default(),
                }],
                ..PodStatus::default()
            },
        }
    }

    #[test]
    fn scales_up_from_nothing() {
        let plan = reconcile(&rs(3), &[]);
        assert_eq!(
            plan.ops,
            vec![RsOp::CreatePod, RsOp::CreatePod, RsOp::CreatePod]
        );
        assert_eq!(plan.status.replicas, 0);
    }

    #[test]
    fn steady_state_is_a_noop() {
        let pods = [
            pod("web-a", Some(RS_UID), PodPhase::Running, true, 30),
            pod("web-b", Some(RS_UID), PodPhase::Running, true, 30),
        ];
        let plan = reconcile(&rs(2), &pods);
        assert!(plan.ops.is_empty());
        assert_eq!(plan.status.replicas, 2);
        assert_eq!(plan.status.ready_replicas, 2);
    }

    #[test]
    fn ignores_pods_it_does_not_own() {
        let pods = [
            pod("mine", Some(RS_UID), PodPhase::Running, true, 30),
            pod("theirs", Some("other-rs"), PodPhase::Running, true, 30),
            pod("orphan", None, PodPhase::Running, true, 30),
        ];
        let plan = reconcile(&rs(1), &pods);
        assert!(plan.ops.is_empty(), "already at 1 owned pod");
        assert_eq!(plan.status.replicas, 1);
    }

    #[test]
    fn scales_down_deleting_not_ready_then_newest() {
        let pods = [
            pod("old-ready", Some(RS_UID), PodPhase::Running, true, 100),
            pod("new-ready", Some(RS_UID), PodPhase::Running, true, 10),
            pod("pending", Some(RS_UID), PodPhase::Pending, false, 50),
        ];
        // Want 1; delete two: the Pending (not ready) and the newest ready.
        let plan = reconcile(&rs(1), &pods);
        let deleted: Vec<&str> = plan
            .ops
            .iter()
            .filter_map(|op| match op {
                RsOp::DeletePod { name } => Some(name.as_str()),
                RsOp::CreatePod => None,
            })
            .collect();
        assert_eq!(deleted, vec!["pending", "new-ready"]);
        assert!(
            deleted.iter().all(|d| *d != "old-ready"),
            "oldest ready survives"
        );
    }

    #[test]
    fn replaces_terminal_pods() {
        let pods = [
            pod("done", Some(RS_UID), PodPhase::Succeeded, false, 30),
            pod("running", Some(RS_UID), PodPhase::Running, true, 30),
        ];
        // desired 2, active 1 (running), terminal 1 (done): delete done + create 1.
        let plan = reconcile(&rs(2), &pods);
        assert!(plan.ops.contains(&RsOp::DeletePod {
            name: "done".to_owned()
        }));
        assert_eq!(
            plan.ops.iter().filter(|op| **op == RsOp::CreatePod).count(),
            1
        );
        assert_eq!(
            plan.status.replicas, 1,
            "terminal pods don't count as replicas"
        );
    }
}
