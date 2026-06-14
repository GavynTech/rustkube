//! Driving the VXLAN overlay from the cluster's Node list. The agent creates
//! its own VXLAN device once, advertises its VTEP (MAC) on its Node, then —
//! on every Node change — programs the route/neighbour/FDB entries for each
//! other node and tears down the ones for nodes that have left.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use rk_net::overlay::{self, Peer};
use rk_types::client::ApiClient;
use rk_types::client::reflector::{self, Cache};
use rk_types::{Node, NodeCondition};
use tokio::sync::Notify;

/// Annotation the agent stamps on its Node carrying the VXLAN device MAC, so
/// peers can target it in their FDB.
pub const VTEP_MAC_ANNOTATION: &str = "rustkube.io/vtep-mac";

/// One peer node distilled from its Node object, if it advertises everything
/// the overlay needs (a different node, a pod CIDR, an address, a VTEP MAC).
fn peer_of(node: &Node, self_name: &str) -> Option<Peer> {
    if node.metadata.name == self_name {
        return None;
    }
    // A node only carries traffic once it's Ready and fully advertised.
    if node.status.condition != NodeCondition::Ready {
        return None;
    }
    let pod_cidr = rk_net::parse_pod_cidr(node.spec.pod_cidr.as_deref()?).ok()?;
    let underlay_ip = match node.status.address? {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return None,
    };
    let mac = node
        .metadata
        .annotations
        .get(VTEP_MAC_ANNOTATION)
        .and_then(|text| overlay::parse_mac(text).ok())?;
    Some(Peer {
        vxlan_ip: rk_net::vtep_ip(pod_cidr),
        pod_cidr,
        mac,
        underlay_ip,
    })
}

/// Compute the peers the overlay should be programmed for, keyed by node name.
fn desired_peers(nodes: &[Node], self_name: &str) -> HashMap<String, Peer> {
    nodes
        .iter()
        .filter_map(|node| peer_of(node, self_name).map(|peer| (node.metadata.name.clone(), peer)))
        .collect()
}

/// Watch Nodes and keep the overlay's peer routes in sync forever. `vxlan_ip`
/// is this node's own VTEP IP (used only to skip self defensively).
pub async fn run(client: Arc<ApiClient>, self_name: String, _self_vxlan_ip: Ipv4Addr) {
    let nodes: Cache<Node> = Arc::default();
    let notify = Arc::new(Notify::new());
    tokio::spawn(reflector::reflect(client, nodes.clone(), notify.clone()));

    let mut programmed: HashMap<String, Peer> = HashMap::new();
    loop {
        notify.notified().await;
        let snapshot = reflector::snapshot(&nodes).await;
        let desired = desired_peers(&snapshot, &self_name);

        // Program new or changed peers.
        for (name, peer) in &desired {
            if programmed.get(name) != Some(peer) {
                match overlay::program_peer(peer).await {
                    Ok(()) => {
                        tracing::info!(peer = %name, cidr = %peer.pod_cidr, "overlay route programmed")
                    }
                    Err(err) => {
                        tracing::warn!(peer = %name, error = %err, "programming overlay peer failed");
                        continue;
                    }
                }
            }
        }
        // Tear down peers that have left or changed.
        for (name, peer) in &programmed {
            if desired.get(name) != Some(peer)
                && let Err(err) = overlay::unprogram_peer(peer).await
            {
                tracing::warn!(peer = %name, error = %err, "unprogramming overlay peer failed");
            }
        }
        programmed = desired;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rk_types::{NodeSpec, NodeStatus, ObjectMeta};

    use super::*;

    fn node(
        name: &str,
        cidr: Option<&str>,
        addr: Option<&str>,
        mac: Option<&str>,
        ready: bool,
    ) -> Node {
        let mut annotations = BTreeMap::new();
        if let Some(mac) = mac {
            annotations.insert(VTEP_MAC_ANNOTATION.to_owned(), mac.to_owned());
        }
        Node {
            api_version: "v1".to_owned(),
            kind: "Node".to_owned(),
            metadata: ObjectMeta {
                annotations,
                ..ObjectMeta::named(name)
            },
            spec: NodeSpec {
                pod_cidr: cidr.map(str::to_owned),
                unschedulable: false,
            },
            status: NodeStatus {
                condition: if ready {
                    NodeCondition::Ready
                } else {
                    NodeCondition::NotReady
                },
                address: addr.map(|a| a.parse().unwrap()),
                ..NodeStatus::default()
            },
        }
    }

    #[test]
    fn builds_peers_from_well_formed_other_nodes() {
        let nodes = [
            node(
                "self",
                Some("10.42.0.0/24"),
                Some("10.200.0.1"),
                Some("aa:bb:cc:dd:ee:01"),
                true,
            ),
            node(
                "peer",
                Some("10.42.1.0/24"),
                Some("10.200.0.2"),
                Some("aa:bb:cc:dd:ee:02"),
                true,
            ),
        ];
        let peers = desired_peers(&nodes, "self");
        assert_eq!(peers.len(), 1, "self is excluded");
        let peer = &peers["peer"];
        assert_eq!(peer.vxlan_ip.to_string(), "10.42.1.0");
        assert_eq!(peer.underlay_ip.to_string(), "10.200.0.2");
        assert_eq!(peer.mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x02]);
    }

    #[test]
    fn skips_nodes_missing_overlay_info_or_not_ready() {
        let nodes = [
            node(
                "no-mac",
                Some("10.42.1.0/24"),
                Some("10.200.0.2"),
                None,
                true,
            ),
            node(
                "no-cidr",
                None,
                Some("10.200.0.3"),
                Some("aa:bb:cc:dd:ee:03"),
                true,
            ),
            node(
                "no-addr",
                Some("10.42.2.0/24"),
                None,
                Some("aa:bb:cc:dd:ee:04"),
                true,
            ),
            node(
                "not-ready",
                Some("10.42.3.0/24"),
                Some("10.200.0.5"),
                Some("aa:bb:cc:dd:ee:05"),
                false,
            ),
        ];
        assert!(desired_peers(&nodes, "self").is_empty());
    }
}
