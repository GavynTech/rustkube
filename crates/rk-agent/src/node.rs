//! Node registration and heartbeats. The agent creates (or refreshes) its
//! Node object at startup, then a background task re-asserts Ready with a
//! fresh heartbeat timestamp every interval. The node controller (Phase 5)
//! is what turns *missed* heartbeats into NotReady and rescheduling.

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

fn ready_status(node: &mut Node, address: Option<IpAddr>, capacity: &ResourceList) {
    node.status.condition = NodeCondition::Ready;
    node.status.capacity = *capacity;
    node.status.allocatable = *capacity;
    node.status.address = address;
    node.status.last_heartbeat = Some(chrono::Utc::now());
}

/// Create the Node, or refresh it if a previous agent run left one behind.
pub async fn register(
    client: &ApiClient,
    name: &str,
    address: Option<IpAddr>,
    capacity: &ResourceList,
) -> anyhow::Result<()> {
    let existing: Option<Node> = client.get(None, name).await?;
    let mut node = existing.unwrap_or_else(|| Node {
        api_version: "v1".to_owned(),
        kind: "Node".to_owned(),
        metadata: ObjectMeta::named(name),
        spec: Default::default(),
        status: Default::default(),
    });
    node.metadata
        .labels
        .insert("rustkube.io/hostname".to_owned(), name.to_owned());
    ready_status(&mut node, address, capacity);

    let result = if node.metadata.resource_version.is_some() {
        client.replace(&node).await
    } else {
        client.create(&node).await
    };
    match result {
        Ok(_) => {
            tracing::info!(node = name, "node registered");
            Ok(())
        }
        // Another registration won the race; the heartbeat loop refreshes it.
        Err(ClientError::Api { status: 409, .. }) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Re-assert Ready forever. Errors are logged and retried next tick; a
/// missing Node (someone deleted it) is re-registered.
pub async fn heartbeat_loop(
    client: Arc<ApiClient>,
    name: String,
    address: Option<IpAddr>,
    capacity: ResourceList,
    interval_secs: u64,
) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let result: anyhow::Result<()> = async {
            match client.get::<Node>(None, &name).await? {
                Some(mut node) => {
                    ready_status(&mut node, address, &capacity);
                    match client.replace(&node).await {
                        Ok(_) => Ok(()),
                        // Lost a write race; the next tick wins or loses again.
                        Err(ClientError::Api { status: 409, .. }) => Ok(()),
                        Err(err) => Err(err.into()),
                    }
                }
                None => register(&client, &name, address, &capacity).await,
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
