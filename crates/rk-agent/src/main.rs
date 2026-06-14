use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use clap::Parser;
use rk_agent::reconcile::{self, ActualContainer, DesiredPod, Op, Trackers};
use rk_agent::{AgentConfig, cache, logs, names, node, overlay, probe, services};
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

    // Advertised capacity: the detected machine resources, with optional dev
    // overrides (netns-simulated nodes share the host's real cpu/memory, so
    // overrides are what make resource-based scheduling demonstrable).
    let mut capacity = node::detect_capacity()?;
    if let Some(cpu) = config.capacity_cpu {
        capacity.cpu = Some(cpu);
    }
    if let Some(memory) = config.capacity_memory {
        capacity.memory = Some(memory);
    }
    let address = config.node_ip.or_else(node::detect_node_ip);
    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_url = %config.api_url,
        node = %node_name,
        address = ?address,
        pod_cidr = config.pod_cidr.as_deref().unwrap_or("10.42.0.0/24 (default)"),
        data_dir = %config.data_dir.display(),
        "rk-agent starting"
    );

    let client = Arc::new(ApiClient::new(&config.api_url)?);
    let store = Store::new(&config.data_dir);
    store.ensure_layout()?;
    if let Some(cidr) = &config.pod_cidr {
        rk_runtime::net::configure_pod_cidr(&store, cidr)
            .with_context(|| format!("configuring pod CIDR {cidr}"))?;
    }

    // The node bridge must exist before the DNS server binds its gateway IP,
    // so create it now rather than lazily on the first pod. The gateway is
    // both the pods' default route and the cluster DNS server they query.
    let gateway = rk_runtime::net::ensure_node_bridge(&store)
        .await
        .context("creating the node bridge")?;
    let dns_server = Some(gateway);

    // Bring up this node's VXLAN endpoint and learn its MAC to advertise. The
    // overlay needs the node's pod CIDR and an underlay address; without both
    // we run single-node (same-node pod networking still works via the bridge).
    let vtep = match (&config.pod_cidr, address) {
        (Some(cidr), Some(IpAddr::V4(underlay))) => {
            let network = rk_net::parse_pod_cidr(cidr)?;
            match rk_net::overlay::ensure_vxlan(underlay, rk_net::vtep_ip(network)).await {
                Ok(vtep) => {
                    info!(mac = %vtep.mac, vtep_ip = %vtep.vxlan_ip, "VXLAN overlay endpoint up");
                    Some(vtep)
                }
                Err(err) => {
                    warn!(error = %err, "VXLAN setup failed; cross-node pod traffic disabled");
                    None
                }
            }
        }
        _ => {
            info!("no pod CIDR or IPv4 address; running without the VXLAN overlay");
            None
        }
    };

    let registration = node::Registration {
        name: node_name.clone(),
        address,
        capacity,
        labels: config.node_labels.clone(),
        pod_cidr: config.pod_cidr.clone(),
        vtep_mac: vtep.as_ref().map(|v| v.mac.clone()),
    };

    // The API server may still be coming up; registration retries until it
    // answers, because nothing else can proceed without a Node.
    loop {
        match node::register(&client, &registration).await {
            Ok(()) => break,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "registration failed; retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    tokio::spawn(node::heartbeat_loop(
        client.clone(),
        registration,
        config.heartbeat_interval_secs,
    ));

    // Keep the overlay's peer routes in sync with the Node list.
    if let Some(vtep) = vtep {
        tokio::spawn(overlay::run(
            client.clone(),
            node_name.clone(),
            vtep.vxlan_ip,
        ));
    }

    // The service data plane: nftables VIP rules + the embedded DNS server,
    // both fed from the cluster's Services and Pods.
    let dns_records: rk_net::dns::Records = Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(rk_net::dns::serve(
        std::net::SocketAddr::from((gateway, 53)),
        dns_records.clone(),
    ));
    tokio::spawn(services::run(client.clone(), dns_records));

    // Serve this node's container logs to `rk-ctl logs`.
    tokio::spawn(logs::serve(client.clone(), Arc::new(store.clone())));

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
    let mut probes: HashMap<String, probe::PodProbes> = HashMap::new();
    let resync = std::time::Duration::from_secs(config.resync_interval_secs.max(1));
    info!("reconcile loop running");
    loop {
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(resync) => {}
        }
        if let Err(err) = reconcile_once(
            &client,
            &store,
            &pods,
            &mut trackers,
            &mut problems,
            &mut probes,
            dns_server,
        )
        .await
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
    probes: &mut HashMap<String, probe::PodProbes>,
    dns_server: Option<Ipv4Addr>,
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
                start_container(
                    store, &desired, &uid, trackers, problems, now_epoch, dns_server,
                )
                .await;
            }
            Op::Restart { uid, runtime_name } => {
                debug!(container = %runtime_name, "restarting container");
                remove_container(store, &runtime_name).await;
                start_container(
                    store, &desired, &uid, trackers, problems, now_epoch, dns_server,
                )
                .await;
            }
        }
    }

    // Run any due liveness/readiness probes; liveness failures restart the
    // container (removed here, recreated next pass), readiness gates `ready`.
    let observed = observe_actual(store)?;
    let (readiness, to_restart) = run_probes(&desired, &observed, probes, now_epoch).await;
    for runtime_name in to_restart {
        warn!(container = %runtime_name, "liveness probe failed — restarting container");
        remove_container(store, &runtime_name).await;
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
            readiness.get(&desired_pod.uid).copied(),
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
/// Run due probes for every running pod, updating `probes` state in place.
/// Returns each pod's readiness verdict (for pods with a readiness probe) and
/// the runtime names of containers a liveness probe wants restarted.
async fn run_probes(
    desired: &[DesiredPod],
    actual: &[ActualContainer],
    probes: &mut HashMap<String, probe::PodProbes>,
    now: u64,
) -> (HashMap<String, bool>, Vec<String>) {
    use probe::{ProbeKind, ProbeState};
    let by_name: HashMap<&str, &ActualContainer> = actual
        .iter()
        .map(|c| (c.runtime_name.as_str(), c))
        .collect();
    let mut readiness = HashMap::new();
    let mut restarts = Vec::new();

    for pod in desired {
        let Ok(run) = &pod.plan else { continue };
        if run.liveness_probe.is_none() && run.readiness_probe.is_none() {
            continue;
        }
        let Some(container) = by_name.get(run.runtime_name.as_str()) else {
            continue;
        };
        if !matches!(container.status, rk_runtime::container::Status::Running) {
            continue;
        }
        let Some(ip) = container.ip.as_deref().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let pid1 = container.pid1.unwrap_or_default();
        let age = container
            .created_at
            .map(|c| u64::try_from((chrono::Utc::now() - c).num_seconds()).unwrap_or(0))
            .unwrap_or(0);
        let entry = probes.entry(pod.uid.clone()).or_default();

        if let Some(p) = &run.readiness_probe {
            let state = entry
                .readiness
                .get_or_insert_with(|| ProbeState::new(ProbeKind::Readiness));
            if probe::is_due(state, now, age, p) {
                let ok = probe::execute(p, ip, pid1).await;
                probe::record(state, ProbeKind::Readiness, ok, now, p);
            }
            readiness.insert(pod.uid.clone(), state.ready);
        }
        if let Some(p) = &run.liveness_probe {
            let state = entry
                .liveness
                .get_or_insert_with(|| ProbeState::new(ProbeKind::Liveness));
            if probe::is_due(state, now, age, p) {
                let ok = probe::execute(p, ip, pid1).await;
                if probe::record(state, ProbeKind::Liveness, ok, now, p).restart {
                    restarts.push(run.runtime_name.clone());
                }
            }
        }
    }

    // Drop state for pods no longer desired.
    let live: std::collections::HashSet<&str> = desired.iter().map(|d| d.uid.as_str()).collect();
    probes.retain(|uid, _| live.contains(uid.as_str()));
    (readiness, restarts)
}

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
            pid1: row.pid1,
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
    dns_server: Option<Ipv4Addr>,
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
        dns_server,
        namespace: Some(pod.namespace.clone()),
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
