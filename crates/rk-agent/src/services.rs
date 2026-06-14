//! The node's service data plane: watch Services and Pods, compute each
//! service's ready endpoints, and (re)program the node's nftables rules and
//! the embedded DNS records. Every node runs this, so a service VIP works
//! from any pod regardless of where its backends live (the overlay carries
//! the DNATed traffic across nodes).
//!
//! The endpoint and DNS computations are pure and tested; the loop is the
//! impure glue.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use rk_net::dns::Records;
use rk_net::service::{self, ServiceTarget};
use rk_types::client::ApiClient;
use rk_types::client::reflector::{self, Cache};
use rk_types::{PodPhase, Service, ServiceType};
use tokio::sync::Notify;

/// A pod is a usable endpoint when it's Running, its container reports ready,
/// and it has an IP.
fn ready_endpoint(pod: &rk_types::Pod) -> Option<Ipv4Addr> {
    if pod.status.phase != PodPhase::Running {
        return None;
    }
    if pod.status.container_statuses.is_empty()
        || !pod.status.container_statuses.iter().all(|c| c.ready)
    {
        return None;
    }
    pod.status.pod_ip
}

fn selector_matches(
    selector: &std::collections::BTreeMap<String, String>,
    labels: &std::collections::BTreeMap<String, String>,
) -> bool {
    !selector.is_empty() && selector.iter().all(|(k, v)| labels.get(k) == Some(v))
}

/// Distill services + pods into the nftables targets: one per (service, port),
/// each with the ready backing pod IPs.
pub fn service_targets(services: &[Service], pods: &[rk_types::Pod]) -> Vec<ServiceTarget> {
    let mut targets = Vec::new();
    for service in services {
        let Some(cluster_ip) = service.spec.cluster_ip else {
            continue;
        };
        let ns = service.metadata.namespace_or_default();
        let backends: Vec<Ipv4Addr> = pods
            .iter()
            .filter(|pod| pod.metadata.namespace_or_default() == ns)
            .filter(|pod| selector_matches(&service.spec.selector, &pod.metadata.labels))
            .filter_map(ready_endpoint)
            .collect();
        if backends.is_empty() {
            continue;
        }
        for port in &service.spec.ports {
            let node_port = match service.spec.service_type {
                ServiceType::NodePort => port.node_port,
                ServiceType::ClusterIP => None,
            };
            targets.push(ServiceTarget {
                name: format!("{}_{ns}", service.metadata.name),
                cluster_ip,
                port: port.port,
                node_port,
                endpoints: backends
                    .iter()
                    .map(|ip| (*ip, port.effective_target_port()))
                    .collect(),
            });
        }
    }
    targets
}

/// Map every service name to its cluster IP, in both the `<svc>.<ns>` and the
/// fully-qualified `…svc.cluster.local` forms the resolver search list uses.
pub fn dns_records(services: &[Service]) -> HashMap<String, Ipv4Addr> {
    let mut records = HashMap::new();
    for service in services {
        let Some(cluster_ip) = service.spec.cluster_ip else {
            continue;
        };
        let ns = service.metadata.namespace_or_default();
        let name = &service.metadata.name;
        records.insert(format!("{name}.{ns}"), cluster_ip);
        records.insert(format!("{name}.{ns}.svc.cluster.local"), cluster_ip);
    }
    records
}

/// Watch Services and Pods cluster-wide and keep this node's nftables rules
/// and DNS records in sync forever.
pub async fn run(client: Arc<ApiClient>, records: Records) {
    let services: Cache<Service> = Arc::default();
    let pods: Cache<rk_types::Pod> = Arc::default();
    let notify = Arc::new(Notify::new());
    tokio::spawn(reflector::reflect(
        client.clone(),
        services.clone(),
        notify.clone(),
    ));
    tokio::spawn(reflector::reflect(client, pods.clone(), notify.clone()));

    loop {
        notify.notified().await;
        let service_list = reflector::snapshot(&services).await;
        let pod_list = reflector::snapshot(&pods).await;

        let targets = service_targets(&service_list, &pod_list);
        if let Err(err) = service::apply(&targets).await {
            tracing::warn!(error = %err, "programming service nftables rules failed");
        }

        let new_records = dns_records(&service_list);
        if let Ok(mut guard) = records.lock() {
            *guard = new_records;
        }
        tracing::debug!(
            services = service_list.len(),
            targets = targets.len(),
            "service data plane updated"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rk_types::pod::{ContainerStatus, PodSpec, PodStatus};
    use rk_types::service::{ServicePort, ServiceSpec};
    use rk_types::{ObjectMeta, Pod};

    use super::*;

    fn pod(name: &str, ns: &str, app: &str, ip: Option<&str>, ready: bool) -> Pod {
        Pod {
            api_version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            metadata: ObjectMeta {
                namespace: Some(ns.to_owned()),
                labels: BTreeMap::from([("app".to_owned(), app.to_owned())]),
                ..ObjectMeta::named(name)
            },
            spec: PodSpec::default(),
            status: PodStatus {
                phase: if ready {
                    PodPhase::Running
                } else {
                    PodPhase::Pending
                },
                pod_ip: ip.map(|i| i.parse().unwrap()),
                container_statuses: vec![ContainerStatus {
                    name: "c".to_owned(),
                    ready,
                    restart_count: 0,
                    state: Default::default(),
                }],
                ..PodStatus::default()
            },
        }
    }

    fn service(name: &str, ns: &str, cip: &str, app: &str) -> Service {
        Service {
            api_version: "v1".to_owned(),
            kind: "Service".to_owned(),
            metadata: ObjectMeta {
                namespace: Some(ns.to_owned()),
                ..ObjectMeta::named(name)
            },
            spec: ServiceSpec {
                service_type: ServiceType::ClusterIP,
                selector: BTreeMap::from([("app".to_owned(), app.to_owned())]),
                cluster_ip: Some(cip.parse().unwrap()),
                ports: vec![ServicePort {
                    name: None,
                    port: 80,
                    target_port: Some(8080),
                    node_port: None,
                    protocol: Default::default(),
                }],
            },
            status: Default::default(),
        }
    }

    #[test]
    fn endpoints_are_ready_matching_pods_in_the_namespace() {
        let svc = service("web", "default", "10.96.0.10", "web");
        let pods = [
            pod("web-1", "default", "web", Some("10.42.0.2"), true),
            pod("web-2", "default", "web", Some("10.42.1.2"), true),
            pod("web-3", "default", "web", Some("10.42.0.3"), false), // not ready
            pod("web-4", "default", "db", Some("10.42.0.4"), true),   // wrong label
            pod("web-5", "other", "web", Some("10.42.0.5"), true),    // wrong ns
        ];
        let targets = service_targets(&[svc], &pods);
        assert_eq!(targets.len(), 1);
        let endpoints: Vec<String> = targets[0]
            .endpoints
            .iter()
            .map(|(ip, p)| format!("{ip}:{p}"))
            .collect();
        assert_eq!(endpoints, vec!["10.42.0.2:8080", "10.42.1.2:8080"]);
    }

    #[test]
    fn services_with_no_ready_pods_produce_no_target() {
        let svc = service("web", "default", "10.96.0.10", "web");
        let pods = [pod("web-1", "default", "web", Some("10.42.0.2"), false)];
        assert!(service_targets(&[svc], &pods).is_empty());
    }

    #[test]
    fn dns_records_cover_short_and_fqdn() {
        let svc = service("web", "default", "10.96.0.10", "web");
        let records = dns_records(&[svc]);
        assert_eq!(records["web.default"].to_string(), "10.96.0.10");
        assert_eq!(
            records["web.default.svc.cluster.local"].to_string(),
            "10.96.0.10"
        );
    }
}
