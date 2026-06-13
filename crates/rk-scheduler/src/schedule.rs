//! The scheduling decision, kept pure: given a pod's needs, the cluster's
//! nodes, and what's already requested on each, pick a node — or explain why
//! nothing fits. No I/O here, so every rule is unit-tested directly.
//!
//! Two stages, like Kubernetes: **filter** to feasible nodes (Ready, not
//! cordoned, `nodeSelector` matches, fits the free resources), then **score**
//! and take the best. The scoring is *most-allocated* — pack onto the
//! fullest node a pod still fits on — which is the resource-aware bin packing
//! the spec calls for (fewer nodes used, tighter packing).

use std::collections::{BTreeMap, HashMap};

use rk_types::{Cpu, Memory, Node, NodeCondition, Pod};

/// A node distilled to what scheduling needs.
#[derive(Debug, Clone)]
pub struct NodeView {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub ready: bool,
    pub schedulable: bool,
    pub alloc_cpu_millis: u64,
    pub alloc_mem_bytes: u64,
}

impl NodeView {
    pub fn from_node(node: &Node) -> Self {
        NodeView {
            name: node.metadata.name.clone(),
            labels: node.metadata.labels.clone(),
            ready: node.status.condition == NodeCondition::Ready,
            schedulable: !node.spec.unschedulable,
            alloc_cpu_millis: node.status.allocatable.cpu.map_or(0, Cpu::as_millis),
            alloc_mem_bytes: node.status.allocatable.memory.map_or(0, Memory::as_bytes),
        }
    }
}

/// A pod's resource ask: the sum of its containers' *requests* (limits are
/// runtime enforcement, not placement). Absent requests count as zero,
/// matching Kubernetes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PodRequest {
    pub cpu_millis: u64,
    pub mem_bytes: u64,
}

impl PodRequest {
    pub fn of(pod: &Pod) -> Self {
        let mut total = PodRequest::default();
        for container in &pod.spec.containers {
            total.cpu_millis += container.resources.requests.cpu.map_or(0, Cpu::as_millis);
            total.mem_bytes += container
                .resources
                .requests
                .memory
                .map_or(0, Memory::as_bytes);
        }
        total
    }

    fn add(&mut self, other: PodRequest) {
        self.cpu_millis += other.cpu_millis;
        self.mem_bytes += other.mem_bytes;
    }
}

/// Running tally of requested resources per node name.
pub type Requested = HashMap<String, PodRequest>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Infeasible {
    NotReady,
    Cordoned,
    SelectorMismatch,
    InsufficientCpu,
    InsufficientMemory,
}

impl Infeasible {
    /// Kubernetes-flavored phrasing for the aggregated reason line.
    fn describe(self) -> &'static str {
        match self {
            Infeasible::NotReady => "node(s) were not ready",
            Infeasible::Cordoned => "node(s) were unschedulable",
            Infeasible::SelectorMismatch => "node(s) didn't match nodeSelector",
            Infeasible::InsufficientCpu => "Insufficient cpu",
            Infeasible::InsufficientMemory => "Insufficient memory",
        }
    }
}

fn selector_matches(
    selector: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> bool {
    selector
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

fn free(node: &NodeView, requested: &Requested) -> (u64, u64) {
    let used = requested.get(&node.name).copied().unwrap_or_default();
    (
        node.alloc_cpu_millis.saturating_sub(used.cpu_millis),
        node.alloc_mem_bytes.saturating_sub(used.mem_bytes),
    )
}

fn feasibility(
    node: &NodeView,
    selector: &BTreeMap<String, String>,
    request: PodRequest,
    requested: &Requested,
) -> Result<(), Infeasible> {
    if !node.ready {
        return Err(Infeasible::NotReady);
    }
    if !node.schedulable {
        return Err(Infeasible::Cordoned);
    }
    if !selector_matches(selector, &node.labels) {
        return Err(Infeasible::SelectorMismatch);
    }
    let (free_cpu, free_mem) = free(node, requested);
    if free_cpu < request.cpu_millis {
        return Err(Infeasible::InsufficientCpu);
    }
    if free_mem < request.mem_bytes {
        return Err(Infeasible::InsufficientMemory);
    }
    Ok(())
}

/// Per-mille utilization of one resource after placement; a zero-capacity
/// resource reads fully used (a pod needing it would already be filtered).
fn frac_permille(used: u64, alloc: u64) -> u64 {
    (used.min(alloc) * 1000).checked_div(alloc).unwrap_or(1000)
}

/// Most-allocated score: average post-placement utilization across cpu and
/// memory, 0..=1000. Higher means fuller — and preferred, for bin packing.
fn score(node: &NodeView, request: PodRequest, requested: &Requested) -> u64 {
    let used = requested.get(&node.name).copied().unwrap_or_default();
    let cpu = frac_permille(used.cpu_millis + request.cpu_millis, node.alloc_cpu_millis);
    let mem = frac_permille(used.mem_bytes + request.mem_bytes, node.alloc_mem_bytes);
    (cpu + mem) / 2
}

fn unschedulable_message(node_count: usize, reasons: &BTreeMap<&'static str, usize>) -> String {
    if reasons.is_empty() {
        return format!("0/{node_count} nodes available");
    }
    let detail = reasons
        .iter()
        .map(|(reason, count)| format!("{count} {reason}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("0/{node_count} nodes available: {detail}")
}

/// Pick the best node for a pod, or return a human-readable reason nothing
/// fits (suitable for the pod's `status.message`). Deterministic: ties in
/// score break by node name.
pub fn schedule(
    selector: &BTreeMap<String, String>,
    request: PodRequest,
    nodes: &[NodeView],
    requested: &Requested,
) -> Result<String, String> {
    let mut best: Option<(&NodeView, u64)> = None;
    let mut reasons: BTreeMap<&'static str, usize> = BTreeMap::new();

    for node in nodes {
        match feasibility(node, selector, request, requested) {
            Ok(()) => {
                let node_score = score(node, request, requested);
                let better = match best {
                    None => true,
                    Some((current, current_score)) => {
                        node_score > current_score
                            || (node_score == current_score && node.name < current.name)
                    }
                };
                if better {
                    best = Some((node, node_score));
                }
            }
            Err(reason) => *reasons.entry(reason.describe()).or_default() += 1,
        }
    }

    match best {
        Some((node, _)) => Ok(node.name.clone()),
        None => Err(unschedulable_message(nodes.len(), &reasons)),
    }
}

/// Build the requested-resource tally from the pods currently bound to nodes.
/// Terminal pods (Succeeded/Failed) have released their resources and are
/// excluded; unscheduled pods (no `nodeName`) contribute nothing yet.
pub fn tally_requested(pods: &[Pod]) -> Requested {
    use rk_types::PodPhase;
    let mut requested = Requested::new();
    for pod in pods {
        let Some(node) = pod.spec.node_name.as_deref() else {
            continue;
        };
        if matches!(pod.status.phase, PodPhase::Succeeded | PodPhase::Failed) {
            continue;
        }
        requested
            .entry(node.to_owned())
            .or_default()
            .add(PodRequest::of(pod));
    }
    requested
}

#[cfg(test)]
mod tests {
    use rk_types::pod::PodSpec;
    use rk_types::quantity::{ResourceList, Resources};
    use rk_types::{Container, ObjectMeta};

    use super::*;

    fn node(name: &str, cpu_millis: u64, mem_bytes: u64) -> NodeView {
        NodeView {
            name: name.to_owned(),
            labels: BTreeMap::new(),
            ready: true,
            schedulable: true,
            alloc_cpu_millis: cpu_millis,
            alloc_mem_bytes: mem_bytes,
        }
    }

    fn req(cpu_millis: u64, mem_bytes: u64) -> PodRequest {
        PodRequest {
            cpu_millis,
            mem_bytes,
        }
    }

    fn empty() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn picks_the_only_feasible_node() {
        let nodes = [node("a", 1000, 1 << 30)];
        let chosen = schedule(&empty(), req(500, 1 << 20), &nodes, &Requested::new()).unwrap();
        assert_eq!(chosen, "a");
    }

    #[test]
    fn bin_packs_onto_the_fuller_node() {
        let nodes = [node("a", 2000, 2 << 30), node("b", 2000, 2 << 30)];
        // b already carries load; an equal pod should pile onto b, not spread.
        let mut requested = Requested::new();
        requested.insert("b".to_owned(), req(1000, 1 << 30));
        let chosen = schedule(&empty(), req(500, 512 << 20), &nodes, &requested).unwrap();
        assert_eq!(chosen, "b", "most-allocated packs onto the fuller node");
    }

    #[test]
    fn skips_nodes_that_dont_fit_and_falls_back() {
        let nodes = [node("small", 250, 1 << 30), node("big", 4000, 4 << 30)];
        let chosen = schedule(&empty(), req(1000, 1 << 30), &nodes, &Requested::new()).unwrap();
        assert_eq!(chosen, "big", "small lacks cpu, big takes it");
    }

    #[test]
    fn honors_node_selector() {
        let mut ssd = node("ssd-node", 4000, 4 << 30);
        ssd.labels.insert("disk".to_owned(), "ssd".to_owned());
        let hdd = node("hdd-node", 4000, 4 << 30);
        let nodes = [hdd, ssd];

        let mut selector = BTreeMap::new();
        selector.insert("disk".to_owned(), "ssd".to_owned());
        let chosen = schedule(&selector, req(500, 1 << 20), &nodes, &Requested::new()).unwrap();
        assert_eq!(chosen, "ssd-node");
    }

    #[test]
    fn ready_and_cordon_gates_apply() {
        let mut not_ready = node("down", 4000, 4 << 30);
        not_ready.ready = false;
        let mut cordoned = node("drain", 4000, 4 << 30);
        cordoned.schedulable = false;
        let nodes = [not_ready, cordoned];

        let err = schedule(&empty(), req(100, 1 << 20), &nodes, &Requested::new()).unwrap_err();
        assert!(err.contains("0/2 nodes available"), "{err}");
        assert!(err.contains("not ready"), "{err}");
        assert!(err.contains("unschedulable"), "{err}");
    }

    #[test]
    fn unschedulable_message_aggregates_reasons() {
        let nodes = [node("a", 100, 1 << 20), node("b", 100, 1 << 20)];
        // Needs more cpu than either has free.
        let err = schedule(&empty(), req(2000, 1 << 20), &nodes, &Requested::new()).unwrap_err();
        assert_eq!(err, "0/2 nodes available: 2 Insufficient cpu");
    }

    #[test]
    fn insufficient_memory_is_distinguished_from_cpu() {
        let nodes = [node("a", 4000, 64 << 20)];
        let err = schedule(&empty(), req(100, 1 << 30), &nodes, &Requested::new()).unwrap_err();
        assert!(err.contains("Insufficient memory"), "{err}");
    }

    #[test]
    fn requested_already_consumed_is_respected() {
        let nodes = [node("a", 1000, 1 << 30)];
        let mut requested = Requested::new();
        requested.insert("a".to_owned(), req(800, 0));
        // Only 200m free; a 500m pod can't fit.
        let err = schedule(&empty(), req(500, 1 << 20), &nodes, &requested).unwrap_err();
        assert!(err.contains("Insufficient cpu"), "{err}");
    }

    fn pod_with(node: Option<&str>, phase: rk_types::PodPhase, cpu: &str, mem: &str) -> Pod {
        Pod {
            api_version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            metadata: ObjectMeta::named("p"),
            spec: PodSpec {
                node_name: node.map(str::to_owned),
                containers: vec![Container {
                    name: "c".to_owned(),
                    image: "img".to_owned(),
                    command: None,
                    args: None,
                    env: Vec::new(),
                    ports: Vec::new(),
                    resources: Resources {
                        requests: ResourceList {
                            cpu: Some(cpu.parse().unwrap()),
                            memory: Some(mem.parse().unwrap()),
                        },
                        limits: ResourceList::default(),
                    },
                    volume_mounts: Vec::new(),
                    liveness_probe: None,
                    readiness_probe: None,
                }],
                ..PodSpec::default()
            },
            status: rk_types::PodStatus {
                phase,
                ..Default::default()
            },
        }
    }

    #[test]
    fn tally_sums_bound_nonterminal_pods_only() {
        let pods = [
            pod_with(Some("a"), rk_types::PodPhase::Running, "500m", "256Mi"),
            pod_with(Some("a"), rk_types::PodPhase::Pending, "250m", "128Mi"),
            pod_with(Some("a"), rk_types::PodPhase::Succeeded, "1", "1Gi"), // terminal: excluded
            pod_with(None, rk_types::PodPhase::Pending, "1", "1Gi"),        // unscheduled: excluded
        ];
        let requested = tally_requested(&pods);
        let a = requested.get("a").copied().unwrap();
        assert_eq!(a.cpu_millis, 750);
        assert_eq!(a.mem_bytes, (256 + 128) << 20);
        assert_eq!(requested.len(), 1);
    }

    #[test]
    fn pod_request_sums_containers_and_zero_fills() {
        let mut pod = pod_with(None, rk_types::PodPhase::Pending, "500m", "256Mi");
        // A second container with no requests adds nothing.
        let mut bare = pod.spec.containers[0].clone();
        bare.resources = Resources::default();
        pod.spec.containers.push(bare);
        let request = PodRequest::of(&pod);
        assert_eq!(request.cpu_millis, 500);
        assert_eq!(request.mem_bytes, 256 << 20);
    }
}
