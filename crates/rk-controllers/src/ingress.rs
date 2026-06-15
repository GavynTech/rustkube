//! Ingress routing, kept pure: turn the cluster's Ingress and Service objects
//! into a flat routing table, and match an incoming `(host, path)` to a
//! backend. The reverse proxy (`bin/rk-ingress.rs`) does the I/O around this.

use rk_types::Service;
use rk_types::ingress::{Ingress, PathType};

/// One resolved route: a host/path matcher and the service VIP it forwards to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// `None` matches any host.
    pub host: Option<String>,
    pub path: String,
    pub path_type: PathType,
    pub backend: std::net::SocketAddrV4,
}

/// Resolve every Ingress path against the Services to a [`Route`]. Paths whose
/// backend service has no ClusterIP yet (just created) are skipped.
pub fn build_routes(ingresses: &[Ingress], services: &[Service]) -> Vec<Route> {
    let mut routes = Vec::new();
    for ingress in ingresses {
        let ns = ingress.metadata.namespace_or_default();
        for rule in &ingress.spec.rules {
            for path in &rule.http.paths {
                let backend = &path.backend.service;
                let Some(cluster_ip) = services
                    .iter()
                    .find(|svc| {
                        svc.metadata.name == backend.name
                            && svc.metadata.namespace_or_default() == ns
                    })
                    .and_then(|svc| svc.spec.cluster_ip)
                else {
                    continue;
                };
                routes.push(Route {
                    host: rule.host.clone(),
                    path: path.path.clone(),
                    path_type: path.path_type,
                    backend: std::net::SocketAddrV4::new(cluster_ip, backend.port),
                });
            }
        }
    }
    routes
}

fn path_matches(route: &Route, path: &str) -> bool {
    match route.path_type {
        PathType::Exact => path == route.path,
        PathType::Prefix => {
            // `/foo` matches `/foo` and `/foo/...`, but not `/foobar`.
            path == route.path
                || (path.starts_with(&route.path)
                    && (route.path.ends_with('/') || path[route.path.len()..].starts_with('/')))
                || route.path == "/"
        }
    }
}

/// Most-specific match wins: a named host beats a wildcard, and among those a
/// longer path beats a shorter one.
pub fn match_route<'r>(routes: &'r [Route], host: &str, path: &str) -> Option<&'r Route> {
    routes
        .iter()
        .filter(|route| route.host.as_deref().is_none_or(|h| h == host))
        .filter(|route| path_matches(route, path))
        .max_by_key(|route| (route.host.is_some(), route.path.len()))
}

#[cfg(test)]
mod tests {
    use rk_types::ingress::{
        HttpIngressPath, HttpIngressRule, IngressBackend, IngressRule, IngressSpec, ServiceBackend,
    };
    use rk_types::service::{ServicePort, ServiceSpec};
    use rk_types::{ObjectMeta, ServiceType};

    use super::*;

    fn service(name: &str, cip: &str) -> Service {
        Service {
            api_version: "v1".to_owned(),
            kind: "Service".to_owned(),
            metadata: ObjectMeta::named(name),
            spec: ServiceSpec {
                service_type: ServiceType::ClusterIP,
                cluster_ip: Some(cip.parse().unwrap()),
                ports: vec![ServicePort {
                    name: None,
                    port: 80,
                    target_port: None,
                    node_port: None,
                    protocol: Default::default(),
                }],
                ..ServiceSpec::default()
            },
            status: Default::default(),
        }
    }

    fn ingress(host: Option<&str>, path: &str, ptype: PathType, svc: &str, port: u16) -> Ingress {
        Ingress {
            api_version: "v1".to_owned(),
            kind: "Ingress".to_owned(),
            metadata: ObjectMeta::named("ing"),
            spec: IngressSpec {
                rules: vec![IngressRule {
                    host: host.map(str::to_owned),
                    http: HttpIngressRule {
                        paths: vec![HttpIngressPath {
                            path: path.to_owned(),
                            path_type: ptype,
                            backend: IngressBackend {
                                service: ServiceBackend {
                                    name: svc.to_owned(),
                                    port,
                                },
                            },
                        }],
                    },
                }],
                tls: Vec::new(),
            },
            status: Default::default(),
        }
    }

    #[test]
    fn resolves_backends_to_cluster_ips() {
        let routes = build_routes(
            &[ingress(
                Some("example.com"),
                "/",
                PathType::Prefix,
                "web",
                80,
            )],
            &[service("web", "10.96.0.10")],
        );
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend.to_string(), "10.96.0.10:80");
    }

    #[test]
    fn skips_paths_whose_service_has_no_cluster_ip() {
        let routes = build_routes(
            &[ingress(None, "/", PathType::Prefix, "missing", 80)],
            &[service("web", "10.96.0.10")],
        );
        assert!(routes.is_empty());
    }

    #[test]
    fn prefix_paths_match_on_segment_boundaries() {
        let routes = build_routes(
            &[ingress(None, "/api", PathType::Prefix, "web", 80)],
            &[service("web", "10.96.0.10")],
        );
        assert!(match_route(&routes, "any", "/api").is_some());
        assert!(match_route(&routes, "any", "/api/v1").is_some());
        assert!(
            match_route(&routes, "any", "/apiary").is_none(),
            "no false prefix"
        );
    }

    #[test]
    fn most_specific_route_wins() {
        let routes = build_routes(
            &[
                ingress(None, "/", PathType::Prefix, "root", 80),
                ingress(None, "/api", PathType::Prefix, "api", 80),
                ingress(Some("example.com"), "/", PathType::Prefix, "host", 80),
            ],
            &[
                service("root", "10.96.0.1"),
                service("api", "10.96.0.2"),
                service("host", "10.96.0.3"),
            ],
        );
        // Longer path beats the root.
        assert_eq!(
            match_route(&routes, "x", "/api/foo")
                .unwrap()
                .backend
                .to_string(),
            "10.96.0.2:80"
        );
        // Named host beats the wildcard at the same path.
        assert_eq!(
            match_route(&routes, "example.com", "/")
                .unwrap()
                .backend
                .to_string(),
            "10.96.0.3:80"
        );
        // Other hosts fall back to the wildcard root.
        assert_eq!(
            match_route(&routes, "other", "/")
                .unwrap()
                .backend
                .to_string(),
            "10.96.0.1:80"
        );
    }

    #[test]
    fn exact_paths_must_equal() {
        let routes = build_routes(
            &[ingress(None, "/health", PathType::Exact, "web", 80)],
            &[service("web", "10.96.0.10")],
        );
        assert!(match_route(&routes, "x", "/health").is_some());
        assert!(match_route(&routes, "x", "/health/live").is_none());
    }
}
