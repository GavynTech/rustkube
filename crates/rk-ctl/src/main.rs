//! RustKube CLI. `apply`, `get` (with `--watch`), `describe` and `delete`
//! ride the shared API client in `rk_types::client`; `logs` needs the node
//! agent's log streaming (Phase 7), `scale` the controllers (Phase 5),
//! `drain` Phase 8 — no dead flags in the meantime.

mod output;

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
    /// Print the client version and, when reachable, the server version.
    Version,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Yaml,
    Json,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Apply { files } => apply(&cli.api_url, &files).await,
        Command::Get {
            kind,
            name,
            namespace,
            all_namespaces,
            output,
            watch,
        } => {
            get(
                &cli.api_url,
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
        } => describe(&cli.api_url, &kind, &name, namespace.as_deref()).await,
        Command::Delete {
            kind,
            name,
            namespace,
        } => delete(&cli.api_url, &kind, &name, namespace.as_deref()).await,
        Command::Version => version(&cli.api_url).await,
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

async fn apply(api_url: &str, files: &[PathBuf]) -> anyhow::Result<()> {
    let client = ApiClient::new(api_url)?;
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
    api_url: &str,
    kind: &str,
    name: Option<&str>,
    namespace: Option<&str>,
    all_namespaces: bool,
    format: Option<OutputFormat>,
    watch: bool,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;
    let client = ApiClient::new(api_url)?;

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
    api_url: &str,
    kind: &str,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;
    let client = ApiClient::new(api_url)?;
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
    api_url: &str,
    kind: &str,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind)?;
    let client = ApiClient::new(api_url)?;
    let ns = kind.namespaced().then(|| namespace.unwrap_or("default"));
    client.delete_raw(kind, ns, name).await?;
    print_line(&format!(
        "{} \"{name}\" deleted",
        kind.as_str().to_lowercase()
    ));
    Ok(())
}

async fn version(api_url: &str) -> anyhow::Result<()> {
    print_line(&format!("client: rk-ctl v{}", env!("CARGO_PKG_VERSION")));
    let client = ApiClient::new(api_url)?;
    match client.server_version().await {
        Ok(info) => print_line(&format!(
            "server: {} v{}",
            info["component"].as_str().unwrap_or("<unknown>"),
            info["version"].as_str().unwrap_or("<unknown>"),
        )),
        Err(_) => print_line(&format!("server: unreachable at {api_url}")),
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
