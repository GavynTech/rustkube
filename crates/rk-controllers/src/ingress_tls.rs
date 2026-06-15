//! TLS termination for the ingress. Build a per-host certificate map from the
//! Ingress `tls` sections and their referenced Secrets, and resolve it by SNI
//! at handshake time. Terminating here means the proxy still speaks cleartext
//! to the backend — the service VIP and its nftables load-balancing are
//! unchanged; only the client-facing hop is encrypted.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use base64::Engine as _;
use rk_types::{Ingress, Secret};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// Resolves the server certificate by SNI host, backed by a map the ingress
/// swaps out whenever Ingress or Secret objects change.
#[derive(Debug, Default)]
pub struct SniResolver {
    by_host: RwLock<HashMap<String, Arc<CertifiedKey>>>,
}

impl SniResolver {
    /// Replace the whole cert map (called after every reflector update).
    pub fn replace(&self, certs: HashMap<String, Arc<CertifiedKey>>) {
        *self.by_host.write().unwrap_or_else(PoisonError::into_inner) = certs;
    }

    pub fn host_count(&self) -> usize {
        self.by_host
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let by_host = self.by_host.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(name) = hello.server_name()
            && let Some(key) = by_host.get(name)
        {
            return Some(key.clone());
        }
        // No SNI match: fall back to any configured cert, so a single-site
        // ingress still answers clients that send no SNI.
        by_host.values().next().cloned()
    }
}

/// A rustls server config that resolves certs through `resolver`. The process
/// crypto provider must already be installed (see `rk-ingress` `main`).
pub fn server_config(resolver: Arc<SniResolver>) -> Arc<rustls::ServerConfig> {
    Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver),
    )
}

/// Build the SNI map: for each Ingress TLS entry, load its Secret's cert+key
/// and map every listed host to it. Entries whose Secret is missing or
/// unparseable are skipped — they appear once the Secret lands.
pub fn build_certs(
    ingresses: &[Ingress],
    secrets: &[Secret],
) -> HashMap<String, Arc<CertifiedKey>> {
    let mut out = HashMap::new();
    for ingress in ingresses {
        let ns = ingress.metadata.namespace_or_default();
        for tls in &ingress.spec.tls {
            let found = secrets.iter().find(|secret| {
                secret.metadata.name == tls.secret_name
                    && secret.metadata.namespace_or_default() == ns
            });
            let Some(secret) = found else { continue };
            let Some(key) = certified_key(secret) else {
                continue;
            };
            let key = Arc::new(key);
            for host in &tls.hosts {
                out.insert(host.clone(), key.clone());
            }
        }
    }
    out
}

/// Parse a `kubernetes.io/tls` Secret (base64 PEM under `tls.crt` / `tls.key`)
/// into a rustls [`CertifiedKey`].
fn certified_key(secret: &Secret) -> Option<CertifiedKey> {
    let cert_pem = decode(secret.data.get("tls.crt")?)?;
    let key_pem = decode(secret.data.get("tls.key")?)?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<CertificateDer<'static>>, _>>()
        .ok()?;
    if certs.is_empty() {
        return None;
    }
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut key_pem.as_slice()).ok()??;
    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key).ok()?;
    Some(CertifiedKey::new(certs, signing_key))
}

fn decode(b64: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rk_types::ObjectMeta;
    use rk_types::ingress::{IngressSpec, IngressTls};

    use super::*;

    /// A real cert/key pair for `app.example.com`, as a tls Secret would carry
    /// it: base64-encoded PEM under `tls.crt` / `tls.key`.
    fn tls_secret(name: &str, host: &str) -> Secret {
        let cert = rcgen::generate_simple_self_signed(vec![host.to_owned()]).unwrap();
        let b64 = |s: String| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        let mut data = BTreeMap::new();
        data.insert("tls.crt".to_owned(), b64(cert.cert.pem()));
        data.insert("tls.key".to_owned(), b64(cert.signing_key.serialize_pem()));
        Secret {
            api_version: "v1".to_owned(),
            kind: "Secret".to_owned(),
            metadata: ObjectMeta::named(name),
            data,
            string_data: BTreeMap::new(),
        }
    }

    fn ingress_with_tls(secret: &str, hosts: &[&str]) -> Ingress {
        Ingress {
            api_version: "v1".to_owned(),
            kind: "Ingress".to_owned(),
            metadata: ObjectMeta::named("ing"),
            spec: IngressSpec {
                rules: Vec::new(),
                tls: vec![IngressTls {
                    hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
                    secret_name: secret.to_owned(),
                }],
            },
            status: Default::default(),
        }
    }

    #[test]
    fn builds_a_cert_per_listed_host() {
        let certs = build_certs(
            &[ingress_with_tls(
                "web-tls",
                &["app.example.com", "www.example.com"],
            )],
            &[tls_secret("web-tls", "app.example.com")],
        );
        assert!(certs.contains_key("app.example.com"));
        assert!(certs.contains_key("www.example.com"));
    }

    #[test]
    fn skips_tls_entries_with_no_secret() {
        let certs = build_certs(
            &[ingress_with_tls("missing", &["app.example.com"])],
            &[tls_secret("other", "app.example.com")],
        );
        assert!(certs.is_empty());
    }

    #[test]
    fn resolver_reports_the_loaded_host_count() {
        let resolver = SniResolver::default();
        assert_eq!(resolver.host_count(), 0);
        resolver.replace(build_certs(
            &[ingress_with_tls("web-tls", &["app.example.com"])],
            &[tls_secret("web-tls", "app.example.com")],
        ));
        assert_eq!(resolver.host_count(), 1);
    }
}
