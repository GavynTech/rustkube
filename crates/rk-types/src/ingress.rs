//! Ingress: host/path routing rules from external HTTP(S) traffic to
//! Services, executed by the ingress controller (Phase 7).

use serde::{Deserialize, Serialize};

use crate::Resource;
use crate::meta::{ObjectMeta, ResourceKind, api_version_v1};

fn kind_ingress() -> String {
    "Ingress".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ingress {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_ingress")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: IngressSpec,
    #[serde(default)]
    pub status: IngressStatus,
}

impl Resource for Ingress {
    const KIND: ResourceKind = ResourceKind::Ingress;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IngressSpec {
    pub rules: Vec<IngressRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tls: Vec<IngressTls>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressRule {
    /// `None` matches any host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub http: HttpIngressRule,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HttpIngressRule {
    pub paths: Vec<HttpIngressPath>,
}

fn default_path() -> String {
    "/".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpIngressPath {
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub path_type: PathType,
    pub backend: IngressBackend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathType {
    #[default]
    Prefix,
    Exact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressBackend {
    pub service: ServiceBackend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceBackend {
    pub name: String,
    pub port: u16,
}

/// TLS termination config: hosts served with the certificate stored in the
/// named Secret (keys `tls.crt` / `tls.key`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressTls {
    pub hosts: Vec<String>,
    pub secret_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IngressStatus {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingress_manifest_parses() {
        let ingress: Ingress = serde_yaml::from_str(
            r#"
apiVersion: v1
kind: Ingress
metadata:
  name: web
spec:
  rules:
    - host: example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: web
                port: 80
  tls:
    - hosts: [example.com]
      secretName: web-tls
"#,
        )
        .unwrap();
        let rule = &ingress.spec.rules[0];
        assert_eq!(rule.host.as_deref(), Some("example.com"));
        assert_eq!(rule.http.paths[0].path_type, PathType::Prefix);
        assert_eq!(rule.http.paths[0].backend.service.name, "web");
        assert_eq!(ingress.spec.tls[0].secret_name, "web-tls");
    }
}
