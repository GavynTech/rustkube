//! Service IP allocation at admission: a ClusterIP from the service CIDR and,
//! for NodePort services, a node port per service port. The allocation scans
//! the live services for what's in use — fine at this scale, and a single
//! writer (the API server) so there's no race.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use rk_types::ResourceKind;
use serde_json::Value;

use crate::error::ApiError;
use crate::resources::AppState;

/// Cluster IPs come from 10.96.0.0/16; the first nine are reserved (kube
/// reserves the low addresses), so allocation starts at .10.
const SERVICE_CIDR_BASE: u32 = u32::from_be_bytes([10, 96, 0, 0]);
const SERVICE_CIDR_FIRST: u32 = SERVICE_CIDR_BASE + 10;
const SERVICE_CIDR_LAST: u32 = SERVICE_CIDR_BASE + 0xFFFF - 1;
const NODE_PORT_FIRST: u16 = 30000;
const NODE_PORT_LAST: u16 = 32767;

fn pick_cluster_ip(used: &HashSet<Ipv4Addr>) -> Option<Ipv4Addr> {
    (SERVICE_CIDR_FIRST..=SERVICE_CIDR_LAST)
        .map(Ipv4Addr::from_bits)
        .find(|ip| !used.contains(ip))
}

fn pick_node_port(used: &HashSet<u16>) -> Option<u16> {
    (NODE_PORT_FIRST..=NODE_PORT_LAST).find(|port| !used.contains(port))
}

/// What's already taken across all existing services.
#[derive(Default)]
struct Used {
    cluster_ips: HashSet<Ipv4Addr>,
    node_ports: HashSet<u16>,
}

fn collect_used(services: &[Value]) -> Used {
    let mut used = Used::default();
    for service in services {
        if let Some(ip) = service
            .pointer("/spec/clusterIP")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
        {
            used.cluster_ips.insert(ip);
        }
        if let Some(ports) = service.pointer("/spec/ports").and_then(Value::as_array) {
            for port in ports {
                if let Some(np) = port.get("nodePort").and_then(Value::as_u64) {
                    used.node_ports.insert(np as u16);
                }
            }
        }
    }
    used
}

/// Fill in `spec.clusterIP` (if unset) and, for NodePort services, each
/// port's `nodePort` (if unset). Mutates `data` in place.
pub async fn allocate(state: &AppState, data: &mut Value) -> Result<(), ApiError> {
    let (existing, _) = state
        .store
        .list(ResourceKind::Service, None)
        .await
        .map_err(ApiError::from)?;
    let services: Vec<Value> = existing.into_iter().map(|o| o.data).collect();
    let mut used = collect_used(&services);

    let is_node_port = data.pointer("/spec/type").and_then(Value::as_str) == Some("NodePort");

    let spec = data
        .get_mut("spec")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::BadRequest("service has no spec".to_owned()))?;

    if spec.get("clusterIP").and_then(Value::as_str).is_none() {
        let ip = pick_cluster_ip(&used.cluster_ips)
            .ok_or_else(|| ApiError::Conflict("service CIDR exhausted".to_owned()))?;
        used.cluster_ips.insert(ip);
        spec.insert("clusterIP".to_owned(), Value::String(ip.to_string()));
    }

    if is_node_port && let Some(ports) = spec.get_mut("ports").and_then(Value::as_array_mut) {
        for port in ports {
            let Some(port) = port.as_object_mut() else {
                continue;
            };
            if port.get("nodePort").and_then(Value::as_u64).is_none() {
                let np = pick_node_port(&used.node_ports)
                    .ok_or_else(|| ApiError::Conflict("node port range exhausted".to_owned()))?;
                used.node_ports.insert(np);
                port.insert("nodePort".to_owned(), Value::Number(np.into()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_ip_starts_at_ten_and_skips_used() {
        let mut used = HashSet::new();
        assert_eq!(pick_cluster_ip(&used).unwrap().to_string(), "10.96.0.10");
        used.insert("10.96.0.10".parse().unwrap());
        used.insert("10.96.0.11".parse().unwrap());
        assert_eq!(pick_cluster_ip(&used).unwrap().to_string(), "10.96.0.12");
    }

    #[test]
    fn node_port_picks_lowest_free() {
        let mut used = HashSet::new();
        assert_eq!(pick_node_port(&used), Some(30000));
        used.extend([30000, 30001]);
        assert_eq!(pick_node_port(&used), Some(30002));
    }

    #[test]
    fn collect_used_reads_existing_services() {
        let services = vec![serde_json::json!({
            "spec": { "clusterIP": "10.96.0.10", "ports": [{ "port": 80, "nodePort": 30080 }] }
        })];
        let used = collect_used(&services);
        assert!(used.cluster_ips.contains(&"10.96.0.10".parse().unwrap()));
        assert!(used.node_ports.contains(&30080));
    }
}
