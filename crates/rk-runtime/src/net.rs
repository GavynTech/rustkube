//! Phase 1 networking: one veth pair per container, host end in the host
//! namespace, peer renamed `eth0` inside the container's netns.
//!
//! Each container gets a /30 carved from this data dir's pod /24 (host = .1
//! of the block, container = .2), so the node curls the container IP over the
//! connected route on the veth — no bridge, no NAT, no DNS yet. The /24 base
//! defaults to `10.42.0.0/24` (Phase 1 `rkrt`) and is set per node by the
//! agent ([`configure_pod_cidr`]) from Phase 4, so simulated nodes don't
//! collide. The real cross-node pod network (bridge + VXLAN + service VIPs)
//! replaces this in Phase 6; outbound container traffic is out of scope here.
//!
//! Configuring the inside of the netns uses the standard setns trick: a
//! scratch thread joins the container's network namespace (threads carry
//! their own netns), then runs a current-thread tokio runtime so rtnetlink
//! sockets created there belong to the container.

use std::fs::File;
use std::net::Ipv4Addr;
use std::os::fd::AsFd;
use std::path::Path;

use futures_util::TryStreamExt as _;
use serde::{Deserialize, Serialize};

use crate::error::{Result, RuntimeError};
use crate::store::Store;

/// Default /24 the /30 blocks are carved from when no pod CIDR is configured.
const POD_NET_BASE: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 0);
/// 64 /30 blocks fit in the /24; block 0 is kept aside so .0/.1 never collide
/// with anything that assumes a network address.
const MAX_BLOCKS: u32 = 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PodNet {
    pub block: u32,
    pub host_ip: Ipv4Addr,
    pub container_ip: Ipv4Addr,
}

/// `(host, container)` addresses of a /30 block within `base`: net + 1 and + 2.
fn block_ips(base: Ipv4Addr, block: u32) -> (Ipv4Addr, Ipv4Addr) {
    let net = u32::from(base) + block * 4;
    (Ipv4Addr::from(net + 1), Ipv4Addr::from(net + 2))
}

fn pod_net(base: Ipv4Addr, block: u32) -> PodNet {
    let (host_ip, container_ip) = block_ips(base, block);
    PodNet {
        block,
        host_ip,
        container_ip,
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IpamState {
    /// The /24 network address pods are carved from; `None` means the
    /// 10.42.0.0/24 default (preserves `rkrt`'s Phase 1 behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<Ipv4Addr>,
    next: u32,
    free: Vec<u32>,
}

fn base_of(state: &IpamState) -> Ipv4Addr {
    state.base.unwrap_or(POD_NET_BASE)
}

fn read_ipam(store: &Store) -> Result<IpamState> {
    let path = store.ipam_path();
    if !path.exists() {
        return Ok(IpamState::default());
    }
    let raw = std::fs::read(&path)
        .map_err(|source| RuntimeError::io("reading ipam state", &path, source))?;
    serde_json::from_slice(&raw).map_err(|source| RuntimeError::json("ipam state", &path, source))
}

fn write_ipam(store: &Store, state: &IpamState) -> Result<()> {
    let path = store.ipam_path();
    let raw = serde_json::to_vec_pretty(state)
        .map_err(|source| RuntimeError::json("ipam state", &path, source))?;
    std::fs::write(&path, raw)
        .map_err(|source| RuntimeError::io("writing ipam state", &path, source))
}

/// Pin the /24 this data dir allocates pod IPs from (e.g. `10.42.1.0/24`).
/// Called once at node-agent startup so simulated nodes don't alias each
/// other's pod IPs. Idempotent for the same CIDR; changing it once blocks
/// are live is refused (it would re-home addresses already in use). `rkrt`
/// never calls this and keeps the 10.42.0.0/24 default.
pub fn configure_pod_cidr(store: &Store, cidr: &str) -> Result<()> {
    let base = parse_slash24(cidr)?;
    store.ensure_layout()?;
    let mut state = read_ipam(store)?;
    if base_of(&state) == base {
        state.base = Some(base); // pin it even if it equalled the default
        return write_ipam(store, &state);
    }
    if state.next > 0 || !state.free.is_empty() {
        return Err(RuntimeError::BadPodCidr {
            cidr: cidr.to_owned(),
            reason: "data dir already has pod IP allocations under a different CIDR",
        });
    }
    state.base = Some(base);
    write_ipam(store, &state)
}

/// Parse `a.b.c.d/24` to its /24 network address. Only /24 is supported
/// (each node owns exactly one /24, 63 usable /30 pod blocks).
fn parse_slash24(cidr: &str) -> Result<Ipv4Addr> {
    let bad = |reason| RuntimeError::BadPodCidr {
        cidr: cidr.to_owned(),
        reason,
    };
    let (addr, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| bad("expected a.b.c.d/24"))?;
    let addr: Ipv4Addr = addr.parse().map_err(|_| bad("invalid IPv4 address"))?;
    if prefix.trim() != "24" {
        return Err(bad("only /24 pod CIDRs are supported"));
    }
    // Mask to the /24 network address so `10.42.1.0/24` and `10.42.1.7/24`
    // both normalize to 10.42.1.0.
    Ok(Ipv4Addr::from(u32::from(addr) & 0xFFFF_FF00))
}

/// Hand out the lowest free /30 block from this data dir's /24. Freed blocks
/// are reused before new ones are cut. Single-writer by construction (one
/// agent owns one data dir).
pub fn allocate(store: &Store) -> Result<PodNet> {
    let mut state = read_ipam(store)?;
    let base = base_of(&state);
    let block = if let Some(block) = state.free.pop() {
        block
    } else {
        // Block numbering starts at 1; see MAX_BLOCKS.
        if state.next + 1 > MAX_BLOCKS {
            return Err(RuntimeError::PoolExhausted(MAX_BLOCKS));
        }
        state.next += 1;
        state.next
    };
    write_ipam(store, &state)?;
    Ok(pod_net(base, block))
}

pub fn release(store: &Store, block: u32) -> Result<()> {
    let mut state = read_ipam(store)?;
    if block <= state.next && !state.free.contains(&block) {
        state.free.push(block);
        write_ipam(store, &state)?;
    }
    Ok(())
}

/// Interface names from the container id — kernel limit is 15 bytes, so the
/// id contributes its first 8 hex chars: `rk1a2b3c4d` / `rkp1a2b3c4d`.
pub fn veth_names(id: &str) -> (String, String) {
    let short: String = id.chars().take(8).collect();
    (format!("rk{short}"), format!("rkp{short}"))
}

async fn link_index(handle: &rtnetlink::Handle, name: &str) -> Result<u32> {
    let link = handle
        .link()
        .get()
        .match_name(name.to_owned())
        .execute()
        .try_next()
        .await
        .map_err(|err| RuntimeError::net("looking up link", err.to_string()))?
        .ok_or_else(|| RuntimeError::net("looking up link", format!("{name} not found")))?;
    Ok(link.header.index)
}

/// Host-side half: create the veth pair, push the peer into the shim's netns,
/// then address and raise the host end.
async fn setup_host_side(
    shim_pid: i32,
    net: PodNet,
    host_name: &str,
    peer_name: &str,
) -> Result<()> {
    let (connection, handle, _) = rtnetlink::new_connection()
        .map_err(|err| RuntimeError::net("opening netlink", err.to_string()))?;
    tokio::spawn(connection);

    handle
        .link()
        .add(rtnetlink::LinkVeth::new(host_name, peer_name).build())
        .execute()
        .await
        .map_err(|err| RuntimeError::net("creating veth pair", err.to_string()))?;

    // From here the pair exists; failures must take it down again or every
    // failed start leaks a dangling host-namespace veth pair.
    let result = async {
        let peer = link_index(&handle, peer_name).await?;
        handle
            .link()
            .set(
                rtnetlink::LinkUnspec::new_with_index(peer)
                    .setns_by_pid(shim_pid as u32)
                    .build(),
            )
            .execute()
            .await
            .map_err(|err| RuntimeError::net("moving peer into netns", err.to_string()))?;

        let host = link_index(&handle, host_name).await?;
        handle
            .address()
            .add(host, net.host_ip.into(), 30)
            .execute()
            .await
            .map_err(|err| RuntimeError::net("addressing host veth", err.to_string()))?;
        handle
            .link()
            .set(rtnetlink::LinkUnspec::new_with_index(host).up().build())
            .execute()
            .await
            .map_err(|err| RuntimeError::net("raising host veth", err.to_string()))?;
        Ok(())
    }
    .await;
    if result.is_err() {
        delete_link_quietly(&handle, host_name).await;
    }
    result
}

/// Best-effort delete of one veth end (the kernel removes the pair). Used on
/// error paths only — the primary teardown is the netns dying.
async fn delete_link_quietly(handle: &rtnetlink::Handle, name: &str) {
    if let Ok(index) = link_index(handle, name).await
        && let Err(err) = handle.link().del(index).execute().await
    {
        tracing::warn!(link = name, error = %err, "failed to delete leftover veth");
    }
}

/// Container-side half, run on a thread that has joined the shim's netns:
/// lo up, peer renamed to eth0, addressed, raised, default route via the
/// host end.
async fn setup_container_side(net: PodNet, peer_name: &str) -> Result<()> {
    let (connection, handle, _) = rtnetlink::new_connection()
        .map_err(|err| RuntimeError::net("opening netlink in netns", err.to_string()))?;
    tokio::spawn(connection);

    let lo = link_index(&handle, "lo").await?;
    handle
        .link()
        .set(rtnetlink::LinkUnspec::new_with_index(lo).up().build())
        .execute()
        .await
        .map_err(|err| RuntimeError::net("raising lo", err.to_string()))?;

    let eth = link_index(&handle, peer_name).await?;
    handle
        .link()
        .set(
            rtnetlink::LinkUnspec::new_with_index(eth)
                .name("eth0".to_owned())
                .build(),
        )
        .execute()
        .await
        .map_err(|err| RuntimeError::net("renaming peer to eth0", err.to_string()))?;
    handle
        .address()
        .add(eth, net.container_ip.into(), 30)
        .execute()
        .await
        .map_err(|err| RuntimeError::net("addressing eth0", err.to_string()))?;
    handle
        .link()
        .set(rtnetlink::LinkUnspec::new_with_index(eth).up().build())
        .execute()
        .await
        .map_err(|err| RuntimeError::net("raising eth0", err.to_string()))?;

    // No destination prefix means 0.0.0.0/0 — the default route.
    let default_route = rtnetlink::RouteMessageBuilder::<Ipv4Addr>::new()
        .gateway(net.host_ip)
        .build();
    handle
        .route()
        .add(default_route)
        .execute()
        .await
        .map_err(|err| RuntimeError::net("adding default route", err.to_string()))?;
    Ok(())
}

/// Wire the container's network. The shim must already exist (its pid names
/// the netns via /proc) and must not have exec'd the workload yet — the
/// parent holds the sync pipe until this returns.
pub async fn setup(shim_pid: i32, container_id: &str, net: PodNet) -> Result<()> {
    let (host_name, peer_name) = veth_names(container_id);
    setup_host_side(shim_pid, net, &host_name, &peer_name).await?;

    // Joining a netns is per-thread, so a scratch OS thread (with its own
    // single-thread runtime) does the inside work without disturbing the
    // caller's runtime threads.
    let ns_path = format!("/proc/{shim_pid}/ns/net");
    let peer = peer_name.clone();
    let joined = std::thread::spawn(move || -> Result<()> {
        let ns = File::open(Path::new(&ns_path))
            .map_err(|source| RuntimeError::io("opening container netns", &ns_path, source))?;
        nix::sched::setns(ns.as_fd(), nix::sched::CloneFlags::CLONE_NEWNET)
            .map_err(|source| RuntimeError::sys("setns(CLONE_NEWNET)", source))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|source| RuntimeError::net("building netns runtime", source.to_string()))?;
        runtime.block_on(setup_container_side(net, &peer))
    })
    .join();

    match joined {
        Ok(result) => result,
        Err(_) => Err(RuntimeError::net(
            "container-side setup",
            "netns thread panicked",
        )),
    }
}

/// The veth pair dies with the netns when the last container process exits,
/// so teardown only needs to return the address block.
pub fn teardown(store: &Store, block: u32) -> Result<()> {
    release(store, block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_map_to_host_and_container_ips() {
        let cases = [
            (1, "10.42.0.5", "10.42.0.6"),
            (2, "10.42.0.9", "10.42.0.10"),
            (62, "10.42.0.249", "10.42.0.250"),
        ];
        for (block, host, container) in cases {
            let net = pod_net(POD_NET_BASE, block);
            assert_eq!(net.host_ip.to_string(), host, "block {block}");
            assert_eq!(net.container_ip.to_string(), container, "block {block}");
        }
    }

    #[test]
    fn pod_cidr_shifts_the_whole_24() {
        let base = parse_slash24("10.42.7.0/24").unwrap();
        let net = pod_net(base, 1);
        assert_eq!(net.host_ip.to_string(), "10.42.7.5");
        assert_eq!(net.container_ip.to_string(), "10.42.7.6");
    }

    #[test]
    fn parse_slash24_normalizes_and_validates() {
        assert_eq!(
            parse_slash24("10.42.3.0/24").unwrap(),
            Ipv4Addr::new(10, 42, 3, 0)
        );
        // Host bits are masked off.
        assert_eq!(
            parse_slash24("10.42.3.77/24").unwrap(),
            Ipv4Addr::new(10, 42, 3, 0)
        );
        for bad in [
            "10.42.3.0",
            "10.42.3.0/16",
            "not-an-ip/24",
            "10.42.3.0/24/8",
        ] {
            assert!(parse_slash24(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn configure_pod_cidr_pins_then_allocates_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();

        configure_pod_cidr(&store, "10.42.5.0/24").unwrap();
        let net = allocate(&store).unwrap();
        assert_eq!(net.container_ip.to_string(), "10.42.5.6");

        // Re-pinning the same CIDR is fine; a different one with live blocks is not.
        configure_pod_cidr(&store, "10.42.5.0/24").unwrap();
        assert!(configure_pod_cidr(&store, "10.42.9.0/24").is_err());
    }

    #[test]
    fn allocate_reuses_released_blocks_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();

        let first = allocate(&store).unwrap();
        let second = allocate(&store).unwrap();
        assert_eq!(first.block, 1);
        assert_eq!(second.block, 2);

        release(&store, first.block).unwrap();
        let third = allocate(&store).unwrap();
        assert_eq!(third.block, first.block, "freed block is reused");
        let fourth = allocate(&store).unwrap();
        assert_eq!(fourth.block, 3);
    }

    #[test]
    fn double_release_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();

        let net = allocate(&store).unwrap();
        release(&store, net.block).unwrap();
        release(&store, net.block).unwrap();
        assert_eq!(allocate(&store).unwrap().block, net.block);
        assert_eq!(allocate(&store).unwrap().block, 2);
    }

    #[test]
    fn veth_names_fit_the_kernel_limit() {
        let (host, peer) = veth_names("4f9d2c81aa55");
        assert_eq!(host, "rk4f9d2c81");
        assert_eq!(peer, "rkp4f9d2c81");
        assert!(host.len() <= 15 && peer.len() <= 15);
    }
}
