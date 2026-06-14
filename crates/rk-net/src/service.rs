//! Service virtual IPs via nftables. The agent distills each Service and its
//! ready endpoints into [`ServiceTarget`]s; this module renders them into one
//! `nft` ruleset (table `rk-services`) that DNATs traffic to a cluster IP — or
//! a node port — to one of the backing pods, load-balanced at random.
//!
//! The whole table is regenerated and re-applied on every change (flush +
//! reload), which is simple and always converges. Generation is pure and
//! tested; [`apply`]/[`clear`] shell out to `nft`.

use std::net::Ipv4Addr;
use std::process::Stdio;

use crate::NetError;

const TABLE: &str = "rk-services";

/// One Service port and the pod endpoints that back it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceTarget {
    pub name: String,
    pub cluster_ip: Ipv4Addr,
    pub port: u16,
    /// Node port, for `NodePort` services.
    pub node_port: Option<u16>,
    /// `(pod ip, target port)` for each ready backing pod.
    pub endpoints: Vec<(Ipv4Addr, u16)>,
}

/// The DNAT expression that picks one endpoint at random (or the only one).
fn dnat_expr(endpoints: &[(Ipv4Addr, u16)]) -> String {
    if endpoints.len() == 1 {
        let (ip, port) = endpoints[0];
        return format!("dnat to {ip}:{port}");
    }
    let arms: Vec<String> = endpoints
        .iter()
        .enumerate()
        .map(|(i, (ip, port))| format!("{i} : {ip} . {port}"))
        .collect();
    format!(
        "dnat to numgen random mod {} map {{ {} }}",
        endpoints.len(),
        arms.join(", ")
    )
}

/// Render the full `rk-services` nft table. Services with no ready endpoints
/// are skipped (traffic to them simply finds no rule). Returns `None` when
/// there is nothing to program, so the caller clears the table instead.
pub fn nftables_ruleset(targets: &[ServiceTarget]) -> Option<String> {
    let active: Vec<&ServiceTarget> = targets.iter().filter(|t| !t.endpoints.is_empty()).collect();
    if active.is_empty() {
        return None;
    }

    let mut clusterip_rules = String::new();
    let mut nodeport_rules = String::new();
    for target in &active {
        let dnat = dnat_expr(&target.endpoints);
        clusterip_rules.push_str(&format!(
            "    ip daddr {} tcp dport {} {dnat}\n",
            target.cluster_ip, target.port
        ));
        if let Some(node_port) = target.node_port {
            // Only traffic addressed to one of this host's own IPs.
            nodeport_rules.push_str(&format!(
                "    fib daddr type local tcp dport {node_port} {dnat}\n"
            ));
        }
    }

    Some(format!(
        "table ip {TABLE} {{\n\
         \x20 chain prerouting {{\n\
         \x20   type nat hook prerouting priority dstnat; policy accept;\n\
         {clusterip_rules}{nodeport_rules}\
         \x20 }}\n\
         \x20 chain output {{\n\
         \x20   type nat hook output priority -100; policy accept;\n\
         {clusterip_rules}\
         \x20 }}\n\
         }}\n"
    ))
}

async fn run_nft(stdin: &str) -> Result<(), NetError> {
    use tokio::io::AsyncWriteExt as _;
    let mut child = tokio::process::Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| NetError::Netlink {
            op: "spawning nft",
            detail: err.to_string(),
        })?;
    if let Some(mut sink) = child.stdin.take() {
        sink.write_all(stdin.as_bytes())
            .await
            .map_err(|err| NetError::Netlink {
                op: "writing nft ruleset",
                detail: err.to_string(),
            })?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|err| NetError::Netlink {
            op: "running nft",
            detail: err.to_string(),
        })?;
    if !output.status.success() {
        return Err(NetError::Netlink {
            op: "nft",
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

/// Replace the `rk-services` table with rules for `targets`. Flushes first so
/// removed services disappear; no active services means the table is cleared.
pub async fn apply(targets: &[ServiceTarget]) -> Result<(), NetError> {
    match nftables_ruleset(targets) {
        Some(rules) => {
            run_nft(&format!(
                "table ip {TABLE} {{}}\ndelete table ip {TABLE}\n{rules}"
            ))
            .await
        }
        None => clear().await,
    }
}

/// Remove the `rk-services` table entirely. Idempotent.
pub async fn clear() -> Result<(), NetError> {
    // `add` then `delete` so deleting a non-existent table never errors.
    run_nft(&format!("table ip {TABLE} {{}}\ndelete table ip {TABLE}\n")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, cip: &str, port: u16, eps: &[(&str, u16)]) -> ServiceTarget {
        ServiceTarget {
            name: name.to_owned(),
            cluster_ip: cip.parse().unwrap(),
            port,
            node_port: None,
            endpoints: eps
                .iter()
                .map(|(ip, p)| (ip.parse().unwrap(), *p))
                .collect(),
        }
    }

    #[test]
    fn single_endpoint_dnats_directly() {
        assert_eq!(
            dnat_expr(&[("10.42.0.2".parse().unwrap(), 80)]),
            "dnat to 10.42.0.2:80"
        );
    }

    #[test]
    fn multiple_endpoints_load_balance_at_random() {
        let expr = dnat_expr(&[
            ("10.42.0.2".parse().unwrap(), 80),
            ("10.42.1.2".parse().unwrap(), 80),
        ]);
        assert_eq!(
            expr,
            "dnat to numgen random mod 2 map { 0 : 10.42.0.2 . 80, 1 : 10.42.1.2 . 80 }"
        );
    }

    #[test]
    fn ruleset_has_clusterip_rules_in_both_hooks() {
        let targets = [target("web", "10.96.0.10", 80, &[("10.42.0.2", 8080)])];
        let rules = nftables_ruleset(&targets).unwrap();
        assert!(rules.contains("table ip rk-services"));
        assert!(rules.contains("hook prerouting"));
        assert!(rules.contains("hook output"));
        // The DNAT appears in both prerouting and output.
        assert_eq!(
            rules
                .matches("ip daddr 10.96.0.10 tcp dport 80 dnat to 10.42.0.2:8080")
                .count(),
            2
        );
    }

    #[test]
    fn nodeport_rule_is_local_only_and_prerouting() {
        let mut t = target("web", "10.96.0.10", 80, &[("10.42.0.2", 8080)]);
        t.node_port = Some(30080);
        let rules = nftables_ruleset(&[t]).unwrap();
        assert!(rules.contains("fib daddr type local tcp dport 30080 dnat to 10.42.0.2:8080"));
        // NodePort is not added to the output hook (host-local origin uses clusterIP).
        let output_section = rules.split("chain output").nth(1).unwrap();
        assert!(!output_section.contains("30080"));
    }

    #[test]
    fn endpointless_services_are_skipped_and_empty_clears() {
        let targets = [target("web", "10.96.0.10", 80, &[])];
        assert!(
            nftables_ruleset(&targets).is_none(),
            "no endpoints → nothing to program"
        );
    }
}
