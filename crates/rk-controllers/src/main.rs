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
use rk_controllers::replicaset::{self, RsOp};
use rk_controllers::support::{self, POD_TEMPLATE_HASH_LABEL};
use rk_controllers::{ControllersConfig, gc};
use rk_controllers::{job, node};
use rk_types::Job;
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

    let client = Arc::new(ApiClient::connect(
        &config.api_url,
        config.token_file.as_deref(),
        config.ca_file.as_deref(),
    )?);
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
        if let Err(err) = reconcile_jobs(&client).await {
            warn!(error = %err, "Job reconciliation failed");
        }
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
            if pod
                .metadata
                .owner_references
                .iter()
                .any(|o| o.kind == ResourceKind::Job)
            {
                let mut failed = pod.clone();
                failed.status.phase = rk_types::PodPhase::Failed;
                failed.status.message = Some(rk_types::job::EVICTED_ATTEMPT_MESSAGE.into());
                replace_logging(client, &failed, "lost Job attempt").await;
                continue;
            }
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

// Finite workloads use authoritative lists rather than independently lagging
// caches: a successful create must be visible before the next batch is planned.
async fn reconcile_jobs(client: &ApiClient) -> Result<(), rk_types::client::ClientError> {
    let jobs = client.list::<Job>(None).await?.items;
    for job in &jobs {
        let pods = client
            .list::<Pod>(Some(job.metadata.namespace_or_default()))
            .await?
            .items;
        let plan = job::reconcile(job, &pods);
        if job.status != plan.status {
            let mut updated = job.clone();
            updated.status = plan.status;
            // Persist completion before cancellation or another attempt.
            if client.replace(&updated).await.is_err() {
                continue;
            }
        }
        for name in plan.cancel {
            client
                .delete::<Pod>(Some(job.metadata.namespace_or_default()), &name)
                .await?;
        }
        for (index, attempt) in plan.create {
            let pod = job::build_pod(job, index, attempt);
            match client.create(&pod).await {
                Ok(_) => info!(job = %job.metadata.name, index, attempt, "created Job attempt"),
                Err(rk_types::client::ClientError::Api { status: 409, .. }) => {}
                Err(err) => return Err(err),
            }
        }
    }
    // Resolve each owner directly: a Job created during this pass must not be
    // mistaken for an orphan because it was absent from the initial list.
    for pod in client.list::<Pod>(None).await?.items {
        if let Some(owner) = pod
            .metadata
            .owner_references
            .iter()
            .find(|o| o.kind == ResourceKind::Job)
        {
            let parent = client
                .get::<Job>(Some(pod.metadata.namespace_or_default()), &owner.name)
                .await?;
            if !parent.is_some_and(|j| j.metadata.uid.is_some() && j.metadata.uid == owner.uid) {
                match client
                    .delete::<Pod>(
                        Some(pod.metadata.namespace_or_default()),
                        &pod.metadata.name,
                    )
                    .await
                {
                    Ok(_) | Err(rk_types::client::ClientError::Api { status: 404, .. }) => {}
                    Err(err) => return Err(err),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod job_integration_tests {
    use super::*;
    use rk_types::{JobPhase, PodPhase};
    async fn server() -> ApiClient {
        let store = rk_store::SqliteStore::in_memory().await.unwrap();
        let state = rk_api::resources::AppState {
            store: Arc::new(store),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, rk_api::server::router(state))
                .await
                .unwrap();
        });
        ApiClient::new(format!("http://{addr}")).unwrap()
    }
    async fn create_job(client: &ApiClient) -> Job {
        let job: Job = serde_yaml::from_str("kind: Job\nmetadata:\n  name: sweep\nspec:\n  completions: 2\n  parallelism: 1\n  backoffLimit: 1\n  template:\n    spec:\n      restartPolicy: Never\n      containers:\n      - name: sim\n        image: alpine:3.21\n").unwrap();
        client.create(&job).await.unwrap()
    }
    async fn pods(client: &ApiClient) -> Vec<Pod> {
        client.list::<Pod>(None).await.unwrap().items
    }
    #[tokio::test]
    async fn finite_batch_retries_completes_and_garbage_collects_over_http() {
        let client = server().await;
        let job = create_job(&client).await;
        reconcile_jobs(&client).await.unwrap();
        reconcile_jobs(&client).await.unwrap();
        let mut first = pods(&client).await.remove(0);
        assert_eq!(pods(&client).await.len(), 1);
        first.status.phase = PodPhase::Failed;
        client.replace(&first).await.unwrap();
        reconcile_jobs(&client).await.unwrap();
        let mut retry = pods(&client)
            .await
            .into_iter()
            .find(|p| p.status.phase == PodPhase::Pending)
            .unwrap();
        assert_eq!(retry.metadata.labels[job::INDEX], "0");
        assert_eq!(retry.metadata.labels[job::ATTEMPT], "1");
        retry.status.phase = PodPhase::Succeeded;
        client.replace(&retry).await.unwrap();
        reconcile_jobs(&client).await.unwrap();
        let mut second = pods(&client)
            .await
            .into_iter()
            .find(|p| p.status.phase == PodPhase::Pending)
            .unwrap();
        assert_eq!(second.metadata.labels[job::INDEX], "1");
        second.status.phase = PodPhase::Succeeded;
        client.replace(&second).await.unwrap();
        reconcile_jobs(&client).await.unwrap();
        let done = client
            .get::<Job>(Some("default"), "sweep")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status.phase, JobPhase::Complete);
        assert_eq!(done.status.succeeded, 2);
        assert_eq!(done.status.failed, 1);
        // Reapply original spec without status, then remove attempt history.
        let mut manifest = serde_json::to_value(job).unwrap();
        manifest.as_object_mut().unwrap().remove("status");
        manifest["metadata"]["resourceVersion"] = serde_json::json!(done.metadata.resource_version);
        client
            .replace_raw(ResourceKind::Job, Some("default"), "sweep", &manifest)
            .await
            .unwrap();
        for pod in pods(&client).await {
            client
                .delete::<Pod>(Some("default"), &pod.metadata.name)
                .await
                .unwrap();
        }
        reconcile_jobs(&client).await.unwrap();
        assert!(pods(&client).await.is_empty());
        assert_eq!(
            client
                .get::<Job>(Some("default"), "sweep")
                .await
                .unwrap()
                .unwrap()
                .status
                .phase,
            JobPhase::Complete
        );
        client
            .delete::<Job>(Some("default"), "sweep")
            .await
            .unwrap();
        create_job(&client).await;
        reconcile_jobs(&client).await.unwrap();
        assert_eq!(pods(&client).await.len(), 1);
        client
            .delete::<Job>(Some("default"), "sweep")
            .await
            .unwrap();
        reconcile_jobs(&client).await.unwrap();
        assert!(pods(&client).await.is_empty());
    }
    #[tokio::test]
    async fn exhausted_retry_budget_survives_attempt_cleanup() {
        let client = server().await;
        create_job(&client).await;
        for _ in 0..2 {
            reconcile_jobs(&client).await.unwrap();
            let mut p = pods(&client)
                .await
                .into_iter()
                .find(|p| p.status.phase == PodPhase::Pending)
                .unwrap();
            p.status.phase = PodPhase::Failed;
            client.replace(&p).await.unwrap();
            reconcile_jobs(&client).await.unwrap();
            client
                .delete::<Pod>(Some("default"), &p.metadata.name)
                .await
                .unwrap();
        }
        reconcile_jobs(&client).await.unwrap();
        let job = client
            .get::<Job>(Some("default"), "sweep")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status.phase, JobPhase::Failed);
        assert_eq!(job.status.failed, 2);
        assert!(
            pods(&client)
                .await
                .iter()
                .all(|p| p.status.phase == PodPhase::Failed)
        );
    }
    #[tokio::test]
    async fn lost_node_retains_attempt_history_and_retries_with_new_index_attempt() {
        let client = server().await;
        create_job(&client).await;
        reconcile_jobs(&client).await.unwrap();
        let mut pod = pods(&client).await.remove(0);
        pod.spec.node_name = Some("lost-worker".into());
        pod.status.phase = PodPhase::Running;
        let pod = client.replace(&pod).await.unwrap();
        let node: Node = serde_yaml::from_str("metadata:\n  name: lost-worker\n").unwrap();
        let node = client.create(&node).await.unwrap();
        let caches = Caches {
            deployments: Arc::default(),
            replicasets: Arc::default(),
            pods: Arc::default(),
            nodes: Arc::default(),
        };
        caches.pods.lock().await.insert("pod".into(), pod.clone());
        caches.nodes.lock().await.insert("node".into(), node);
        reconcile_nodes(
            &client,
            &caches,
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
        .await;
        let failed = client
            .get::<Pod>(Some("default"), &pod.metadata.name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status.phase, PodPhase::Failed);
        assert_eq!(
            failed.status.message.as_deref(),
            Some(rk_types::job::EVICTED_ATTEMPT_MESSAGE)
        );
        reconcile_jobs(&client).await.unwrap();
        let all = pods(&client).await;
        assert_eq!(all.len(), 2);
        let retry = all
            .iter()
            .find(|p| p.status.phase == PodPhase::Pending)
            .unwrap();
        assert_eq!(retry.metadata.labels[job::INDEX], "0");
        assert_eq!(retry.metadata.labels[job::ATTEMPT], "1");
    }

    #[tokio::test]
    async fn job_admission_rejects_invalid_batches_and_spec_changes() {
        let client = server().await;
        let job = create_job(&client).await;
        let mut changed = job.clone();
        changed.spec.completions = 3;
        assert!(client.replace(&changed).await.is_err());
        for (parallelism, policy) in [
            (0, rk_types::RestartPolicy::Never),
            (3, rk_types::RestartPolicy::Never),
            (1, rk_types::RestartPolicy::Always),
        ] {
            let mut bad = job.clone();
            bad.metadata.name = "bad".into();
            bad.spec.parallelism = parallelism;
            bad.spec.template.spec.restart_policy = policy;
            assert!(client.create(&bad).await.is_err());
        }
    }
}
