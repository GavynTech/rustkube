//! Node controller: turn missed heartbeats into NotReady, and once a node has
//! been dead long enough, evict its pods so they reschedule elsewhere. Pure —
//! it classifies nodes by heartbeat age; the loop flips status and deletes the
//! pods bound to dead nodes (the ReplicaSet controller then replaces them, and
//! the scheduler avoids the NotReady node).

use std::time::Duration;

use chrono::{DateTime, Utc};
use rk_types::{Node, NodeCondition};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct NodePlan {
    /// Ready nodes whose heartbeat has gone stale — flip to NotReady.
    pub mark_not_ready: Vec<String>,
    /// Nodes dead long enough that their pods should be evicted.
    pub evict_from: Vec<String>,
}

/// How long since a node's last heartbeat. A node that never heartbeat is
/// treated as maximally stale.
fn staleness(node: &Node, now: DateTime<Utc>) -> Duration {
    match node.status.last_heartbeat {
        Some(hb) => (now - hb).to_std().unwrap_or(Duration::ZERO),
        None => Duration::MAX,
    }
}

pub fn classify(
    nodes: &[Node],
    now: DateTime<Utc>,
    not_ready_after: Duration,
    evict_after: Duration,
) -> NodePlan {
    let mut plan = NodePlan::default();
    for node in nodes {
        let stale = staleness(node, now);
        if node.status.condition == NodeCondition::Ready && stale > not_ready_after {
            plan.mark_not_ready.push(node.metadata.name.clone());
        }
        if stale > evict_after {
            plan.evict_from.push(node.metadata.name.clone());
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use rk_types::quantity::ResourceList;
    use rk_types::{NodeStatus, ObjectMeta};

    use super::*;

    fn node(name: &str, condition: NodeCondition, heartbeat_age_secs: Option<i64>) -> Node {
        Node {
            api_version: "v1".to_owned(),
            kind: "Node".to_owned(),
            metadata: ObjectMeta::named(name),
            spec: Default::default(),
            status: NodeStatus {
                condition,
                last_heartbeat: heartbeat_age_secs
                    .map(|age| Utc::now() - chrono::Duration::seconds(age)),
                capacity: ResourceList::default(),
                allocatable: ResourceList::default(),
                address: None,
            },
        }
    }

    fn classify_default(nodes: &[Node]) -> NodePlan {
        classify(
            nodes,
            Utc::now(),
            Duration::from_secs(30),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn fresh_ready_node_is_left_alone() {
        let plan = classify_default(&[node("n1", NodeCondition::Ready, Some(5))]);
        assert!(plan.mark_not_ready.is_empty());
        assert!(plan.evict_from.is_empty());
    }

    #[test]
    fn stale_ready_node_is_marked_not_ready() {
        let plan = classify_default(&[node("n1", NodeCondition::Ready, Some(40))]);
        assert_eq!(plan.mark_not_ready, vec!["n1"]);
        assert!(
            plan.evict_from.is_empty(),
            "not dead long enough to evict yet"
        );
    }

    #[test]
    fn long_dead_node_has_pods_evicted() {
        // Already NotReady (marked earlier), now past the eviction grace.
        let plan = classify_default(&[node("n1", NodeCondition::NotReady, Some(90))]);
        assert!(plan.mark_not_ready.is_empty(), "already NotReady");
        assert_eq!(plan.evict_from, vec!["n1"]);
    }

    #[test]
    fn node_that_never_heartbeat_is_maximally_stale() {
        let plan = classify_default(&[node("n1", NodeCondition::Ready, None)]);
        assert_eq!(plan.mark_not_ready, vec!["n1"]);
        assert_eq!(plan.evict_from, vec!["n1"]);
    }
}
