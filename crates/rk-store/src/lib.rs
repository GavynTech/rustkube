//! Storage contract for the RustKube control plane.
//!
//! `rk-api` is the ONLY component that talks to a [`Store`]; everything else
//! goes through the API server. The trait is object-safe (`Arc<dyn Store>`)
//! so the sqlite implementation (Phase 2) can later be swapped for etcd
//! without touching the API server.
//!
//! Concurrency model: the store maintains a single monotonically increasing
//! `revision` counter. Every successful `put`/`delete` bumps it, the new
//! revision is stamped onto the object, and watchers receive events ordered
//! by revision. Conditional writes (`expected_revision`) give the API server
//! optimistic concurrency for resource versions.

use std::pin::Pin;

use futures_core::Stream;
use rk_types::ResourceKind;
use serde::{Deserialize, Serialize};

/// A resource as the store sees it: identity, revision, and the full object
/// as JSON. The store never interprets `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredObject {
    pub kind: ResourceKind,
    /// `None` for cluster-scoped kinds (Node, Namespace).
    pub namespace: Option<String>,
    pub name: String,
    /// Store revision at which this version of the object was written.
    pub revision: u64,
    pub data: serde_json::Value,
}

impl StoredObject {
    /// `namespace/name` (or just `name` for cluster-scoped objects), used in
    /// errors and logs.
    pub fn key(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreEventKind {
    /// Insert or update; `Added` vs `Modified` is derived by the API layer.
    Put,
    Delete,
}

#[derive(Debug, Clone)]
pub struct StoreEvent {
    pub kind: StoreEventKind,
    pub object: StoredObject,
}

pub type StoreEvents = Pin<Box<dyn Stream<Item = StoreEvent> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{kind} {key:?} not found")]
    NotFound { kind: ResourceKind, key: String },
    #[error("revision conflict on {key:?}: expected {expected}, current is {actual}")]
    Conflict {
        key: String,
        expected: u64,
        actual: u64,
    },
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("storage backend error: {0}")]
    Backend(String),
}

#[async_trait::async_trait]
pub trait Store: Send + Sync + 'static {
    /// Insert or update an object. With `expected_revision: Some(rev)` the
    /// write only succeeds if the object's current revision is exactly `rev`
    /// (optimistic concurrency); `None` writes unconditionally. Returns the
    /// object stamped with its new revision.
    async fn put(
        &self,
        object: StoredObject,
        expected_revision: Option<u64>,
    ) -> Result<StoredObject, StoreError>;

    async fn get(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<StoredObject>, StoreError>;

    /// List objects of a kind, optionally filtered to one namespace. Returns
    /// the items plus the store revision the listing was taken at, so a
    /// caller can `watch` from that revision and miss nothing.
    async fn list(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
    ) -> Result<(Vec<StoredObject>, u64), StoreError>;

    /// Delete an object, with the same `expected_revision` semantics as
    /// [`Store::put`]. Returns the deleted object.
    async fn delete(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
        expected_revision: Option<u64>,
    ) -> Result<StoredObject, StoreError>;

    /// Stream every event with revision greater than `since`, then keep
    /// streaming live events. `kind: None` watches all kinds.
    async fn watch(
        &self,
        kind: Option<ResourceKind>,
        since: u64,
    ) -> Result<StoreEvents, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_object_key_includes_namespace_when_present() {
        let mut object = StoredObject {
            kind: ResourceKind::Pod,
            namespace: Some("default".to_owned()),
            name: "web-0".to_owned(),
            revision: 7,
            data: serde_json::json!({}),
        };
        assert_eq!(object.key(), "default/web-0");
        object.namespace = None;
        assert_eq!(object.key(), "web-0");
    }

    #[test]
    fn stored_object_round_trips_through_json() {
        let object = StoredObject {
            kind: ResourceKind::Service,
            namespace: Some("prod".to_owned()),
            name: "web".to_owned(),
            revision: 42,
            data: serde_json::json!({"spec": {"ports": [{"port": 80}]}}),
        };
        let json = serde_json::to_string(&object).unwrap();
        let back: StoredObject = serde_json::from_str(&json).unwrap();
        assert_eq!(object, back);
    }
}
