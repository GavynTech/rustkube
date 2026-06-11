//! Thin typed HTTP client for the rk-api REST surface. Server error bodies
//! (`{"error": "..."}`) become the error message the user sees.

use anyhow::{Context as _, anyhow, bail};
use futures_util::StreamExt as _;
use rk_types::{List, ResourceKind, WatchEvent};
use serde_json::Value;

use crate::sse::SseParser;

pub struct ApiClient {
    base: String,
    http: reqwest::Client,
}

/// Namespaced kinds take an item namespace; cluster-scoped kinds take none.
/// Centralized so every caller builds the same paths.
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
    pub fn new(base: impl Into<String>) -> anyhow::Result<Self> {
        Ok(ApiClient {
            base: base.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .build()
                .context("building HTTP client")?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Decode a 2xx as JSON; turn anything else into the server's error
    /// message (or the raw body when it isn't the standard shape).
    async fn decode(response: reqwest::Response) -> anyhow::Result<Value> {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str(&body)
                .with_context(|| format!("invalid JSON in {status} response"));
        }
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or(body);
        bail!("{status}: {message}")
    }

    pub async fn get_item(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
    ) -> anyhow::Result<Option<Value>> {
        let response = self
            .http
            .get(self.url(&item_path(kind, namespace, name)))
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::decode(response).await?))
    }

    pub async fn list(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
    ) -> anyhow::Result<List<Value>> {
        let response = self
            .http
            .get(self.url(&collection_path(kind, namespace)))
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        let value = Self::decode(response).await?;
        serde_json::from_value(value).context("invalid list response")
    }

    /// POST to the cluster collection path — the manifest's own
    /// `metadata.namespace` (or the server default) decides where it lands.
    pub async fn create(&self, kind: ResourceKind, object: &Value) -> anyhow::Result<Value> {
        let response = self
            .http
            .post(self.url(&format!("/api/v1/{}", kind.plural())))
            .json(object)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        Self::decode(response).await
    }

    pub async fn update(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
        object: &Value,
    ) -> anyhow::Result<Value> {
        let response = self
            .http
            .put(self.url(&item_path(kind, namespace, name)))
            .json(object)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        Self::decode(response).await
    }

    pub async fn delete(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
    ) -> anyhow::Result<Value> {
        let response = self
            .http
            .delete(self.url(&item_path(kind, namespace, name)))
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        Self::decode(response).await
    }

    /// Open a watch and invoke `on_event` for each event. Returns when the
    /// server closes the stream (e.g. a lagged watch) — callers re-list.
    pub async fn watch(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        since: u64,
        mut on_event: impl FnMut(WatchEvent<Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut url = format!("{}/api/v1/watch/{}?since={since}", self.base, kind.plural());
        if let Some(ns) = namespace {
            url.push_str(&format!("&namespace={ns}"));
        }
        let response = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        if !response.status().is_success() {
            return Err(anyhow!("watch failed: {}", response.status()));
        }

        let mut parser = SseParser::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("watch stream broke")?;
            for payload in parser.feed(&String::from_utf8_lossy(&chunk)) {
                let event: WatchEvent<Value> = serde_json::from_str(&payload)
                    .with_context(|| format!("invalid watch event: {payload}"))?;
                on_event(event)?;
            }
        }
        Ok(())
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
}
