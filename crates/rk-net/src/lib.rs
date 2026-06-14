//! Container networking for RustKube.
//!
//! The per-pod bridge + veth wiring lives in `rk-runtime` (it runs alongside
//! the container shim); this crate owns the node-level overlay: the VXLAN
//! device and the route/neighbour/FDB entries that carry pod traffic between
//! nodes ([`overlay`]). Services (nftables VIPs) and the embedded DNS server
//! land here in Phase 6B.

pub mod overlay;

use std::net::Ipv4Addr;

use ipnetwork::Ipv4Network;

/// Cluster-wide pod CIDR used when none is configured. /16 leaves room for
/// 256 per-node /24s.
pub const DEFAULT_POD_CIDR: &str = "10.42.0.0/16";

#[derive(Debug, Clone, thiserror::Error)]
pub enum NetError {
    #[error("invalid pod CIDR {cidr:?}: {reason}")]
    InvalidPodCidr { cidr: String, reason: String },

    #[error("netlink {op} failed: {detail}")]
    Netlink { op: &'static str, detail: String },
}

/// A node's VTEP overlay IP is the network address of its pod /24
/// (`10.42.1.0/24` → `10.42.1.0`). Deterministic, so peers derive it from the
/// node's advertised pod CIDR without extra coordination.
pub fn vtep_ip(pod_cidr: Ipv4Network) -> Ipv4Addr {
    pod_cidr.network()
}

/// Validate a cluster pod CIDR: IPv4 and wide enough to carve at least one
/// per-node /24 out of.
pub fn parse_pod_cidr(cidr: &str) -> Result<Ipv4Network, NetError> {
    let network: Ipv4Network =
        cidr.parse()
            .map_err(|err: ipnetwork::IpNetworkError| NetError::InvalidPodCidr {
                cidr: cidr.to_owned(),
                reason: err.to_string(),
            })?;
    if network.prefix() > 24 {
        return Err(NetError::InvalidPodCidr {
            cidr: cidr.to_owned(),
            reason: "prefix must be /24 or wider to fit at least one per-node /24".to_owned(),
        });
    }
    Ok(network)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pod_cidr_is_valid() {
        let network = parse_pod_cidr(DEFAULT_POD_CIDR).unwrap();
        assert_eq!(network.prefix(), 16);
    }

    #[test]
    fn too_narrow_and_garbage_cidrs_are_rejected() {
        assert!(parse_pod_cidr("10.42.0.0/25").is_err());
        assert!(parse_pod_cidr("not-a-cidr").is_err());
        assert!(parse_pod_cidr("10.42.0.0").is_err());
    }

    #[test]
    fn exact_slash_24_is_allowed() {
        assert!(parse_pod_cidr("10.42.7.0/24").is_ok());
    }
}
