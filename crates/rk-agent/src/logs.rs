//! A tiny HTTP endpoint that serves a pod's container logs from this node, so
//! `rk-ctl logs` can fetch them (the kubelet-equivalent log API). Logs live on
//! the node — the runtime captures each container's stdout/stderr to
//! `console.log` — so the CLI resolves a pod to its node and reads from here.
//!
//! `GET /logs/{namespace}/{name}` → the container's `console.log`, or 404.
//! Just enough HTTP/1.1 by hand to avoid pulling a server framework into the
//! agent.

use std::path::PathBuf;
use std::sync::Arc;

use rk_runtime::store::Store;
use rk_types::Pod;
use rk_types::client::ApiClient;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// The port the agent serves logs on (kubelet uses 10250).
pub const LOG_PORT: u16 = 10250;

/// Locate a pod's `console.log` on this node by its UID. Containers are named
/// `p<uid12>-<container>`, so the UID prefix finds the directory.
fn console_log_path(store: &Store, uid: &str) -> Option<PathBuf> {
    let uid12: String = uid.chars().take(12).collect();
    let prefix = format!("p{uid12}-");
    let entries = std::fs::read_dir(store.containers_dir()).ok()?;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let path = entry.path().join("console.log");
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let line = head.lines().next()?;
    // "GET /logs/ns/name HTTP/1.1"
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    Some(parts.next()?.to_owned())
}

async fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(body).await;
}

async fn handle(mut stream: TcpStream, client: Arc<ApiClient>, store: Arc<Store>) {
    let Some(target) = read_request_target(&mut stream).await else {
        respond(&mut stream, "400 Bad Request", b"bad request\n").await;
        return;
    };
    // Path: /logs/{namespace}/{name}
    let segments: Vec<&str> = target.trim_start_matches('/').split('/').collect();
    let [_logs, namespace, name] = segments.as_slice() else {
        respond(
            &mut stream,
            "404 Not Found",
            b"usage: GET /logs/{namespace}/{name}\n",
        )
        .await;
        return;
    };

    let uid = match client.get::<Pod>(Some(namespace), name).await {
        Ok(Some(pod)) => pod.metadata.uid,
        _ => None,
    };
    let body = uid
        .and_then(|uid| console_log_path(&store, &uid))
        .and_then(|path| std::fs::read(path).ok());

    match body {
        Some(bytes) => respond(&mut stream, "200 OK", &bytes).await,
        None => {
            respond(
                &mut stream,
                "404 Not Found",
                b"no logs for that pod on this node\n",
            )
            .await
        }
    }
}

/// Serve the log endpoint forever on `0.0.0.0:LOG_PORT`.
pub async fn serve(client: Arc<ApiClient>, store: Arc<Store>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", LOG_PORT)).await?;
    tracing::info!(port = LOG_PORT, "pod log endpoint listening");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let client = client.clone();
        let store = store.clone();
        tokio::spawn(handle(stream, client, store));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_console_log_by_uid_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure_layout().unwrap();
        let cdir = store.container_dir("p4f9d2c81aa55-nginx");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("console.log"), b"hello logs").unwrap();

        let found = console_log_path(&store, "4f9d2c81aa55b2e7c01d9f33ab12cd34").unwrap();
        assert_eq!(std::fs::read(found).unwrap(), b"hello logs");
        assert!(console_log_path(&store, "ffffffffffff0000").is_none());
    }
}
