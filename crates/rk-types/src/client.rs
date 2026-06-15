//! The HTTP client every non-API component uses to talk to `rk-api` —
//! the agent, scheduler, controllers and CLI all watch and write through
//! this (feature `client`, so the pure-types consumers like `rk-runtime`
//! don't drag reqwest in).
//!
//! Two surfaces over the same wire calls:
//! - typed, generic over [`crate::Resource`] (`get::<Pod>`, `watch::<Pod>`),
//!   for components that know what they're handling;
//! - raw [`serde_json::Value`] keyed by [`ResourceKind`], for the CLI, which
//!   must round-trip exactly what the user wrote.
//!
//! Watch consumes the API server's SSE stream; the parser lives here too.

use std::collections::VecDeque;
use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt as _;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Resource;
use crate::meta::ResourceKind;
use crate::watch::{List, WatchEvent};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request to {url} failed")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("invalid {what} in response")]
    Decode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("watch stream error: {0}")]
    Watch(String),
    #[error("client configuration: {0}")]
    Config(String),
}

/// Credentials and trust for talking to `rk-api`: a bearer token sent on every
/// request, and the CA certificate (PEM) that signed the server's TLS cert.
/// Both default to `None` — a plaintext, unauthenticated dev cluster.
#[derive(Default, Clone)]
pub struct ClientAuth {
    pub token: Option<String>,
    pub ca_cert_pem: Option<Vec<u8>>,
}

pub type WatchStream<T> = Pin<Box<dyn Stream<Item = Result<WatchEvent<T>, ClientError>> + Send>>;

pub struct ApiClient {
    base: String,
    http: reqwest::Client,
}

/// Namespaced kinds get `/api/v1/namespaces/{ns}/{plural}`; cluster-scoped
/// kinds (and all-namespace listings) the short `/api/v1/{plural}`.
fn collection_path(kind: ResourceKind, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) if kind.namespaced() => format!("/api/v1/namespaces/{ns}/{}", kind.plural()),
        _ => format!("/api/v1/{}", kind.plural()),
    }
}

fn item_path(kind: ResourceKind, namespace: Option<&str>, name: &str) -> String {
    format!("{}/{name}", collection_path(kind, namespace))
}

impl ApiClient {
    /// A client with no credentials and the system trust roots — for tests and
    /// dev clusters with auth and TLS turned off.
    pub fn new(base: impl Into<String>) -> Result<Self, ClientError> {
        Self::with_auth(base, ClientAuth::default())
    }

    /// A client that sends `Authorization: Bearer <token>` on every request (if
    /// a token is set) and trusts `ca_cert_pem` in addition to the system roots,
    /// so it can verify the API server's self-signed TLS certificate.
    pub fn with_auth(base: impl Into<String>, auth: ClientAuth) -> Result<Self, ClientError> {
        let base = base.into().trim_end_matches('/').to_owned();
        let mut builder =
            reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(3));
        if let Some(token) = &auth.token {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| ClientError::Config("auth token has invalid characters".to_owned()))?;
            value.set_sensitive(true);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        if let Some(pem) = &auth.ca_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem)
                .map_err(|err| ClientError::Config(format!("invalid CA certificate: {err}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().map_err(|source| ClientError::Http {
            url: base.clone(),
            source,
        })?;
        Ok(ApiClient { base, http })
    }

    /// Build a client from a base URL and optional token / CA-cert file paths,
    /// as the component configs carry them. A `None` path skips that credential;
    /// a path that can't be read is an error.
    pub fn connect(
        base: impl Into<String>,
        token_file: Option<&std::path::Path>,
        ca_file: Option<&std::path::Path>,
    ) -> Result<Self, ClientError> {
        let token = match token_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|err| {
                        ClientError::Config(format!("reading token {}: {err}", path.display()))
                    })?
                    .trim()
                    .to_owned(),
            ),
            None => None,
        };
        let ca_cert_pem = match ca_file {
            Some(path) => Some(std::fs::read(path).map_err(|err| {
                ClientError::Config(format!("reading CA cert {}: {err}", path.display()))
            })?),
            None => None,
        };
        Self::with_auth(base, ClientAuth { token, ca_cert_pem })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        url: &str,
    ) -> Result<Value, ClientError> {
        let response = request.send().await.map_err(|source| ClientError::Http {
            url: url.to_owned(),
            source,
        })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or(body);
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        serde_json::from_str(&body).map_err(|source| ClientError::Decode {
            what: "response body",
            source,
        })
    }

    fn decode_typed<T: DeserializeOwned>(
        value: Value,
        what: &'static str,
    ) -> Result<T, ClientError> {
        serde_json::from_value(value).map_err(|source| ClientError::Decode { what, source })
    }

    // ---- raw surface (CLI) ----

    pub async fn get_raw(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<Value>, ClientError> {
        let url = self.url(&item_path(kind, namespace, name));
        match self.send(self.http.get(&url), &url).await {
            Ok(value) => Ok(Some(value)),
            Err(ClientError::Api { status: 404, .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn list_raw(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
    ) -> Result<List<Value>, ClientError> {
        let url = self.url(&collection_path(kind, namespace));
        let value = self.send(self.http.get(&url), &url).await?;
        Self::decode_typed(value, "list")
    }

    /// POST to the cluster collection path — the manifest's own
    /// `metadata.namespace` (or the server default) decides where it lands.
    pub async fn create_raw(
        &self,
        kind: ResourceKind,
        object: &Value,
    ) -> Result<Value, ClientError> {
        let url = self.url(&format!("/api/v1/{}", kind.plural()));
        self.send(self.http.post(&url).json(object), &url).await
    }

    pub async fn replace_raw(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
        object: &Value,
    ) -> Result<Value, ClientError> {
        let url = self.url(&item_path(kind, namespace, name));
        self.send(self.http.put(&url).json(object), &url).await
    }

    pub async fn delete_raw(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Value, ClientError> {
        let url = self.url(&item_path(kind, namespace, name));
        self.send(self.http.delete(&url), &url).await
    }

    pub async fn watch_raw(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        since: u64,
    ) -> Result<WatchStream<Value>, ClientError> {
        let mut url = self.url(&format!("/api/v1/watch/{}?since={since}", kind.plural()));
        if let Some(ns) = namespace.filter(|_| kind.namespaced()) {
            url.push_str(&format!("&namespace={ns}"));
        }
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|source| ClientError::Http {
                url: url.clone(),
                source,
            })?;
        if !response.status().is_success() {
            return Err(ClientError::Api {
                status: response.status().as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }

        // bytes -> SSE payloads -> WatchEvents. The parser carries partial
        // events across chunks; `pending` flushes multi-event chunks one
        // item at a time.
        struct WatchState<S> {
            stream: S,
            parser: SseParser,
            pending: VecDeque<String>,
        }
        let state = WatchState {
            stream: response.bytes_stream(),
            parser: SseParser::default(),
            pending: VecDeque::new(),
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(payload) = state.pending.pop_front() {
                    let event =
                        serde_json::from_str::<WatchEvent<Value>>(&payload).map_err(|err| {
                            ClientError::Watch(format!("invalid event {payload:?}: {err}"))
                        });
                    return Some((event, state));
                }
                match state.stream.next().await {
                    Some(Ok(chunk)) => {
                        let text = String::from_utf8_lossy(&chunk);
                        state.pending.extend(state.parser.feed(&text));
                    }
                    Some(Err(err)) => {
                        return Some((Err(ClientError::Watch(err.to_string())), state));
                    }
                    None => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }

    // ---- typed surface (agent, scheduler, controllers) ----

    pub async fn get<R: Resource>(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<R>, ClientError> {
        match self.get_raw(R::KIND, namespace, name).await? {
            Some(value) => Ok(Some(Self::decode_typed(value, "object")?)),
            None => Ok(None),
        }
    }

    pub async fn list<R: Resource>(&self, namespace: Option<&str>) -> Result<List<R>, ClientError> {
        let raw = self.list_raw(R::KIND, namespace).await?;
        let mut items = Vec::with_capacity(raw.items.len());
        for item in raw.items {
            items.push(Self::decode_typed(item, "list item")?);
        }
        Ok(List {
            items,
            resource_version: raw.resource_version,
        })
    }

    pub async fn create<R: Resource>(&self, object: &R) -> Result<R, ClientError> {
        let value = serde_json::to_value(object).map_err(|source| ClientError::Decode {
            what: "object to send",
            source,
        })?;
        let created = self.create_raw(R::KIND, &value).await?;
        Self::decode_typed(created, "created object")
    }

    /// PUT the object at the path its own metadata names. The server enforces
    /// `metadata.resourceVersion` as the optimistic-concurrency token.
    pub async fn replace<R: Resource>(&self, object: &R) -> Result<R, ClientError> {
        let meta = object.metadata();
        let namespace = if R::KIND.namespaced() {
            Some(meta.namespace_or_default().to_owned())
        } else {
            None
        };
        let value = serde_json::to_value(object).map_err(|source| ClientError::Decode {
            what: "object to send",
            source,
        })?;
        let updated = self
            .replace_raw(R::KIND, namespace.as_deref(), &meta.name, &value)
            .await?;
        Self::decode_typed(updated, "updated object")
    }

    pub async fn delete<R: Resource>(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<R, ClientError> {
        let value = self.delete_raw(R::KIND, namespace, name).await?;
        Self::decode_typed(value, "deleted object")
    }

    pub async fn watch<R: Resource>(
        &self,
        namespace: Option<&str>,
        since: u64,
    ) -> Result<WatchStream<R>, ClientError> {
        let raw = self.watch_raw(R::KIND, namespace, since).await?;
        Ok(Box::pin(raw.map(|item| {
            item.and_then(|event| {
                Ok(WatchEvent {
                    event_type: event.event_type,
                    object: Self::decode_typed(event.object, "watched object")?,
                })
            })
        })))
    }

    /// `GET /version` — used by `rk-ctl version`.
    pub async fn server_version(&self) -> Result<Value, ClientError> {
        let url = self.url("/version");
        self.send(self.http.get(&url), &url).await
    }
}

/// A generic list+watch reflector: keep a local map of a resource kind in
/// sync with the API server, waking a [`tokio::sync::Notify`] on every change.
/// Any watch failure or server-side lag falls back to a fresh list, so the
/// cache always re-converges. The agent, scheduler and controllers all build
/// their views on this.
pub mod reflector {
    use std::collections::HashMap;
    use std::sync::Arc;

    use futures_util::StreamExt as _;
    use tokio::sync::{Mutex, Notify};

    use super::ApiClient;
    use crate::Resource;
    use crate::watch::EventType;

    /// Objects keyed by `namespace/name` (or just `name` for cluster-scoped
    /// kinds), the same key the store uses.
    pub type Cache<R> = Arc<Mutex<HashMap<String, R>>>;

    pub fn key_of<R: Resource>(object: &R) -> String {
        let meta = object.metadata();
        match &meta.namespace {
            Some(ns) => format!("{ns}/{}", meta.name),
            None => meta.name.clone(),
        }
    }

    /// Reflect a resource kind into `cache` forever. The first list populates
    /// the cache and fires `notify`; subsequent watch events keep it live.
    pub async fn reflect<R: Resource>(
        client: Arc<ApiClient>,
        cache: Cache<R>,
        notify: Arc<Notify>,
    ) {
        let kind = R::KIND;
        loop {
            let list = match client.list::<R>(None).await {
                Ok(list) => list,
                Err(err) => {
                    tracing::warn!(%kind, error = %err, "list failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            let since = list.resource_version.unwrap_or(0);
            {
                let mut map = cache.lock().await;
                map.clear();
                for object in list.items {
                    map.insert(key_of(&object), object);
                }
                tracing::debug!(%kind, count = map.len(), "cache rebuilt from list");
            }
            notify.notify_one();

            let mut stream = match client.watch::<R>(None, since).await {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!(%kind, error = %err, "watch failed; re-listing");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            while let Some(item) = stream.next().await {
                let event = match item {
                    Ok(event) => event,
                    Err(err) => {
                        tracing::warn!(%kind, error = %err, "watch stream error; re-listing");
                        break;
                    }
                };
                let key = key_of(&event.object);
                {
                    let mut map = cache.lock().await;
                    match event.event_type {
                        EventType::Added | EventType::Modified => {
                            map.insert(key, event.object);
                        }
                        EventType::Deleted => {
                            map.remove(&key);
                        }
                    }
                }
                notify.notify_one();
            }
            tracing::info!(%kind, "watch ended; re-listing");
        }
    }

    /// Snapshot a cache's values, cloned, for a pass of pure logic.
    pub async fn snapshot<R: Resource>(cache: &Cache<R>) -> Vec<R> {
        cache.lock().await.values().cloned().collect()
    }
}

/// Minimal Server-Sent-Events parser: feed it chunks, get `data:` payloads
/// out. Handles events split across chunks, multi-line data, CRLF, and
/// ignores comments/other fields — all the SSE this client needs.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
}

impl SseParser {
    /// Feed one chunk; returns the data payload of every event completed by
    /// this chunk, in order.
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        // Normalize CRLF up front so the blank-line scan below only has one
        // separator to find. A `\r\n` split across chunks resolves once the
        // `\n` arrives, because the dangling `\r` is still in the buffer.
        self.buffer.push_str(chunk);
        self.buffer = self.buffer.replace("\r\n", "\n");
        let mut events = Vec::new();

        // An SSE event ends at a blank line.
        while let Some(boundary) = self.buffer.find("\n\n") {
            let event: String = self.buffer.drain(..boundary + 2).collect();
            let mut data_lines = Vec::new();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.strip_prefix(' ').unwrap_or(data));
                }
            }
            if !data_lines.is_empty() {
                events.push(data_lines.join("\n"));
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_respect_resource_scope() {
        assert_eq!(
            collection_path(ResourceKind::Pod, Some("prod")),
            "/api/v1/namespaces/prod/pods"
        );
        assert_eq!(collection_path(ResourceKind::Pod, None), "/api/v1/pods");
        // Cluster-scoped kinds ignore a namespace argument entirely.
        assert_eq!(
            collection_path(ResourceKind::Node, Some("prod")),
            "/api/v1/nodes"
        );
        assert_eq!(
            item_path(ResourceKind::Namespace, None, "prod"),
            "/api/v1/namespaces/prod"
        );
        assert_eq!(
            item_path(ResourceKind::Pod, Some("default"), "web-0"),
            "/api/v1/namespaces/default/pods/web-0"
        );
    }

    #[test]
    fn sse_events_split_across_chunks_reassemble() {
        let mut parser = SseParser::default();
        assert!(parser.feed("data: {\"a\":").is_empty());
        assert_eq!(parser.feed("1}\n\n"), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn sse_multiple_events_in_one_chunk_come_out_in_order() {
        let mut parser = SseParser::default();
        let events = parser.feed("data: one\n\ndata: two\n\n");
        assert_eq!(events, vec!["one", "two"]);
    }

    #[test]
    fn sse_keep_alive_comments_are_ignored() {
        let mut parser = SseParser::default();
        assert!(parser.feed(": keep-alive\n\n").is_empty());
        assert_eq!(parser.feed("event: message\ndata: x\n\n"), vec!["x"]);
    }

    #[test]
    fn sse_crlf_streams_parse_too() {
        let mut parser = SseParser::default();
        assert_eq!(parser.feed("data: x\r\n\r\n"), vec!["x"]);

        // A CRLF pair split across chunks still resolves.
        let mut split = SseParser::default();
        assert!(split.feed("data: y\r\n\r").is_empty());
        assert_eq!(split.feed("\n"), vec!["y"]);
    }
}
