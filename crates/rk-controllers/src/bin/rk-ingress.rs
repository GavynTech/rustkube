//! `rk-ingress` — the ingress controller and reverse proxy.
//!
//! It watches Ingress, Service and Secret objects, keeps a routing table of
//! `(host, path) -> service ClusterIP:port`, and serves HTTP on :80 and HTTPS
//! on :443, forwarding each request to the matched service's virtual IP (where
//! the node's nftables rules load-balance it onto a backing pod). It must run
//! on a node that carries those service rules.
//!
//! The proxy is hand-rolled HTTP/1.1 over tokio — the same minimal-dependency
//! style as the agent's DNS and log servers. On :443 it terminates TLS with
//! rustls, choosing the certificate by SNI from the Ingress `tls` Secrets (see
//! `ingress_tls`), then runs the identical cleartext proxy to the backend.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rk_controllers::ingress::{self, Route};
use rk_controllers::ingress_tls::{self, SniResolver};
use rk_types::client::ApiClient;
use rk_types::client::reflector::{self, Cache};
use rk_types::{Ingress, Secret, Service};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, RwLock};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "rk-ingress",
    version,
    about = "RustKube ingress controller + reverse proxy"
)]
struct Args {
    /// API server base URL.
    #[arg(long, env = "RK_API_URL", default_value = "http://127.0.0.1:6443")]
    api_url: String,
    /// File holding the bearer token to present to the API server.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// PEM CA certificate that signed the API server's TLS cert.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// Port to serve cleartext HTTP on.
    #[arg(long, default_value_t = 80)]
    http_port: u16,
    /// Port to serve HTTPS on (TLS terminated from Ingress `tls` Secrets).
    #[arg(long, default_value_t = 443)]
    https_port: u16,
}

type Routes = Arc<RwLock<Vec<Route>>>;

// Canned error responses. Each Content-Length must equal its body's byte
// length, or a strict client (curl) aborts the read with "transfer closed with
// outstanding data" — see `error_bodies_match_their_content_length`.
const RESP_404: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 16\r\nConnection: close\r\n\r\n404: no ingress\n";
const RESP_502: &[u8] =
    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 17\r\nConnection: close\r\n\r\n502: bad gateway\n";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rk_types::bootstrap::init_tracing()?;
    let args = Args::parse();
    info!(api_url = %args.api_url, http = args.http_port, https = args.https_port, "rk-ingress starting");

    let client = Arc::new(ApiClient::connect(
        &args.api_url,
        args.token_file.as_deref(),
        args.ca_file.as_deref(),
    )?);
    let routes: Routes = Arc::new(RwLock::new(Vec::new()));

    // TLS: one rustls config whose SNI resolver the watcher keeps current.
    // rustls 0.23 needs a process-wide crypto provider chosen explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let resolver = Arc::new(SniResolver::default());
    let tls = TlsAcceptor::from(ingress_tls::server_config(resolver.clone()));

    tokio::spawn(maintain_routes(client, routes.clone(), resolver));

    // HTTPS listener: terminate TLS, then run the same proxy over the stream.
    let https = TcpListener::bind(("0.0.0.0", args.https_port)).await?;
    info!(port = args.https_port, "ingress HTTPS listening");
    tokio::spawn(serve_https(https, tls, routes.clone()));

    // HTTP listener.
    let listener = TcpListener::bind(("0.0.0.0", args.http_port)).await?;
    info!(port = args.http_port, "ingress HTTP listening");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let routes = routes.clone();
        tokio::spawn(async move {
            if let Err(err) = proxy(stream, routes).await {
                tracing::debug!(error = %err, "proxy connection ended");
            }
        });
    }
}

/// Accept TLS connections, terminate the handshake, then proxy the plaintext.
async fn serve_https(listener: TcpListener, tls: TlsAcceptor, routes: Routes) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (tls, routes) = (tls.clone(), routes.clone());
        tokio::spawn(async move {
            match tls.accept(stream).await {
                Ok(stream) => {
                    if let Err(err) = proxy(stream, routes).await {
                        tracing::debug!(error = %err, "https proxy connection ended");
                    }
                }
                Err(err) => tracing::debug!(error = %err, "tls handshake failed"),
            }
        });
    }
}

/// Watch Ingress + Service + Secret and rebuild both the routing table and the
/// TLS certificate map on every change.
async fn maintain_routes(client: Arc<ApiClient>, routes: Routes, resolver: Arc<SniResolver>) {
    let ingresses: Cache<Ingress> = Arc::default();
    let services: Cache<Service> = Arc::default();
    let secrets: Cache<Secret> = Arc::default();
    let notify = Arc::new(Notify::new());
    tokio::spawn(reflector::reflect(
        client.clone(),
        ingresses.clone(),
        notify.clone(),
    ));
    tokio::spawn(reflector::reflect(
        client.clone(),
        services.clone(),
        notify.clone(),
    ));
    tokio::spawn(reflector::reflect(client, secrets.clone(), notify.clone()));

    loop {
        notify.notified().await;
        let ing = reflector::snapshot(&ingresses).await;
        let svc = reflector::snapshot(&services).await;
        let sec = reflector::snapshot(&secrets).await;
        let table = ingress::build_routes(&ing, &svc);
        let routes_count = table.len();
        *routes.write().await = table;
        let certs = ingress_tls::build_certs(&ing, &sec);
        let tls_hosts = certs.len();
        resolver.replace(certs);
        tracing::debug!(routes = routes_count, tls_hosts, "ingress tables rebuilt");
    }
}

/// Read just enough of the request to route it, then splice to the backend.
/// Generic over the client stream so it serves both plain TCP (:80) and a
/// TLS-terminated stream (:443) with one body.
async fn proxy<S>(mut client: S, routes: Routes) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Read the request head (up to the blank line), capped so a slow or
    // oversized client can't hold the connection open forever.
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > 65536 {
            break;
        }
    }

    let (host, path) = parse_request(&head).unwrap_or_default();
    let backend = match_backend(&routes, &host, &path).await;
    let Some(backend) = backend else {
        let _ = client.write_all(RESP_404).await;
        return Ok(());
    };

    let mut upstream = match TcpStream::connect(backend).await {
        Ok(stream) => stream,
        Err(err) => {
            warn!(%backend, error = %err, "ingress backend unreachable");
            let _ = client.write_all(RESP_502).await;
            return Ok(());
        }
    };
    // Replay the head we consumed, then let the two sides talk freely.
    upstream.write_all(&head).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn match_backend(routes: &Routes, host: &str, path: &str) -> Option<std::net::SocketAddrV4> {
    ingress::match_route(&routes.read().await, host, path).map(|route| route.backend)
}

/// Pull the request path and `Host` header out of a request head.
fn parse_request(head: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.lines();
    let request_line = lines.next()?;
    // "GET /path HTTP/1.1" — strip any query string from the path.
    let raw_path = request_line.split_whitespace().nth(1)?;
    let path = raw_path
        .split(['?', '#'])
        .next()
        .unwrap_or(raw_path)
        .to_owned();
    let host = lines
        .take_while(|l| !l.is_empty())
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .map(|(_, v)| v.trim().split(':').next().unwrap_or("").to_owned())
        .unwrap_or_default();
    Some((host, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_path_and_host() {
        let head = b"GET /api/v1?x=1 HTTP/1.1\r\nHost: example.com:80\r\nAccept: */*\r\n\r\n";
        let (host, path) = parse_request(head).unwrap();
        assert_eq!(host, "example.com", "port stripped from Host");
        assert_eq!(path, "/api/v1", "query string stripped from path");
    }

    #[test]
    fn missing_host_is_empty_not_an_error() {
        let head = b"GET / HTTP/1.0\r\n\r\n";
        let (host, path) = parse_request(head).unwrap();
        assert_eq!(host, "");
        assert_eq!(path, "/");
    }

    #[test]
    fn error_bodies_match_their_content_length() {
        for resp in [RESP_404, RESP_502] {
            let text = std::str::from_utf8(resp).expect("response is utf-8");
            let (head, body) = text.split_once("\r\n\r\n").expect("blank line separator");
            let declared: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .expect("Content-Length header present")
                .parse()
                .expect("Content-Length is a number");
            assert_eq!(
                declared,
                body.len(),
                "Content-Length must equal the body for {head:?}"
            );
        }
    }
}
