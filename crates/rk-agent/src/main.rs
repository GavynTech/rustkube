use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use rk_agent::reconcile::{self, ActualContainer, DesiredPod, Op, Trackers};
use rk_agent::{AgentConfig, cache, names, node};
use rk_runtime::container::{self, PullPolicy, RunOptions};
use rk_runtime::store::Store;
use rk_types::client::{ApiClient, ClientError};
use rk_types::{ConfigMap, Pod, Secret};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

#[derive(Debug, Parser)]
#[command(name = "rk-agent", version, about = "RustKube node agent")]
struct Args {
    /// Path to the config file (default search: $RK_CONFIG,
    /// ./config/agent.yaml, /etc/rustkube/agent.yaml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> std::process::ExitCode {
    // Container shim path: rk-runtime starts containers by re-executing
    // /proc/self/exe with argv[1] == "shim" — in this process, that binary
    // is rk-agent. Route it before clap or tokio exist: the shim forks, and
    // forking after runtime threads spin up is undefined-behavior bingo.
    if std::env::args().nth(1).as_deref() == Some("shim") {
        return rk_runtime::init::shim_main();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("rk-agent: failed to start the async runtime: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run_agent()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rk-agent: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_agent() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    let config: AgentConfig = rk_types::bootstrap::load_config(args.config.as_deref(), "agent")?;

    if !nix::unistd::geteuid().is_root() {
        anyhow::bail!(
            "rk-agent drives namespaces, cgroups and mounts and must run as root \
             (run it on the rustkube VM)"
        );
    }

    let node_name = match &config.node_name {
        Some(name) => name.clone(),
        None => nix::unistd::gethostname()
            .context("reading hostname")?
            .to_string_lossy()
            .into_owned(),
    };
    let capacity = node::detect_capacity()?;
    let address = config.node_ip.or_else(node::detect_node_ip);
    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        node = %node_name,
        address = ?address,
        data_dir = %config.data_dir.display(),
        "rk-agent starting"
    );

    let client = Arc::new(ApiClient::new(&config.api_url)?);
    let store = Store::new(&config.data_dir);
    store.ensure_layout()?;

    // The API server may still be coming up; registration retries until it
    // answers, because nothing else can proceed without a Node.
    loop {
        match node::register(&client, &node_name, address, &capacity).await {
            Ok(()) => break,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "registration failed; retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    tokio::spawn(node::heartbeat_loop(
        client.clone(),
        node_name.clone(),
        address,
        capacity,
        config.heartbeat_interval_secs,
    ));

    let pods: cache::SharedPods = Arc::default();
    let notify = Arc::new(Notify::new());
    tokio::spawn(cache::run(
        client.clone(),
        node_name.clone(),
        pods.clone(),
        notify.clone(),
    ));

    let mut trackers = Trackers::new();
    let mut problems: HashMap<String, String> = HashMap::new();
    let resync = std::time::Duration::from_secs(config.resync_interval_secs.max(1));
    info!("reconcile loop running");
    loop {
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(resync) => {}
        }
        if let Err(err) = reconcile_once(&client, &store, &pods, &mut trackers, &mut problems).await
        {
            warn!(error = %format!("{err:#}"), "reconcile pass failed");
        }
    }
}

/// One full pass: snapshot desired, observe actual, plan, execute, report.
async fn reconcile_once(
    client: &ApiClient,
    store: &Store,
    pods: &cache::SharedPods,
    trackers: &mut Trackers,
    problems: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let snapshot: Vec<Pod> = pods.lock().await.values().cloned().collect();

    // Prefetch every ConfigMap/Secret the pods reference, namespace by
    // namespace, so plan building stays pure.
    let (config_maps, secrets) = prefetch_references(client, &snapshot).await;

    let empty_cms = BTreeMap::new();
    let empty_secrets = BTreeMap::new();
    let desired: Vec<DesiredPod> = snapshot
        .iter()
        .filter_map(|pod| {
            let ns = pod.metadata.namespace_or_default();
            reconcile::desire(
                pod,
                config_maps.get(ns).unwrap_or(&empty_cms),
                secrets.get(ns).unwrap_or(&empty_secrets),
            )
        })
        .collect();

    let actual = observe_actual(store)?;
    let now = chrono::Utc::now();
    let now_epoch = u64::try_from(now.timestamp()).unwrap_or(0);
    reconcile::note_observed(trackers, &desired, &actual, now);

    for op in reconcile::plan(&desired, &actual, trackers, now_epoch) {
        match op {
            Op::Teardown { runtime_name } => {
                debug!(container = %runtime_name, "tearing down orphan container");
                remove_container(store, &runtime_name).await;
            }
            Op::Start { uid } => {
                start_container(store, &desired, &uid, trackers, problems, now_epoch).await;
            }
            Op::Restart { uid, runtime_name } => {
                debug!(container = %runtime_name, "restarting container");
                remove_container(store, &runtime_name).await;
                start_container(store, &desired, &uid, trackers, problems, now_epoch).await;
            }
        }
    }

    // Report status from the post-execution view of the node.
    let actual = observe_actual(store)?;
    let by_name: HashMap<&str, &ActualContainer> = actual
        .iter()
        .map(|c| (c.runtime_name.as_str(), c))
        .collect();
    problems.retain(|uid, _| desired.iter().any(|d| &d.uid == uid));

    for pod in &snapshot {
        let Some(desired_pod) = desired
            .iter()
            .find(|d| Some(&d.uid) == pod.metadata.uid.as_ref())
        else {
            continue;
        };
        let actual_container = desired_pod
            .plan
            .as_ref()
            .ok()
            .and_then(|run| by_name.get(run.runtime_name.as_str()).copied());
        let tracker = desired_pod
            .plan
            .as_ref()
            .ok()
            .and_then(|run| trackers.get(&run.runtime_name));
        let status = reconcile::pod_status(
            desired_pod,
            actual_container,
            tracker,
            problems.get(&desired_pod.uid).map(String::as_str),
        );
        if pod.status != status {
            let mut updated = pod.clone();
            updated.status = status;
            match client.replace(&updated).await {
                Ok(_) => {
                    debug!(pod = %desired_pod.name, phase = ?updated.status.phase, "status reported");
                }
                // Stale cache revision: the watch delivers the fresh object
                // and the next pass reports against it.
                Err(ClientError::Api { status: 409, .. }) => {}
                Err(err) => warn!(pod = %desired_pod.name, error = %err, "status update failed"),
            }
        }
    }
    Ok(())
}

type RefMaps<T> = HashMap<String, BTreeMap<String, T>>;

/// Fetch referenced ConfigMaps/Secrets per namespace. Missing objects simply
/// don't appear (the planner reports "not found"); transport errors are
/// logged and treated the same — the next resync retries.
async fn prefetch_references(
    client: &ApiClient,
    pods: &[Pod],
) -> (RefMaps<ConfigMap>, RefMaps<Secret>) {
    let mut config_maps: RefMaps<ConfigMap> = HashMap::new();
    let mut secrets: RefMaps<Secret> = HashMap::new();
    for pod in pods {
        let ns = pod.metadata.namespace_or_default();
        for container in &pod.spec.containers {
            for env in &container.env {
                let Some(source) = &env.value_from else {
                    continue;
                };
                if let Some(key_ref) = &source.config_map_key_ref {
                    let entry = config_maps.entry(ns.to_owned()).or_default();
                    if !entry.contains_key(&key_ref.name) {
                        match client.get::<ConfigMap>(Some(ns), &key_ref.name).await {
                            Ok(Some(cm)) => {
                                entry.insert(key_ref.name.clone(), cm);
                            }
                            Ok(None) => {}
                            Err(err) => warn!(error = %err, "configmap fetch failed"),
                        }
                    }
                }
                if let Some(key_ref) = &source.secret_key_ref {
                    let entry = secrets.entry(ns.to_owned()).or_default();
                    if !entry.contains_key(&key_ref.name) {
                        match client.get::<Secret>(Some(ns), &key_ref.name).await {
                            Ok(Some(secret)) => {
                                entry.insert(key_ref.name.clone(), secret);
                            }
                            Ok(None) => {}
                            Err(err) => warn!(error = %err, "secret fetch failed"),
                        }
                    }
                }
            }
        }
    }
    (config_maps, secrets)
}

/// The node's containers that belong to this agent (recognized by the
/// `p<uid12>-` naming scheme). Hand-started `rkrt` containers are invisible
/// here and therefore never torn down as orphans.
fn observe_actual(store: &Store) -> anyhow::Result<Vec<ActualContainer>> {
    let rows = container::list(store)?;
    let mut actual = Vec::new();
    for row in rows {
        let Some((uid12, _)) = names::parse_runtime_name(&row.name) else {
            continue;
        };
        actual.push(ActualContainer {
            uid12: uid12.to_owned(),
            runtime_name: row.name,
            image: row.image,
            status: row.status,
            ip: Some(row.ip),
            created_at: chrono::DateTime::parse_from_rfc3339(&row.created_at)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc)),
        });
    }
    Ok(actual)
}

async fn remove_container(store: &Store, runtime_name: &str) {
    let store = store.clone();
    let name = runtime_name.to_owned();
    let result = tokio::task::spawn_blocking(move || container::remove(&store, &name, true)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(container = %runtime_name, error = %err, "remove failed"),
        Err(err) => warn!(container = %runtime_name, error = %err, "remove task panicked"),
    }
}

async fn start_container(
    store: &Store,
    desired: &[DesiredPod],
    uid: &str,
    trackers: &mut Trackers,
    problems: &mut HashMap<String, String>,
    now_epoch: u64,
) {
    let Some(pod) = desired.iter().find(|d| d.uid == uid) else {
        return;
    };
    let Ok(run) = &pod.plan else { return };

    // Arm the backoff gate before trying: a failing image pull must not
    // hot-loop against the registry any more than a crashing binary may.
    reconcile::note_started(trackers, &run.runtime_name, now_epoch);

    info!(pod = %pod.name, container = %run.runtime_name, image = %run.image, "starting container");
    let options = RunOptions {
        name: run.runtime_name.clone(),
        image: run.image.clone(),
        limits: run.limits,
        env: run.env.clone(),
        command: run.command.clone(),
        args: run.args.clone(),
        pull: PullPolicy::Auto,
    };
    match container::run(store, options).await {
        Ok(summary) => {
            info!(pod = %pod.name, ip = %summary.ip, pid1 = summary.pid1, "container running");
            problems.remove(uid);
        }
        Err(err) => {
            let chain = format!("{:#}", anyhow::Error::new(err));
            warn!(pod = %pod.name, error = %chain, "container start failed");
            problems.insert(uid.to_owned(), chain);
        }
    }
}
