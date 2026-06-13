//! A generic list+watch reflector: keep a local map of a resource kind in
//! sync with the API server, and wake a [`Notify`] on every change. Any watch
//! failure or server-side lag falls back to a fresh list, so the cache always
//! re-converges. Used here for both Nodes and Pods; reusable for any
//! [`Resource`].

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt as _;
use rk_types::Resource;
use rk_types::client::ApiClient;
use rk_types::watch::EventType;
use tokio::sync::{Mutex, Notify};

/// Objects keyed by `namespace/name` (or just `name` for cluster-scoped
/// kinds), the same key the store uses.
pub type Cache<R> = Arc<Mutex<HashMap<String, R>>>;

fn key_of<R: Resource>(object: &R) -> String {
    let meta = object.metadata();
    match &meta.namespace {
        Some(ns) => format!("{ns}/{}", meta.name),
        None => meta.name.clone(),
    }
}

/// Reflect a resource kind into `cache` forever. The first list populates the
/// cache and fires `notify`; subsequent watch events keep it live.
pub async fn reflect<R: Resource>(client: Arc<ApiClient>, cache: Cache<R>, notify: Arc<Notify>) {
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
