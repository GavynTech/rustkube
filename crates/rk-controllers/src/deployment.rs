//! Deployment controller: drive the set of ReplicaSets toward the desired
//! template and replica count. Pure — given the deployment, the hash of its
//! current template, and a view of its ReplicaSets, it returns the
//! create/scale ops and the recomputed status.
//!
//! Rolling update (the default): surge the new revision up by at most
//! `maxSurge` over desired, and drain old revisions down so long as enough
//! pods stay ready (`desired - maxUnavailable`). Each reconcile pass takes
//! one step; the loop re-runs until the new RS holds all replicas and the
//! old ones are at zero. `Recreate` instead drains all old pods first, then
//! brings the new revision up.

use chrono::{DateTime, Utc};
use rk_types::workloads::{Deployment, DeploymentStatus, DeploymentStrategyType};

/// A ReplicaSet as the Deployment controller needs to see it.
#[derive(Debug, Clone)]
pub struct RsView {
    pub name: String,
    pub hash: String,
    pub spec_replicas: u32,
    pub status_replicas: u32,
    pub ready_replicas: u32,
    pub creation: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeployOp {
    CreateReplicaSet { hash: String, replicas: u32 },
    ScaleReplicaSet { name: String, replicas: u32 },
}

#[derive(Debug)]
pub struct DeployPlan {
    pub ops: Vec<DeployOp>,
    pub status: DeploymentStatus,
}

fn status_of(rss: &[RsView], new: Option<&RsView>) -> DeploymentStatus {
    DeploymentStatus {
        replicas: rss.iter().map(|rs| rs.status_replicas).sum(),
        ready_replicas: rss.iter().map(|rs| rs.ready_replicas).sum(),
        updated_replicas: new.map_or(0, |rs| rs.status_replicas),
    }
}

pub fn reconcile(deploy: &Deployment, desired_hash: &str, rss: &[RsView]) -> DeployPlan {
    let desired = deploy.spec.replicas;
    let new = rss.iter().find(|rs| rs.hash == desired_hash);
    let old: Vec<&RsView> = rss.iter().filter(|rs| rs.hash != desired_hash).collect();

    let ops = match deploy.spec.strategy.strategy_type {
        DeploymentStrategyType::Recreate => plan_recreate(desired_hash, desired, new, &old),
        DeploymentStrategyType::RollingUpdate => {
            let ru = deploy.spec.strategy.rolling_update;
            // A rollout with no room to move can't progress; nudge surge to 1,
            // exactly as Kubernetes forbids both being zero.
            let (max_surge, max_unavailable) = if ru.max_surge == 0 && ru.max_unavailable == 0 {
                (1, 0)
            } else {
                (ru.max_surge, ru.max_unavailable)
            };
            plan_rolling(desired_hash, desired, max_surge, max_unavailable, new, &old)
        }
    };

    DeployPlan {
        ops,
        status: status_of(rss, new),
    }
}

fn plan_recreate(
    desired_hash: &str,
    desired: u32,
    new: Option<&RsView>,
    old: &[&RsView],
) -> Vec<DeployOp> {
    let mut ops = Vec::new();
    let old_still_present = old
        .iter()
        .any(|rs| rs.spec_replicas > 0 || rs.status_replicas > 0);

    // Always scale every non-empty old RS to zero.
    for rs in old {
        if rs.spec_replicas > 0 {
            ops.push(DeployOp::ScaleReplicaSet {
                name: rs.name.clone(),
                replicas: 0,
            });
        }
    }

    let new_target = if old_still_present { 0 } else { desired };
    apply_new_target(&mut ops, desired_hash, new, new_target);
    ops
}

fn plan_rolling(
    desired_hash: &str,
    desired: u32,
    max_surge: u32,
    max_unavailable: u32,
    new: Option<&RsView>,
    old: &[&RsView],
) -> Vec<DeployOp> {
    let mut ops = Vec::new();
    let new_current = new.map_or(0, |rs| rs.spec_replicas);
    let total_spec: u32 = new_current + old.iter().map(|rs| rs.spec_replicas).sum::<u32>();

    // 1. Scale the new revision up, bounded by the surge ceiling.
    let scale_up_room = (desired + max_surge).saturating_sub(total_spec);
    let new_target = desired.min(new_current + scale_up_room);
    apply_new_target(&mut ops, desired_hash, new, new_target);

    // 2. Scale old revisions down, keeping enough ready pods available.
    let total_spec_after = total_spec - new_current + new_target;
    let min_available = desired.saturating_sub(max_unavailable);
    let new_unavailable = new_target.saturating_sub(new.map_or(0, |rs| rs.ready_replicas));
    let mut budget = total_spec_after
        .saturating_sub(min_available)
        .saturating_sub(new_unavailable);

    // Drain oldest revisions first.
    let mut draining: Vec<&&RsView> = old.iter().filter(|rs| rs.spec_replicas > 0).collect();
    draining.sort_by_key(|rs| rs.creation);
    for rs in draining {
        if budget == 0 {
            break;
        }
        let reduce = rs.spec_replicas.min(budget);
        ops.push(DeployOp::ScaleReplicaSet {
            name: rs.name.clone(),
            replicas: rs.spec_replicas - reduce,
        });
        budget -= reduce;
    }
    ops
}

/// Emit the op to bring the new revision to `target`, creating its RS if it
/// doesn't exist yet. A no-op when it's already there — and we never create
/// an empty RS (target 0), so a drain doesn't spawn a placeholder.
fn apply_new_target(
    ops: &mut Vec<DeployOp>,
    desired_hash: &str,
    new: Option<&RsView>,
    target: u32,
) {
    match new {
        Some(rs) if rs.spec_replicas != target => ops.push(DeployOp::ScaleReplicaSet {
            name: rs.name.clone(),
            replicas: target,
        }),
        Some(_) => {}
        None if target == 0 => {}
        None => ops.push(DeployOp::CreateReplicaSet {
            hash: desired_hash.to_owned(),
            replicas: target,
        }),
    }
}

#[cfg(test)]
mod tests {
    use rk_types::ObjectMeta;
    use rk_types::workloads::{DeploymentSpec, DeploymentStrategy, RollingUpdate};

    use super::*;

    const NEW: &str = "newhash";
    const OLD: &str = "oldhash";

    fn deploy(replicas: u32, surge: u32, unavailable: u32) -> Deployment {
        Deployment {
            api_version: "v1".to_owned(),
            kind: "Deployment".to_owned(),
            metadata: ObjectMeta::named("web"),
            spec: DeploymentSpec {
                replicas,
                strategy: DeploymentStrategy {
                    strategy_type: DeploymentStrategyType::RollingUpdate,
                    rolling_update: RollingUpdate {
                        max_surge: surge,
                        max_unavailable: unavailable,
                    },
                },
                ..DeploymentSpec::default()
            },
            status: Default::default(),
        }
    }

    fn rsview(hash: &str, spec: u32, ready: u32, age: i64) -> RsView {
        RsView {
            name: format!("web-{hash}"),
            hash: hash.to_owned(),
            spec_replicas: spec,
            status_replicas: spec,
            ready_replicas: ready,
            creation: Some(Utc::now() - chrono::Duration::seconds(age)),
        }
    }

    #[test]
    fn fresh_deployment_creates_one_rs_at_full_size() {
        let plan = reconcile(&deploy(3, 1, 1), NEW, &[]);
        assert_eq!(
            plan.ops,
            vec![DeployOp::CreateReplicaSet {
                hash: NEW.to_owned(),
                replicas: 3
            }]
        );
    }

    #[test]
    fn steady_state_is_a_noop() {
        let rss = [rsview(NEW, 3, 3, 60)];
        let plan = reconcile(&deploy(3, 1, 1), NEW, &rss);
        assert!(plan.ops.is_empty());
        assert_eq!(plan.status.ready_replicas, 3);
        assert_eq!(plan.status.updated_replicas, 3);
    }

    #[test]
    fn rolling_update_surges_new_and_drains_old_one_step() {
        // Old revision fully up and ready; a new template arrives.
        let rss = [rsview(OLD, 3, 3, 120)];
        // surge 1, unavailable 1.
        let plan = reconcile(&deploy(3, 1, 1), NEW, &rss);
        // New RS created at the surge ceiling: desired(3)+surge(1)-total(3) = 1.
        assert!(plan.ops.contains(&DeployOp::CreateReplicaSet {
            hash: NEW.to_owned(),
            replicas: 1
        }));
        // Old drained by what availability allows:
        // total_after=4, min_available=2, new_unavailable=1-0=1 → budget=1 → old 3->2.
        assert!(plan.ops.contains(&DeployOp::ScaleReplicaSet {
            name: "web-oldhash".to_owned(),
            replicas: 2
        }));
    }

    #[test]
    fn rolling_update_completes_when_new_is_ready() {
        // Mid-rollout: new at 3 ready, old at 1 not yet drained.
        let rss = [rsview(NEW, 3, 3, 20), rsview(OLD, 1, 1, 120)];
        let plan = reconcile(&deploy(3, 1, 1), NEW, &rss);
        // New already at desired; drain the last old pod.
        // total_after=4, min_available=2, new_unavailable=0 → budget=2 → old 1->0.
        assert_eq!(
            plan.ops,
            vec![DeployOp::ScaleReplicaSet {
                name: "web-oldhash".to_owned(),
                replicas: 0
            }]
        );
    }

    #[test]
    fn rolling_update_waits_when_new_pods_are_not_ready() {
        // New surged to 1 but not ready yet; must not drain old below safety.
        let rss = [rsview(NEW, 1, 0, 5), rsview(OLD, 3, 3, 120)];
        let plan = reconcile(&deploy(3, 1, 1), NEW, &rss);
        // total_after=4, min_available=2, new_unavailable=1 → budget=1 → old 3->2.
        // (Only one old pod drained; the rest wait for new to become ready.)
        assert_eq!(
            plan.ops,
            vec![DeployOp::ScaleReplicaSet {
                name: "web-oldhash".to_owned(),
                replicas: 2
            }]
        );
    }

    #[test]
    fn recreate_drains_all_old_before_starting_new() {
        let mut d = deploy(2, 0, 0);
        d.spec.strategy.strategy_type = DeploymentStrategyType::Recreate;
        let rss = [rsview(OLD, 2, 2, 120)];
        let plan = reconcile(&d, NEW, &rss);
        // Old scaled to 0; new not started yet.
        assert_eq!(
            plan.ops,
            vec![DeployOp::ScaleReplicaSet {
                name: "web-oldhash".to_owned(),
                replicas: 0
            }]
        );

        // Once old is gone, the new RS comes up at full size.
        let rss = [rsview(OLD, 0, 0, 120)];
        let plan = reconcile(&d, NEW, &rss);
        assert_eq!(
            plan.ops,
            vec![DeployOp::CreateReplicaSet {
                hash: NEW.to_owned(),
                replicas: 2
            }]
        );
    }

    #[test]
    fn scaling_an_unchanged_deployment_resizes_the_current_rs() {
        let rss = [rsview(NEW, 2, 2, 60)];
        let plan = reconcile(&deploy(5, 1, 1), NEW, &rss);
        assert_eq!(
            plan.ops,
            vec![DeployOp::ScaleReplicaSet {
                name: "web-newhash".to_owned(),
                replicas: 5
            }]
        );
    }
}
