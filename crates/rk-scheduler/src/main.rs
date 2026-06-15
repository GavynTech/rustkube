use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use rk_scheduler::SchedulerConfig;
use rk_scheduler::cache::{self, Cache};
use rk_scheduler::schedule::{self, NodeView, PodRequest};
use rk_types::client::{ApiClient, ClientError};
use rk_types::{Node, Pod, PodPhase, PodStatus};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

#[derive(Debug, Parser)]
#[command(name = "rk-scheduler", version, about = "RustKube pod scheduler")]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/scheduler.yaml, /etc/rustkube/scheduler.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

/// A bound pod takes a moment to reappear (with its `nodeName`) through the
/// watch; until then we skip re-binding it, but no longer than this in case
/// the bind was lost.
const ASSUME_BOUND_TTL: Duration = Duration::from_secs(30);

/// A binding we've written but the watch hasn't echoed back yet. Held so the
/// pod is neither re-bound nor — crucially — left out of the resource tally
/// (which would let the next pod oversubscribe the same node).
struct Assumed {
    node: String,
    request: PodRequest,
    when: Instant,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: SchedulerConfig =
        rk_types::bootstrap::load_config(args.config.as_deref(), "scheduler")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        "rk-scheduler starting"
    );

    let client = Arc::new(ApiClient::connect(
        &config.api_url,
        config.token_file.as_deref(),
        config.ca_file.as_deref(),
    )?);
    let nodes: Cache<Node> = Arc::default();
    let pods: Cache<Pod> = Arc::default();
    let notify = Arc::new(Notify::new());
    tokio::spawn(cache::reflect(
        client.clone(),
        nodes.clone(),
        notify.clone(),
    ));
    tokio::spawn(cache::reflect(client.clone(), pods.clone(), notify.clone()));

    // pod uid -> our not-yet-echoed binding (see Assumed / ASSUME_BOUND_TTL).
    let mut assumed: HashMap<String, Assumed> = HashMap::new();
    let resync = Duration::from_secs(config.resync_interval_secs.max(1));
    info!("scheduling loop running");
    loop {
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(resync) => {}
        }
        schedule_once(&client, &nodes, &pods, &mut assumed).await;
    }
}

async fn schedule_once(
    client: &ApiClient,
    nodes: &Cache<Node>,
    pods: &Cache<Pod>,
    assumed: &mut HashMap<String, Assumed>,
) {
    let node_objs = cache::snapshot(nodes).await;
    let pod_objs = cache::snapshot(pods).await;
    let views: Vec<NodeView> = node_objs.iter().map(NodeView::from_node).collect();
    let mut requested = schedule::tally_requested(&pod_objs);

    // Forget assumptions about pods that are now bound (gone from the
    // unscheduled set) or have aged out.
    let unscheduled: HashMap<&str, ()> = pod_objs
        .iter()
        .filter(|p| p.spec.node_name.is_none())
        .filter_map(|p| p.metadata.uid.as_deref().map(|uid| (uid, ())))
        .collect();
    assumed.retain(|uid, a| {
        unscheduled.contains_key(uid.as_str()) && a.when.elapsed() < ASSUME_BOUND_TTL
    });
    // Count still-pending assumed bindings against their nodes: the cache
    // doesn't show them bound yet, so without this the next pod could
    // oversubscribe a node we already filled.
    for a in assumed.values() {
        let entry = requested.entry(a.node.clone()).or_default();
        entry.cpu_millis += a.request.cpu_millis;
        entry.mem_bytes += a.request.mem_bytes;
    }

    // Schedule in a deterministic order so behavior is reproducible.
    let mut pending: Vec<&Pod> = pod_objs
        .iter()
        .filter(|p| p.spec.node_name.is_none())
        .collect();
    pending.sort_by(|a, b| {
        a.metadata
            .namespace_or_default()
            .cmp(b.metadata.namespace_or_default())
            .then_with(|| a.metadata.name.cmp(&b.metadata.name))
    });

    for pod in pending {
        let Some(uid) = pod.metadata.uid.clone() else {
            continue;
        };
        if assumed.contains_key(&uid) {
            continue; // bound this session; waiting for the watch to confirm
        }
        let request = PodRequest::of(pod);
        match schedule::schedule(&pod.spec.node_selector, request, &views, &requested) {
            Ok(node) => match bind(client, pod, &node).await {
                BindOutcome::Bound => {
                    info!(pod = %pod.metadata.name, %node, "pod bound");
                    let entry = requested.entry(node.clone()).or_default();
                    entry.cpu_millis += request.cpu_millis;
                    entry.mem_bytes += request.mem_bytes;
                    assumed.insert(
                        uid,
                        Assumed {
                            node,
                            request,
                            when: Instant::now(),
                        },
                    );
                }
                BindOutcome::Conflict => {
                    debug!(pod = %pod.metadata.name, "bind conflict; retrying next pass");
                }
                BindOutcome::Failed(err) => {
                    warn!(pod = %pod.metadata.name, error = %err, "bind failed");
                }
            },
            Err(reason) => report_unschedulable(client, pod, &reason).await,
        }
    }
}

enum BindOutcome {
    Bound,
    Conflict,
    Failed(String),
}

/// Write the binding: set `spec.nodeName` and reset status so any stale
/// "Unschedulable" message clears (the node's agent repopulates status once
/// it adopts the pod). Optimistic concurrency via the pod's resourceVersion.
async fn bind(client: &ApiClient, pod: &Pod, node: &str) -> BindOutcome {
    let mut updated = pod.clone();
    updated.spec.node_name = Some(node.to_owned());
    updated.status = PodStatus::default();
    match client.replace(&updated).await {
        Ok(_) => BindOutcome::Bound,
        Err(ClientError::Api { status: 409, .. }) => BindOutcome::Conflict,
        Err(err) => BindOutcome::Failed(err.to_string()),
    }
}

/// Record why an unscheduled pod can't be placed, in its `status.message`.
/// Safe because a pod with no `nodeName` has no owning agent. Idempotent: a
/// pod already showing this reason is left alone, so this never churns.
async fn report_unschedulable(client: &ApiClient, pod: &Pod, reason: &str) {
    if pod.status.phase == PodPhase::Pending && pod.status.message.as_deref() == Some(reason) {
        return;
    }
    let mut updated = pod.clone();
    updated.status = PodStatus {
        phase: PodPhase::Pending,
        message: Some(reason.to_owned()),
        ..PodStatus::default()
    };
    match client.replace(&updated).await {
        Ok(_) => debug!(pod = %pod.metadata.name, %reason, "pod marked unschedulable"),
        Err(ClientError::Api { status: 409, .. }) => {}
        Err(err) => warn!(pod = %pod.metadata.name, error = %err, "unschedulable update failed"),
    }
}
