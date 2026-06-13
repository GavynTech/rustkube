//! The controller manager: one reconcile loop over shared Deployment,
//! ReplicaSet, Pod and Node caches. Each pass runs the pure planners in
//! dependency order — Deployment → ReplicaSet → Node → GC — and applies their
//! ops through the API client. All writes carry the cached resourceVersion;
//! a 409 just means the cache was stale and the next pass retries.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rk_controllers::deployment::{self, DeployOp, RsView};
use rk_controllers::node;
use rk_controllers::replicaset::{self, RsOp};
use rk_controllers::support::{self, POD_TEMPLATE_HASH_LABEL};
use rk_controllers::{ControllersConfig, gc};
use rk_types::client::ApiClient;
use rk_types::client::reflector::{Cache, reflect, snapshot};
use rk_types::workloads::{PodTemplateSpec, ReplicaSet, ReplicaSetSpec};
use rk_types::{Deployment, Node, NodeCondition, ObjectMeta, Pod, Resource, ResourceKind};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "rk-controllers",
    version,
    about = "RustKube controller manager"
)]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/controllers.yaml, /etc/rustkube/controllers.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

struct Caches {
    deployments: Cache<Deployment>,
    replicasets: Cache<ReplicaSet>,
    pods: Cache<Pod>,
    nodes: Cache<Node>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: ControllersConfig =
        rk_types::bootstrap::load_config(args.config.as_deref(), "controllers")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        "rk-controllers starting"
    );

    let client = Arc::new(ApiClient::new(&config.api_url)?);
    let caches = Caches {
        deployments: Arc::default(),
        replicasets: Arc::default(),
        pods: Arc::default(),
        nodes: Arc::default(),
    };
    let notify = Arc::new(Notify::new());
    tokio::spawn(reflect(
        client.clone(),
        caches.deployments.clone(),
        notify.clone(),
    ));
    tokio::spawn(reflect(
        client.clone(),
        caches.replicasets.clone(),
        notify.clone(),
    ));
    tokio::spawn(reflect(client.clone(), caches.pods.clone(), notify.clone()));
    tokio::spawn(reflect(
        client.clone(),
        caches.nodes.clone(),
        notify.clone(),
    ));

    let resync = Duration::from_secs(config.resync_interval_secs.max(1));
    let not_ready_after = Duration::from_secs(config.node_not_ready_secs.max(1));
    let evict_after = Duration::from_secs(config.pod_eviction_secs.max(1));
    info!("reconcile loop running");
    loop {
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(resync) => {}
        }
        reconcile_deployments(&client, &caches).await;
        reconcile_replicasets(&client, &caches).await;
        reconcile_nodes(&client, &caches, not_ready_after, evict_after).await;
        collect_garbage(&client, &caches).await;
    }
}

// ---- Deployment controller ----

/// The deployment's template with its `selector.matchLabels` merged in (every
/// managed pod must carry them) but no pod-template-hash — the canonical form
/// that both the revision hash and the created ReplicaSet derive from.
fn canonical_template(deploy: &Deployment) -> PodTemplateSpec {
    let mut template = deploy.spec.template.clone();
    for (key, value) in &deploy.spec.selector.match_labels {
        template.metadata.labels.insert(key.clone(), value.clone());
    }
    template.metadata.labels.remove(POD_TEMPLATE_HASH_LABEL);
    template
}

fn owned_replicasets<'a>(deploy: &Deployment, all: &'a [ReplicaSet]) -> Vec<&'a ReplicaSet> {
    let Some(uid) = deploy.metadata.uid.as_deref() else {
        return Vec::new();
    };
    all.iter()
        .filter(|rs| support::owned_by(&rs.metadata, ResourceKind::Deployment, uid))
        .collect()
}

fn rs_hash(rs: &ReplicaSet) -> String {
    rs.metadata
        .labels
        .get(POD_TEMPLATE_HASH_LABEL)
        .cloned()
        .unwrap_or_else(|| support::pod_template_hash(&rs.spec.template))
}

async fn reconcile_deployments(client: &ApiClient, caches: &Caches) {
    let deployments = snapshot(&caches.deployments).await;
    let replicasets = snapshot(&caches.replicasets).await;

    for deploy in &deployments {
        let template = canonical_template(deploy);
        let desired_hash = support::pod_template_hash(&template);
        let owned = owned_replicasets(deploy, &replicasets);
        let views: Vec<RsView> = owned
            .iter()
            .map(|rs| RsView {
                name: rs.metadata.name.clone(),
                hash: rs_hash(rs),
                spec_replicas: rs.spec.replicas,
                status_replicas: rs.status.replicas,
                ready_replicas: rs.status.ready_replicas,
                creation: rs.metadata.creation_timestamp,
            })
            .collect();

        let plan = deployment::reconcile(deploy, &desired_hash, &views);
        for op in plan.ops {
            match op {
                DeployOp::CreateReplicaSet { hash, replicas } => {
                    let rs = build_replicaset(deploy, &template, &hash, replicas);
                    match client.create::<ReplicaSet>(&rs).await {
                        Ok(_) => {
                            info!(deploy = %deploy.metadata.name, rs = %rs.metadata.name, replicas, "created ReplicaSet")
                        }
                        Err(err) => {
                            warn!(deploy = %deploy.metadata.name, error = %err, "create ReplicaSet failed")
                        }
                    }
                }
                DeployOp::ScaleReplicaSet { name, replicas } => {
                    scale_replicaset(client, &replicasets, &name, replicas).await;
                }
            }
        }

        if deploy.status != plan.status {
            let mut updated = deploy.clone();
            updated.status = plan.status;
            replace_logging(client, &updated, "deployment status").await;
        }
    }
}

fn build_replicaset(
    deploy: &Deployment,
    template: &PodTemplateSpec,
    hash: &str,
    replicas: u32,
) -> ReplicaSet {
    let mut labels = template.metadata.labels.clone();
    labels.insert(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.to_owned());

    let mut selector = deploy.spec.selector.clone();
    selector
        .match_labels
        .insert(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.to_owned());

    let mut rs_template = template.clone();
    rs_template
        .metadata
        .labels
        .insert(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.to_owned());

    ReplicaSet {
        api_version: "v1".to_owned(),
        kind: "ReplicaSet".to_owned(),
        metadata: ObjectMeta {
            name: format!("{}-{hash}", deploy.metadata.name),
            namespace: Some(deploy.metadata.namespace_or_default().to_owned()),
            labels,
            owner_references: vec![support::owner_ref(
                ResourceKind::Deployment,
                &deploy.metadata,
            )],
            ..ObjectMeta::default()
        },
        spec: ReplicaSetSpec {
            replicas,
            selector,
            template: rs_template,
        },
        status: Default::default(),
    }
}

async fn scale_replicaset(client: &ApiClient, all: &[ReplicaSet], name: &str, replicas: u32) {
    let Some(rs) = all.iter().find(|rs| rs.metadata.name == name) else {
        return;
    };
    if rs.spec.replicas == replicas {
        return;
    }
    let mut updated = rs.clone();
    updated.spec.replicas = replicas;
    match client.replace::<ReplicaSet>(&updated).await {
        Ok(_) => info!(rs = %name, replicas, "scaled ReplicaSet"),
        Err(rk_types::client::ClientError::Api { status: 409, .. }) => {}
        Err(err) => warn!(rs = %name, error = %err, "scale ReplicaSet failed"),
    }
}

// ---- ReplicaSet controller ----

async fn reconcile_replicasets(client: &ApiClient, caches: &Caches) {
    let replicasets = snapshot(&caches.replicasets).await;
    let pods = snapshot(&caches.pods).await;

    for rs in &replicasets {
        let plan = replicaset::reconcile(rs, &pods);
        for op in plan.ops {
            match op {
                RsOp::CreatePod => {
                    let pod = build_pod(rs);
                    match client.create::<Pod>(&pod).await {
                        Ok(created) => {
                            debug!(rs = %rs.metadata.name, pod = %created.metadata.name, "created pod")
                        }
                        Err(err) => {
                            warn!(rs = %rs.metadata.name, error = %err, "create pod failed")
                        }
                    }
                }
                RsOp::DeletePod { name } => {
                    let ns = rs.metadata.namespace_or_default();
                    match client.delete::<Pod>(Some(ns), &name).await {
                        Ok(_) => debug!(rs = %rs.metadata.name, pod = %name, "deleted pod"),
                        Err(rk_types::client::ClientError::Api { status: 404, .. }) => {}
                        Err(err) => {
                            warn!(rs = %rs.metadata.name, pod = %name, error = %err, "delete pod failed")
                        }
                    }
                }
            }
        }

        if rs.status != plan.status {
            let mut updated = rs.clone();
            updated.status = plan.status;
            replace_logging(client, &updated, "replicaset status").await;
        }
    }
}

fn build_pod(rs: &ReplicaSet) -> Pod {
    let name = format!("{}-{}", rs.metadata.name, support::rand_suffix());
    Pod {
        api_version: "v1".to_owned(),
        kind: "Pod".to_owned(),
        metadata: ObjectMeta {
            name,
            namespace: Some(rs.metadata.namespace_or_default().to_owned()),
            labels: rs.spec.template.metadata.labels.clone(),
            annotations: rs.spec.template.metadata.annotations.clone(),
            owner_references: vec![support::owner_ref(ResourceKind::ReplicaSet, &rs.metadata)],
            ..ObjectMeta::default()
        },
        spec: rs.spec.template.spec.clone(),
        status: Default::default(),
    }
}

// ---- Node controller ----

async fn reconcile_nodes(
    client: &ApiClient,
    caches: &Caches,
    not_ready_after: Duration,
    evict_after: Duration,
) {
    let nodes = snapshot(&caches.nodes).await;
    let pods = snapshot(&caches.pods).await;
    let plan = node::classify(&nodes, chrono::Utc::now(), not_ready_after, evict_after);

    for name in &plan.mark_not_ready {
        if let Some(node) = nodes.iter().find(|n| &n.metadata.name == name) {
            let mut updated = node.clone();
            updated.status.condition = NodeCondition::NotReady;
            match client.replace::<Node>(&updated).await {
                Ok(_) => warn!(node = %name, "node missed heartbeats — marked NotReady"),
                // The agent's heartbeat raced us back to Ready; it's alive.
                Err(rk_types::client::ClientError::Api { status: 409, .. }) => {}
                Err(err) => warn!(node = %name, error = %err, "mark NotReady failed"),
            }
        }
    }

    let dead: HashSet<&str> = plan.evict_from.iter().map(String::as_str).collect();
    for pod in &pods {
        let Some(node) = pod.spec.node_name.as_deref() else {
            continue;
        };
        if dead.contains(node) && !support::is_terminal(pod) {
            let ns = pod.metadata.namespace_or_default();
            match client.delete::<Pod>(Some(ns), &pod.metadata.name).await {
                Ok(_) => warn!(pod = %pod.metadata.name, %node, "evicted pod from dead node"),
                Err(rk_types::client::ClientError::Api { status: 404, .. }) => {}
                Err(err) => warn!(pod = %pod.metadata.name, error = %err, "evict failed"),
            }
        }
    }
}

// ---- Garbage collection ----

async fn collect_garbage(client: &ApiClient, caches: &Caches) {
    let deployments = snapshot(&caches.deployments).await;
    let replicasets = snapshot(&caches.replicasets).await;
    let pods = snapshot(&caches.pods).await;

    let live_deploys: HashSet<&str> = deployments
        .iter()
        .filter_map(|d| d.metadata.uid.as_deref())
        .collect();
    for rs in &replicasets {
        if gc::is_orphan(&rs.metadata, ResourceKind::Deployment, &live_deploys) {
            let ns = rs.metadata.namespace_or_default();
            match client
                .delete::<ReplicaSet>(Some(ns), &rs.metadata.name)
                .await
            {
                Ok(_) => info!(rs = %rs.metadata.name, "garbage-collected orphaned ReplicaSet"),
                Err(rk_types::client::ClientError::Api { status: 404, .. }) => {}
                Err(err) => warn!(rs = %rs.metadata.name, error = %err, "GC ReplicaSet failed"),
            }
        }
    }

    let live_rs: HashSet<&str> = replicasets
        .iter()
        .filter_map(|rs| rs.metadata.uid.as_deref())
        .collect();
    for pod in &pods {
        if gc::is_orphan(&pod.metadata, ResourceKind::ReplicaSet, &live_rs) {
            let ns = pod.metadata.namespace_or_default();
            match client.delete::<Pod>(Some(ns), &pod.metadata.name).await {
                Ok(_) => debug!(pod = %pod.metadata.name, "garbage-collected orphaned pod"),
                Err(rk_types::client::ClientError::Api { status: 404, .. }) => {}
                Err(err) => warn!(pod = %pod.metadata.name, error = %err, "GC pod failed"),
            }
        }
    }
}

// ---- shared ----

async fn replace_logging<R: Resource>(client: &ApiClient, object: &R, what: &str) {
    match client.replace(object).await {
        Ok(_) => {}
        // Stale cache: the next pass recomputes against the fresh object.
        Err(rk_types::client::ClientError::Api { status: 409, .. }) => {}
        Err(err) => warn!(%what, error = %err, "status update failed"),
    }
}
