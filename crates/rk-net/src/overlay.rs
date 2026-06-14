//! The VXLAN overlay that carries pod traffic between nodes (flannel's
//! model). Each node runs one VXLAN device, `rk-vxlan0`, in VNI 42 with
//! learning off — we program its forwarding database ourselves from the
//! cluster's Node list, so there's no flooding.
//!
//! For every *other* node the agent learns three things, all on `rk-vxlan0`:
//!  - a **route** `<peer /24> via <peer vxlan ip> onlink`, so off-node pod
//!    traffic leaves through the tunnel;
//!  - a **neighbour** mapping that peer vxlan IP to the peer's VTEP MAC
//!    (static ARP — no learning);
//!  - an **FDB** entry mapping that MAC to the peer's underlay address, so the
//!    kernel knows which real host to encapsulate toward.
//!
//! The local VTEP IP is the node's own pod-/24 network address (`10.42.0.0`
//! for the `10.42.0.0/24` node); the MAC is whatever the kernel assigns, read
//! back after creation and advertised on the node so peers can target it.

use std::net::{IpAddr, Ipv4Addr};

use futures_util::TryStreamExt as _;
use ipnetwork::Ipv4Network;
use rtnetlink::packet_route::neighbour::{NeighbourFlags, NeighbourState};
use rtnetlink::{Handle, LinkVxlan, RouteMessageBuilder};

use crate::NetError;

const VXLAN_NAME: &str = "rk-vxlan0";
const VNI: u32 = 42;
const VXLAN_PORT: u16 = 4789;

/// This node's tunnel endpoint, advertised on its Node object so peers can
/// reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vtep {
    /// VXLAN device MAC, e.g. `2a:1b:…`. Peers target it in their FDB.
    pub mac: String,
    /// The node's address on the underlay network (its real reachable IP).
    pub underlay_ip: Ipv4Addr,
    /// The VTEP's overlay IP — this node's pod-/24 network address.
    pub vxlan_ip: Ipv4Addr,
}

/// A peer node's tunnel endpoint, as learned from its Node object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub pod_cidr: Ipv4Network,
    pub vxlan_ip: Ipv4Addr,
    pub mac: [u8; 6],
    pub underlay_ip: Ipv4Addr,
}

fn netlink<E: std::fmt::Display>(op: &'static str, err: E) -> NetError {
    NetError::Netlink {
        op,
        detail: err.to_string(),
    }
}

async fn connect() -> Result<Handle, NetError> {
    let (connection, handle, _) =
        rtnetlink::new_connection().map_err(|err| netlink("opening netlink", err))?;
    tokio::spawn(connection);
    Ok(handle)
}

async fn link_index(handle: &Handle, name: &str) -> Option<u32> {
    handle
        .link()
        .get()
        .match_name(name.to_owned())
        .execute()
        .try_next()
        .await
        .ok()
        .flatten()
        .map(|link| link.header.index)
}

/// A stable VTEP MAC derived from the node's underlay IPv4: `2a:00:<a:b:c:d>`.
/// We set it explicitly (rather than read the kernel's random one) so the
/// device MAC always equals what's advertised to peers — a mismatch silently
/// black-holes return traffic. `0x2a` is locally-administered and unicast.
fn derive_mac(underlay: Ipv4Addr) -> [u8; 6] {
    let o = underlay.octets();
    [0x2a, 0x00, o[0], o[1], o[2], o[3]]
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse_mac(text: &str) -> Result<[u8; 6], NetError> {
    let parts: Vec<&str> = text.split(':').collect();
    let bad = || NetError::Netlink {
        op: "parsing VTEP MAC",
        detail: format!("not a 6-octet MAC: {text:?}"),
    };
    if parts.len() != 6 {
        return Err(bad());
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        *slot = u8::from_str_radix(part, 16).map_err(|_| bad())?;
    }
    Ok(mac)
}

/// Best-effort node sysctls for the overlay: forward IPv4 between the pod
/// bridge and the tunnel, and relax reverse-path filtering so packets that
/// arrive over the VXLAN aren't dropped as martians (overlay routing is
/// symmetric, but strict `rp_filter` defaults bite some hosts).
fn enable_ip_forwarding() {
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
    for key in ["all", "default"] {
        let _ = std::fs::write(format!("/proc/sys/net/ipv4/conf/{key}/rp_filter"), "0");
    }
}

/// Create (or find) `rk-vxlan0`, address it with the VTEP IP, bring it up,
/// and return this node's endpoint. Idempotent.
pub async fn ensure_vxlan(underlay_ip: Ipv4Addr, vxlan_ip: Ipv4Addr) -> Result<Vtep, NetError> {
    enable_ip_forwarding();
    let handle = connect().await?;
    let mac = derive_mac(underlay_ip);

    let index = match link_index(&handle, VXLAN_NAME).await {
        Some(index) => index,
        None => {
            handle
                .link()
                .add(
                    LinkVxlan::new(VXLAN_NAME, VNI)
                        .local(underlay_ip)
                        .port(VXLAN_PORT)
                        .learning(false)
                        .address(mac.to_vec())
                        .build(),
                )
                .execute()
                .await
                .map_err(|err| netlink("creating vxlan device", err))?;
            link_index(&handle, VXLAN_NAME)
                .await
                .ok_or_else(|| netlink("vxlan device", "not present after create"))?
        }
    };

    // Address (ignore EEXIST) and raise.
    let _ = handle
        .address()
        .add(index, IpAddr::V4(vxlan_ip), 32)
        .execute()
        .await;
    handle
        .link()
        .set(rtnetlink::LinkUnspec::new_with_index(index).up().build())
        .execute()
        .await
        .map_err(|err| netlink("raising vxlan device", err))?;

    Ok(Vtep {
        mac: format_mac(&mac),
        underlay_ip,
        vxlan_ip,
    })
}

/// Program the route + neighbour + FDB entries that reach one peer node.
/// Idempotent (existing entries are replaced or their EEXIST ignored).
pub async fn program_peer(peer: &Peer) -> Result<(), NetError> {
    let handle = connect().await?;
    let index = link_index(&handle, VXLAN_NAME)
        .await
        .ok_or_else(|| netlink("program peer", "rk-vxlan0 missing"))?;

    // Route: the peer's /24 via its VTEP IP, onlink (the gateway sits inside
    // the destination subnet). Re-adding an identical route is harmless.
    let route = RouteMessageBuilder::<Ipv4Addr>::new()
        .destination_prefix(peer.pod_cidr.network(), 24)
        .gateway(peer.vxlan_ip)
        .output_interface(index)
        .onlink()
        .build();
    let _ = handle.route().add(route).execute().await;

    // Static ARP: peer VTEP IP -> peer MAC.
    handle
        .neighbours()
        .add(index, IpAddr::V4(peer.vxlan_ip))
        .link_local_address(&peer.mac)
        .state(NeighbourState::Permanent)
        .replace()
        .execute()
        .await
        .map_err(|err| netlink("adding overlay neighbour", err))?;

    // FDB: peer MAC -> peer underlay address (which host to encapsulate to).
    handle
        .neighbours()
        .add_bridge(index, &peer.mac)
        .destination(IpAddr::V4(peer.underlay_ip))
        .flags(NeighbourFlags::Own)
        .replace()
        .execute()
        .await
        .map_err(|err| netlink("adding overlay fdb entry", err))?;
    Ok(())
}

/// Remove a peer's overlay entries (when its node leaves the cluster).
/// Best-effort: missing entries are not an error.
pub async fn unprogram_peer(peer: &Peer) -> Result<(), NetError> {
    let handle = connect().await?;
    let Some(index) = link_index(&handle, VXLAN_NAME).await else {
        return Ok(());
    };
    let route = RouteMessageBuilder::<Ipv4Addr>::new()
        .destination_prefix(peer.pod_cidr.network(), 24)
        .gateway(peer.vxlan_ip)
        .output_interface(index)
        .onlink()
        .build();
    let _ = handle.route().del(route).execute().await;
    // The neighbour/FDB entries are keyed by the device; deleting the route is
    // what stops traffic. Leftover neigh/fdb entries are inert without it.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_round_trips() {
        let mac = [0x2a, 0x1b, 0x0c, 0xde, 0xf0, 0x12];
        let text = format_mac(&mac);
        assert_eq!(text, "2a:1b:0c:de:f0:12");
        assert_eq!(parse_mac(&text).unwrap(), mac);
    }

    #[test]
    fn bad_macs_are_rejected() {
        for bad in ["", "2a:1b", "2a:1b:0c:de:f0:12:34", "zz:1b:0c:de:f0:12"] {
            assert!(parse_mac(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn derived_mac_encodes_the_underlay_and_is_stable() {
        let mac = derive_mac("10.200.0.1".parse().unwrap());
        assert_eq!(format_mac(&mac), "2a:00:0a:c8:00:01");
        // Locally-administered + unicast, and deterministic.
        assert_eq!(mac[0] & 0x03, 0x02);
        assert_eq!(derive_mac("10.200.0.1".parse().unwrap()), mac);
        assert_ne!(derive_mac("10.200.0.2".parse().unwrap()), mac);
    }
}
