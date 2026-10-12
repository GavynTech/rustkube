//! RustKube CLI. `apply`, `get` (with `--watch`), `describe` and `delete`
//! ride the shared API client in `rk_types::client`; `logs` needs the node
//! agent's log streaming (Phase 7), `scale` the controllers (Phase 5), `pki`
//! mints TLS material + a token offline and `drain`/`cordon` gate a node
//! (Phase 8) — no dead flags in the meantime.

mod output;
mod pki;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt as _;
use rk_types::client::ApiClient;
use rk_types::{EventType, ResourceKind};
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "rk-ctl", version, about = "RustKube CLI")]
struct Cli {
    /// API server base URL.
    #[arg(
        long,
        global = true,
        env = "RK_API_URL",
        default_value = "http://127.0.0.1:6443"
    )]
    api_url: String,
    /// File holding the bearer token to send to the API server.
    #[arg(long, global = true, env = "RK_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// PEM CA certificate that signed the API server's TLS cert.
    #[arg(long, global = true, env = "RK_CA_FILE")]
    ca_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply manifests: create objects that don't exist, update ones that do.
    Apply {
        /// Manifest file(s); `-` reads stdin. Multi-document YAML works.
        #[arg(short = 'f', long = "filename", required = true)]
        files: Vec<PathBuf>,
    },
    /// List resources, or show one by name.
    Get {
        /// Resource kind: `pods`, `pod`, `nodes`, `configmaps`, ...
        kind: String,
        /// Object name; omit to list.
        name: Option<String>,
        #[arg(short = 'n', long, global = true)]
        namespace: Option<String>,
        /// List across all namespaces.
        #[arg(short = 'A', long, conflicts_with = "namespace")]
        all_namespaces: bool,
        /// Output format (default: table).
        #[arg(short = 'o', long)]
        output: Option<OutputFormat>,
        /// After listing, keep printing changes as they happen.
        #[arg(short = 'w', long)]
        watch: bool,
    },
    /// Print one object as YAML.
    Describe {
        kind: String,
        name: String,
        #[arg(short = 'n', long)]
        namespace: Option<String>,
    },
    /// Delete an object.
    Delete {
        kind: String,
        name: String,
        #[arg(short = 'n', long)]
        namespace: Option<String>,
    },
    /// Print a pod's container logs (fetched from the node running it).
    Logs {
        pod: String,
        #[arg(short = 'n', long)]
        namespace: Option<String>,
    },
    /// Set the replica count of a Deployment or ReplicaSet.
    Scale {
        /// `deployment`/`deploy` or `replicaset`/`rs`.
        kind: String,
        name: String,
        #[arg(long)]
        replicas: u32,
        #[arg(short = 'n', long)]
        namespace: Option<String>,
    },
    /// Mark a node unschedulable; the scheduler places no new pods on it.
    Cordon { node: String },
    /// Reverse `cordon`, letting the node take pods again.
    Uncordon { node: String },
    /// Cordon a node and evict its pods so they reschedule elsewhere.
    Drain { node: String },
    /// Print the client version and, when reachable, the server version.
    Version,
    /// Generate TLS PKI (a CA + API server cert/key) and a bearer token.
    /// Offline — it never contacts the API server.
    Pki {
        #[command(subcommand)]
        command: PkiCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PkiCommand {
    /// Write ca.crt, ca.key, server.crt, server.key and token into a directory.
    Init {
        /// Output directory (created if missing).
        #[arg(long, default_value = "pki")]
        dir: PathBuf,
        /// DNS name or IP address the API server is reached at; repeatable.
        /// `127.0.0.1` and `localhost` are always included.
        #[arg(long = "api-name")]
        api_names: Vec<String>,
        /// Overwrite existing files instead of refusing.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Yaml,
    Json,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // `pki` is offline — handle it before building an API client.
    if let Command::Pki { command } = &cli.command {
        return match command {
            PkiCommand::Init {
                dir,
                api_names,
                force,
            } => pki::init(dir, api_names, *force),
        };
    }

    let client = ApiClient::connect(
        &cli.api_url,
        cli.token_file.as_deref(),
        cli.ca_file.as_deref(),
    )?;
    match cli.command {
        Command::Apply { files } => apply(&client, &files).await,
        Command::Get {
            kind,
            name,
            namespace,
            all_namespaces,
            output,
            watch,
        } => {
            get(
                &client,
                &kind,
                name.as_deref(),
                namespace.as_deref(),
                all_namespaces,
                output,
                watch,
            )
            .await
        }
        Command::Describe {
            kind,
            name,
            namespace,
        } => describe(&client, &kind, &name, namespace.as_deref()).await,
        Command::Delete {
            kind,
            name,
            namespace,
        } => delete(&client, &kind, &name, namespace.as_deref()).await,
        Command::Logs { pod, namespace } => logs(&client, &pod, namespace.as_deref()).await,
        Command::Scale {
            kind,
            name,
            replicas,
            namespace,
        } => scale(&client, &kind, &name, replicas, namespace.as_deref()).await,
        Command::Cordon { node } => set_cordon(&client, &node, true).await,
        Command::Uncordon { node } => set_cordon(&client, &node, false).await,
        Command::Drain { node } => drain(&client, &node).await,
        Command::Version => version(&client).await,
        Command::Pki { .. } => unreachable!("handled before the client is built"),
    }
}

/// Print one line, exiting quietly if the reader of our stdout has gone away
/// (e.g. `rk-ctl ... | head`) — a closed pipe is not an error for a CLI.
fn print_line(line: &str) {
    use std::io::Write as _;
    if writeln!(std::io::stdout(), "{line}").is_err() {
        std::process::exit(0);
    }
}

fn parse_kind(raw: &str) -> anyhow::Result<ResourceKind> {
    raw.parse().with_context(|| {
        format!(
            "unknown resource kind {raw:?} (one of: {})",
            ResourceKind::ALL.map(ResourceKind::plural).join(", ")
        )
    })
}

/// Pull (kind, namespace-for-API-calls, name) out of a raw manifest. The
/// namespace mirrors the server's defaulting so the follow-up item requests
/// hit the object the create/update landed on.
fn manifest_identity(object: &Value) -> anyhow::Result<(ResourceKind, Option<String>, String)> {
    let kind: ResourceKind = object
        .get("kind")
        .and_then(Value::as_str)
        .context("manifest is missing `kind`")?
        .parse()
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let name = object
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .context("manifest is missing `metadata.name`")?
        .to_owned();
    let namespace = kind.namespaced().then(|| {
        object
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned()
    });
    Ok((kind, namespace, name))
}

/// Parse a manifest stream into raw JSON values. Raw — not the typed model —
/// because apply must send exactly what the user wrote; the server validates.
fn parse_manifests(input: &str) -> anyhow::Result<Vec<Value>> {
    use serde::Deserialize as _;
    let mut objects = Vec::new();
    for document in serde_yaml::Deserializer::from_str(input) {
        let value = Value::deserialize(document).context("invalid YAML")?;
        if !value.is_null() {
            objects.push(value);
        }
    }
    Ok(objects)
}

async fn apply(client: &ApiClient, files: &[PathBuf]) -> anyhow::Result<()> {
    for file in files {
        let manifest = if file.as_os_str() == "-" {
            std::io::read_to_string(std::io::stdin()).context("reading stdin")?
        } else {
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?
        };
        let objects =
            parse_manifests(&manifest).with_context(|| format!("parsing {}", file.display()))?;
        if objects.is_empty() {
            bail!("{} contains no objects", file.display());
        }

        for mut object in objects {
            let (kind, namespace, name) = manifest_identity(&object)?;
            let current = client.get_raw(kind, namespace.as_deref(), &name).await?;
            match current {
                None => {
                    client.create_raw(kind, &object).await?;
                    print_line(&format!("{}/{name} created", kind.as_str().to_lowercase()));
                }
                Some(current) => {
                    // Replace at the observed version unless the manifest
                    // pins its own resourceVersion (then the server enforces
                    // that one).
                    let has_rv = object.pointer("/metadata/resourceVersion").is_some();
                    if !has_rv
                        && let Some(rv) = current.pointer("/metadata/resourceVersion")
                        && let Some(meta) =
                            object.get_mut("metadata").and_then(Value::as_object_mut)
                    {
                        meta.insert("resourceVersion".to_owned(), rv.clone());
                    }
                    client
                        .replace_raw(kind, namespace.as_deref(), &name, &object)
                        .await?;
                    print_line(&format!(
                        "{}/{name} configured",
                        kind.as_str().to_lowercase()
                    ));
                }
            }
        }
    }
    Ok(())
}

async fn get(
    client: &ApiClient,
    kind: &str,
    name: Option<&str>,
    namespace: Option<&str>,
    all_namespaces: bool,
    format: Option<OutputFormat>,
    watch: bool,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;

    // Scope of the request: a single namespace unless -A (or cluster kind).
    let effective_ns = if all_namespaces || !kind.namespaced() {
        None
    } else {
        Some(namespace.unwrap_or("default"))
    };
    let wide_ns = kind.namespaced() && all_namespaces;

    if let Some(name) = name {
        let object = client
            .get_raw(kind, effective_ns, name)
            .await?
            .with_context(|| format!("{kind} {name:?} not found"))?;
        match format {
            Some(OutputFormat::Yaml) => print_line(serde_yaml::to_string(&object)?.trim_end()),
            Some(OutputFormat::Json) => print_line(&serde_json::to_string_pretty(&object)?),
            None => {
                let now = chrono::Utc::now();
                print_line(&output::render_table(
                    &output::headers(kind, false),
                    &[output::row(kind, &object, false, now)],
                ));
            }
        }
        return Ok(());
    }

    let list = client.list_raw(kind, effective_ns).await?;
    match format {
        Some(OutputFormat::Yaml) => print_line(serde_yaml::to_string(&list.items)?.trim_end()),
        Some(OutputFormat::Json) => print_line(&serde_json::to_string_pretty(&list.items)?),
        None => {
            let now = chrono::Utc::now();
            if list.items.is_empty() && !watch {
                print_line(&format!("No {} found", kind.plural()));
                return Ok(());
            }
            let rows: Vec<Vec<String>> = list
                .items
                .iter()
                .map(|item| output::row(kind, item, wide_ns, now))
                .collect();
            print_line(&output::render_table(
                &output::headers(kind, wide_ns),
                &rows,
            ));
        }
    }

    if watch {
        let since = list.resource_version.unwrap_or(0);
        let mut stream = client.watch_raw(kind, effective_ns, since).await?;
        while let Some(item) = stream.next().await {
            let event = item?;
            let now = chrono::Utc::now();
            let mut row = output::row(kind, &event.object, wide_ns, now);
            let tag = match event.event_type {
                EventType::Added => "ADDED",
                EventType::Modified => "MODIFIED",
                EventType::Deleted => "DELETED",
            };
            row.insert(0, tag.to_owned());
            print_line(&row.join("   "));
        }
        bail!("watch stream ended (the server cut a lagging watch); re-run to resume");
    }
    Ok(())
}

async fn describe(
    client: &ApiClient,
    kind: &str,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;
    let ns = kind.namespaced().then(|| namespace.unwrap_or("default"));
    let object = client
        .get_raw(kind, ns, name)
        .await?
        .with_context(|| format!("{kind} {name:?} not found"))?;
    print_line(&format!(
        "# {kind} {}{name}",
        ns.map(|n| format!("{n}/")).unwrap_or_default()
    ));
    print_line(serde_yaml::to_string(&object)?.trim_end());
    Ok(())
}

async fn delete(
    client: &ApiClient,
    kind: &str,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;
    let ns = kind.namespaced().then(|| namespace.unwrap_or("default"));
    client.delete_raw(kind, ns, name).await?;
    print_line(&format!(
        "{} \"{name}\" deleted",
        kind.as_str().to_lowercase()
    ));
    Ok(())
}

async fn scale(
    client: &ApiClient,
    kind: &str,
    name: &str,
    replicas: u32,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;
    if !matches!(kind, ResourceKind::Deployment | ResourceKind::ReplicaSet) {
        bail!("can only scale Deployments and ReplicaSets, not {kind}");
    }
    let ns = namespace.unwrap_or("default");
    // Read-modify-write the existing object at its current resourceVersion;
    // the server enforces the optimistic-concurrency check.
    let mut object = client
        .get_raw(kind, Some(ns), name)
        .await?
        .with_context(|| format!("{kind} {name:?} not found"))?;
    object["spec"]["replicas"] = replicas.into();
    client.replace_raw(kind, Some(ns), name, &object).await?;
    print_line(&format!(
        "{} \"{name}\" scaled to {replicas}",
        kind.as_str().to_lowercase()
    ));
    Ok(())
}

/// Set a node's `spec.unschedulable`; the scheduler's filter already honors it.
async fn set_cordon(client: &ApiClient, node: &str, unschedulable: bool) -> anyhow::Result<()> {
    let mut obj = client
        .get_raw(ResourceKind::Node, None, node)
        .await?
        .with_context(|| format!("node {node:?} not found"))?;
    obj.as_object_mut()
        .context("node object is malformed")?
        .entry("spec")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("node spec is malformed")?
        .insert("unschedulable".to_owned(), unschedulable.into());
    client
        .replace_raw(ResourceKind::Node, None, node, &obj)
        .await?;
    print_line(&format!(
        "node \"{node}\" {}",
        if unschedulable {
            "cordoned"
        } else {
            "uncordoned"
        }
    ));
    Ok(())
}

/// Cordon `node`, then delete every pod scheduled to it. The controllers
/// recreate managed pods, and since the node is now unschedulable they land on
/// other nodes; bare pods are simply removed.
async fn drain(client: &ApiClient, node: &str) -> anyhow::Result<()> {
    set_cordon(client, node, true).await?;
    let pods = client.list_raw(ResourceKind::Pod, None).await?;
    let on_node: Vec<&Value> = pods
        .items
        .iter()
        .filter(|pod| pod.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node))
        .collect();
    if on_node.is_empty() {
        print_line(&format!("node \"{node}\" drained (no pods to evict)"));
        return Ok(());
    }
    for pod in &on_node {
        let name = pod
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ns = pod
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let typed: rk_types::Pod = serde_json::from_value((*pod).clone())?;
        if typed
            .metadata
            .owner_references
            .iter()
            .any(|o| o.kind == ResourceKind::Job)
        {
            if !matches!(
                typed.status.phase,
                rk_types::PodPhase::Succeeded | rk_types::PodPhase::Failed
            ) {
                let mut evicted = typed;
                evicted.status.phase = rk_types::PodPhase::Failed;
                evicted.status.message = Some(rk_types::job::EVICTED_ATTEMPT_MESSAGE.into());
                client.replace(&evicted).await?;
            }
        } else {
            client.delete_raw(ResourceKind::Pod, Some(ns), name).await?;
        }
        print_line(&format!("evicting pod {ns}/{name}"));
    }
    print_line(&format!(
        "node \"{node}\" drained ({} pod(s) evicted)",
        on_node.len()
    ));
    Ok(())
}

/// Fetch a pod's logs from the node running it: resolve pod → nodeName →
/// node address, then GET the node agent's log endpoint.
async fn logs(client: &ApiClient, pod: &str, namespace: Option<&str>) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let ns = namespace.unwrap_or("default");
    let pod_obj = client
        .get_raw(ResourceKind::Pod, Some(ns), pod)
        .await?
        .with_context(|| format!("pod {pod:?} not found"))?;
    let node_name = pod_obj
        .pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .context("pod is not scheduled to a node yet (no logs)")?;
    let node = client
        .get_raw(ResourceKind::Node, None, node_name)
        .await?
        .with_context(|| format!("node {node_name:?} not found"))?;
    let address = node
        .pointer("/status/address")
        .and_then(Value::as_str)
        .with_context(|| format!("node {node_name:?} has no address"))?;

    // Minimal HTTP/1.0 GET to the node's log endpoint (port 10250).
    let mut stream = tokio::net::TcpStream::connect((address, 10250u16))
        .await
        .with_context(|| format!("connecting to node {node_name} ({address}:10250)"))?;
    let request =
        format!("GET /logs/{ns}/{pod} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;

    // Split off the headers; print the body.
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    if text.starts_with("HTTP/1.0 404") || text.starts_with("HTTP/1.1 404") {
        bail!("no logs available for {pod} (is it running?)");
    }
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(body.as_bytes());
    Ok(())
}

async fn version(client: &ApiClient) -> anyhow::Result<()> {
    print_line(&format!("client: rk-ctl v{}", env!("CARGO_PKG_VERSION")));
    match client.server_version().await {
        Ok(info) => print_line(&format!(
            "server: {} v{}",
            info["component"].as_str().unwrap_or("<unknown>"),
            info["version"].as_str().unwrap_or("<unknown>"),
        )),
        Err(_) => print_line(&format!("server: unreachable at {}", client.base_url())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_parse_and_identify() {
        let objects = parse_manifests(
            "kind: Pod\nmetadata:\n  name: web-0\nspec:\n  containers: []\n---\n\
             kind: Node\nmetadata:\n  name: worker-1\n---\n",
        )
        .unwrap();
        assert_eq!(objects.len(), 2);

        let (kind, ns, name) = manifest_identity(&objects[0]).unwrap();
        assert_eq!(kind, ResourceKind::Pod);
        assert_eq!(
            ns.as_deref(),
            Some("default"),
            "namespace defaults like the server"
        );
        assert_eq!(name, "web-0");

        let (kind, ns, _) = manifest_identity(&objects[1]).unwrap();
        assert_eq!(kind, ResourceKind::Node);
        assert_eq!(ns, None, "cluster-scoped kinds carry no namespace");
    }

    #[test]
    fn missing_kind_or_name_is_a_clear_error() {
        let err = manifest_identity(&serde_json::json!({"metadata": {"name": "x"}})).unwrap_err();
        assert!(err.to_string().contains("kind"));
        let err = manifest_identity(&serde_json::json!({"kind": "Pod"})).unwrap_err();
        assert!(err.to_string().contains("metadata.name"));
    }

    #[test]
    fn unknown_kind_lists_the_valid_ones() {
        let err = parse_kind("gizmo").unwrap_err();
        assert!(format!("{err:#}").contains("pods"));
    }
}
