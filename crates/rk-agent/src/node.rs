//! Node registration and heartbeats. The agent creates (or refreshes) its
//! Node object at startup, then a background task re-asserts Ready with a
//! fresh heartbeat timestamp every interval. The node controller (Phase 5)
//! is what turns *missed* heartbeats into NotReady and rescheduling.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Context as _;
use rk_types::client::{ApiClient, ClientError};
use rk_types::quantity::ResourceList;
use rk_types::{Cpu, Memory, Node, NodeCondition, ObjectMeta};

/// CPU and memory this machine offers. Phase 3 reports
/// `allocatable == capacity`; the scheduler subtracts placed pods' requests
/// itself from Phase 4.
pub fn detect_capacity() -> anyhow::Result<ResourceList> {
    let cores = std::thread::available_parallelism()
        .context("detecting CPU count")?
        .get() as u64;
    let meminfo = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    let memory = parse_mem_total(&meminfo).context("parsing MemTotal from /proc/meminfo")?;
    Ok(ResourceList {
        cpu: Some(Cpu::from_cores(cores)),
        memory: Some(Memory::from_bytes(memory)),
    })
}

/// `MemTotal:       1628416 kB` -> bytes.
fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// First non-loopback, up interface address — good enough until multi-homed
/// nodes matter; `nodeIp` in the config overrides it.
pub fn detect_node_ip() -> Option<IpAddr> {
    let addrs = nix::ifaddrs::getifaddrs().ok()?;
    for ifaddr in addrs {
        if !ifaddr.flags.contains(nix::net::if_::InterfaceFlags::IFF_UP)
            || ifaddr
                .flags
                .contains(nix::net::if_::InterfaceFlags::IFF_LOOPBACK)
        {
            continue;
        }
        if let Some(sockaddr) = ifaddr.address
            && let Some(ipv4) = sockaddr.as_sockaddr_in()
        {
            return Some(IpAddr::V4(ipv4.ip()));
        }
    }
    None
}

/// Everything the agent advertises about its node, fixed for the process
/// lifetime — passed once to [`register`] and the [`heartbeat_loop`].
#[derive(Debug, Clone)]
pub struct Registration {
    pub name: String,
    pub address: Option<IpAddr>,
    pub capacity: ResourceList,
    pub labels: BTreeMap<String, String>,
    pub pod_cidr: Option<String>,
}

fn ready_status(node: &mut Node, reg: &Registration) {
    node.status.condition = NodeCondition::Ready;
    node.status.capacity = reg.capacity;
    node.status.allocatable = reg.capacity;
    node.status.address = reg.address;
    node.status.last_heartbeat = Some(chrono::Utc::now());
}

fn apply_identity(node: &mut Node, reg: &Registration) {
    for (key, value) in &reg.labels {
        node.metadata.labels.insert(key.clone(), value.clone());
    }
    node.metadata
        .labels
        .insert("rustkube.io/hostname".to_owned(), reg.name.clone());
    if let Some(cidr) = &reg.pod_cidr {
        node.spec.pod_cidr = Some(cidr.clone());
    }
}

/// Create the Node, or refresh it if a previous agent run left one behind.
pub async fn register(client: &ApiClient, reg: &Registration) -> anyhow::Result<()> {
    let existing: Option<Node> = client.get(None, &reg.name).await?;
    let mut node = existing.unwrap_or_else(|| Node {
        api_version: "v1".to_owned(),
        kind: "Node".to_owned(),
        metadata: ObjectMeta::named(&reg.name),
        spec: Default::default(),
        status: Default::default(),
    });
    apply_identity(&mut node, reg);
    ready_status(&mut node, reg);

    let result = if node.metadata.resource_version.is_some() {
        client.replace(&node).await
    } else {
        client.create(&node).await
    };
    match result {
        Ok(_) => {
            tracing::info!(node = %reg.name, "node registered");
            Ok(())
        }
        // Another registration won the race; the heartbeat loop refreshes it.
        Err(ClientError::Api { status: 409, .. }) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Re-assert Ready forever. Errors are logged and retried next tick; a
/// missing Node (someone deleted it) is re-registered.
pub async fn heartbeat_loop(client: Arc<ApiClient>, reg: Registration, interval_secs: u64) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let result: anyhow::Result<()> = async {
            match client.get::<Node>(None, &reg.name).await? {
                Some(mut node) => {
                    ready_status(&mut node, &reg);
                    match client.replace(&node).await {
                        Ok(_) => Ok(()),
                        // Lost a write race; the next tick wins or loses again.
                        Err(ClientError::Api { status: 409, .. }) => Ok(()),
                        Err(err) => Err(err.into()),
                    }
                }
                None => register(&client, &reg).await,
            }
        }
        .await;
        if let Err(err) = result {
            tracing::warn!(error = %format!("{err:#}"), "heartbeat failed; will retry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_total_parses_kb_to_bytes() {
        let meminfo = "MemTotal:       1628416 kB\nMemFree:         1183244 kB\n";
        assert_eq!(parse_mem_total(meminfo), Some(1_628_416 * 1024));
        assert_eq!(parse_mem_total("nonsense"), None);
    }

    #[test]
    fn capacity_detects_something_plausible() {
        let capacity = detect_capacity().unwrap();
        assert!(capacity.cpu.unwrap().as_millis() >= 1000);
        assert!(capacity.memory.unwrap().as_bytes() > 1 << 20);
    }
}
