//! Service: a stable virtual IP load-balancing across the pods matched by its
//! selector. ClusterIP services are reachable inside the cluster; NodePort
//! additionally exposes the service on every node's IP.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::Resource;
use crate::meta::{ObjectMeta, ResourceKind, api_version_v1};
use crate::pod::Protocol;

fn kind_service() -> String {
    "Service".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_service")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: ServiceSpec,
    #[serde(default)]
    pub status: ServiceStatus,
}

impl Resource for Service {
    const KIND: ResourceKind = ResourceKind::Service;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceSpec {
    #[serde(rename = "type")]
    pub service_type: ServiceType,
    /// Pods whose labels match all entries back this service.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub selector: BTreeMap<String, String>,
    pub ports: Vec<ServicePort>,
    /// Virtual IP, allocated by the API server from the service CIDR.
    /// Kubernetes spells the acronym in caps — `clusterIP`, not the `clusterIp`
    /// camelCase would produce — and the API allocator writes that spelling.
    #[serde(rename = "clusterIP", skip_serializing_if = "Option::is_none")]
    pub cluster_ip: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceType {
    #[default]
    ClusterIP,
    NodePort,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Port exposed on the cluster IP.
    pub port: u16,
    /// Container port traffic is forwarded to; defaults to `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
    /// For NodePort services: the port opened on every node (allocated from
    /// 30000-32767 when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_port: Option<u16>,
    #[serde(default)]
    pub protocol: Protocol,
}

impl ServicePort {
    pub fn effective_target_port(&self) -> u16 {
        self.target_port.unwrap_or(self.port)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceStatus {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_manifest_parses() {
        let service: Service = serde_yaml::from_str(
            r#"
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  type: NodePort
  selector:
    app: web
  ports:
    - port: 80
      targetPort: 8080
      nodePort: 30080
"#,
        )
        .unwrap();
        assert_eq!(service.spec.service_type, ServiceType::NodePort);
        assert_eq!(service.spec.ports[0].effective_target_port(), 8080);
        assert_eq!(service.spec.ports[0].node_port, Some(30080));
        assert_eq!(service.spec.ports[0].protocol, Protocol::Tcp);
        assert!(service.spec.cluster_ip.is_none(), "allocated server-side");
    }

    #[test]
    fn cluster_ip_uses_the_kubernetes_spelling() {
        // The API allocator and `kubectl` both write `clusterIP`; the typed
        // model must round-trip that exact key, not camelCase `clusterIp`.
        let service: Service =
            serde_yaml::from_str("metadata:\n  name: web\nspec:\n  clusterIP: 10.96.0.10\n")
                .unwrap();
        assert_eq!(service.spec.cluster_ip, Some("10.96.0.10".parse().unwrap()));
        let json = serde_json::to_value(&service).unwrap();
        assert_eq!(json["spec"]["clusterIP"], "10.96.0.10");
    }

    #[test]
    fn target_port_defaults_to_port() {
        let port = ServicePort {
            name: None,
            port: 443,
            target_port: None,
            node_port: None,
            protocol: Protocol::Tcp,
        };
        assert_eq!(port.effective_target_port(), 443);
    }
}
