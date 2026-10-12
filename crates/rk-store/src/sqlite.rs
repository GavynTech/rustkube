//! The sqlite [`Store`] implementation.
//!
//! Three tables: `objects` (current state, one row per live object),
//! `events` (the append-only change log watches replay from), and
//! `revision` (the single global counter). Every write bumps the counter,
//! stamps the new revision into the object's `metadata.resourceVersion` —
//! the one piece of `data` the store is allowed to touch — and appends an
//! event row in the same transaction, so the log can never disagree with
//! the state.
//!
//! Watches subscribe to a broadcast channel *before* querying the backlog,
//! then drop live events at or below the last replayed revision — no gap,
//! no duplicates. A watcher that falls so far behind the channel laps it is
//! cut off; clients re-list and re-watch, exactly like a Kubernetes client
//! handling a 410.

use std::path::Path;
use std::str::FromStr;

use futures_util::StreamExt as _;
use rk_types::ResourceKind;
use sqlx::Row as _;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use tokio::sync::broadcast;

use crate::{Store, StoreError, StoreEvent, StoreEventKind, StoreEvents, StoredObject};

/// Capacity of the live-event fan-out. A watcher this far behind is cut off
/// and must re-list.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

pub struct SqliteStore {
    pool: SqlitePool,
    events_tx: broadcast::Sender<StoreEvent>,
}

impl SqliteStore {
    /// Open (creating if missing) a file-backed store.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        Self::from_options(options).await
    }

    /// In-memory store for tests. State dies with the value.
    pub async fn in_memory() -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        Self::from_options(options).await
    }

    async fn from_options(options: SqliteConnectOptions) -> Result<Self, StoreError> {
        // One connection: sqlite serializes writes anyway at this scale, and
        // a single connection is what makes `:memory:` databases coherent.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(backend)?;
        let store = SqliteStore {
            pool,
            events_tx: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
        };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS revision (
                id      INTEGER PRIMARY KEY CHECK (id = 1),
                counter INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO revision (id, counter) VALUES (1, 0);
            CREATE TABLE IF NOT EXISTS objects (
                kind      TEXT NOT NULL,
                namespace TEXT NOT NULL DEFAULT '',
                name      TEXT NOT NULL,
                revision  INTEGER NOT NULL,
                data      TEXT NOT NULL,
                PRIMARY KEY (kind, namespace, name)
            );
            CREATE TABLE IF NOT EXISTS events (
                revision       INTEGER PRIMARY KEY,
                kind           TEXT NOT NULL,
                namespace      TEXT NOT NULL DEFAULT '',
                name           TEXT NOT NULL,
                event          TEXT NOT NULL CHECK (event IN ('put', 'delete')),
                prior_revision INTEGER,
                data           TEXT NOT NULL
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

fn backend(err: sqlx::Error) -> StoreError {
    StoreError::Backend(err.to_string())
}

fn to_u64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Backend(format!("negative revision {value}")))
}

fn to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Backend(format!("revision {value} overflows")))
}

/// `''` is the namespace column value for cluster-scoped objects — sqlite
/// PRIMARY KEY columns reject NULL comparisons the way we'd want them.
fn ns_column(namespace: Option<&str>) -> &str {
    namespace.unwrap_or("")
}

fn kind_from_column(raw: &str) -> Result<ResourceKind, StoreError> {
    raw.parse()
        .map_err(|_| StoreError::Backend(format!("unknown kind {raw:?} in database")))
}

fn object_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<StoredObject, StoreError> {
    let kind: String = row.get("kind");
    let namespace: String = row.get("namespace");
    let name: String = row.get("name");
    let revision: i64 = row.get("revision");
    let data: String = row.get("data");
    Ok(StoredObject {
        kind: kind_from_column(&kind)?,
        namespace: (!namespace.is_empty()).then_some(namespace),
        name,
        revision: to_u64(revision)?,
        data: serde_json::from_str(&data)?,
    })
}

/// Stamp the store revision into `metadata.resourceVersion` so the stored
/// JSON, list responses and watch events all agree on the object's version.
fn stamp_resource_version(data: &mut serde_json::Value, revision: u64) {
    if let Some(metadata) = data.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        metadata.insert("resourceVersion".to_owned(), revision.into());
    }
}

#[async_trait::async_trait]
impl Store for SqliteStore {
    async fn put(
        &self,
        mut object: StoredObject,
        expected_revision: Option<u64>,
    ) -> Result<StoredObject, StoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;

        let current: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM objects WHERE kind = ? AND namespace = ? AND name = ?",
        )
        .bind(object.kind.as_str())
        .bind(ns_column(object.namespace.as_deref()))
        .bind(&object.name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;

        let prior_revision = match current {
            Some(rev) => Some(to_u64(rev)?),
            None => None,
        };
        check_expectation(expected_revision, prior_revision, &object)?;

        let revision: i64 = sqlx::query_scalar(
            "UPDATE revision SET counter = counter + 1 WHERE id = 1 RETURNING counter",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        let revision = to_u64(revision)?;

        stamp_resource_version(&mut object.data, revision);
        object.revision = revision;
        let data = serde_json::to_string(&object.data)?;

        sqlx::query(
            "INSERT OR REPLACE INTO objects (kind, namespace, name, revision, data) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(object.kind.as_str())
        .bind(ns_column(object.namespace.as_deref()))
        .bind(&object.name)
        .bind(to_i64(revision)?)
        .bind(&data)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;

        sqlx::query(
            "INSERT INTO events (revision, kind, namespace, name, event, prior_revision, data) \
             VALUES (?, ?, ?, ?, 'put', ?, ?)",
        )
        .bind(to_i64(revision)?)
        .bind(object.kind.as_str())
        .bind(ns_column(object.namespace.as_deref()))
        .bind(&object.name)
        .bind(match prior_revision {
            Some(rev) => Some(to_i64(rev)?),
            None => None,
        })
        .bind(&data)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;

        tx.commit().await.map_err(backend)?;

        // Nobody listening is fine; watches replay from the event log.
        let _ = self.events_tx.send(StoreEvent {
            kind: StoreEventKind::Put,
            prior_revision,
            object: object.clone(),
        });
        Ok(object)
    }

    async fn get(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<StoredObject>, StoreError> {
        let row = sqlx::query(
            "SELECT kind, namespace, name, revision, data FROM objects \
             WHERE kind = ? AND namespace = ? AND name = ?",
        )
        .bind(kind.as_str())
        .bind(ns_column(namespace))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.as_ref().map(object_from_row).transpose()
    }

    async fn list(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
    ) -> Result<(Vec<StoredObject>, u64), StoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let rows = match namespace {
            Some(ns) => {
                sqlx::query(
                    "SELECT kind, namespace, name, revision, data FROM objects \
                     WHERE kind = ? AND namespace = ? ORDER BY namespace, name",
                )
                .bind(kind.as_str())
                .bind(ns)
                .fetch_all(&mut *tx)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT kind, namespace, name, revision, data FROM objects \
                     WHERE kind = ? ORDER BY namespace, name",
                )
                .bind(kind.as_str())
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(backend)?;

        // Same transaction as the listing, so the revision is exactly the
        // point a watcher should resume from.
        let revision: i64 = sqlx::query_scalar("SELECT counter FROM revision WHERE id = 1")
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;

        let objects = rows
            .iter()
            .map(object_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((objects, to_u64(revision)?))
    }

    async fn delete(
        &self,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
        expected_revision: Option<u64>,
    ) -> Result<StoredObject, StoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;

        let row = sqlx::query(
            "SELECT kind, namespace, name, revision, data FROM objects \
             WHERE kind = ? AND namespace = ? AND name = ?",
        )
        .bind(kind.as_str())
        .bind(ns_column(namespace))
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        let Some(row) = row else {
            return Err(StoreError::NotFound {
                kind,
                key: key_of(namespace, name),
            });
        };
        let mut object = object_from_row(&row)?;
        let prior_revision = object.revision;

        if let Some(expected) = expected_revision
            && expected != prior_revision
        {
            return Err(StoreError::Conflict {
                key: object.key(),
                expected,
                actual: prior_revision,
            });
        }

        let revision: i64 = sqlx::query_scalar(
            "UPDATE revision SET counter = counter + 1 WHERE id = 1 RETURNING counter",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        let revision = to_u64(revision)?;

        sqlx::query("DELETE FROM objects WHERE kind = ? AND namespace = ? AND name = ?")
            .bind(kind.as_str())
            .bind(ns_column(namespace))
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;

        let data = serde_json::to_string(&object.data)?;
        sqlx::query(
            "INSERT INTO events (revision, kind, namespace, name, event, prior_revision, data) \
             VALUES (?, ?, ?, ?, 'delete', ?, ?)",
        )
        .bind(to_i64(revision)?)
        .bind(kind.as_str())
        .bind(ns_column(namespace))
        .bind(name)
        .bind(to_i64(prior_revision)?)
        .bind(&data)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;

        tx.commit().await.map_err(backend)?;

        object.revision = revision;
        let _ = self.events_tx.send(StoreEvent {
            kind: StoreEventKind::Delete,
            prior_revision: Some(prior_revision),
            object: object.clone(),
        });
        Ok(object)
    }

    async fn watch(
        &self,
        kind: Option<ResourceKind>,
        since: u64,
    ) -> Result<StoreEvents, StoreError> {
        // Subscribe before reading the backlog: anything committed during
        // the query shows up on the channel and is deduped by revision.
        let rx = self.events_tx.subscribe();

        let rows = match kind {
            Some(kind) => {
                sqlx::query(
                    "SELECT revision, kind, namespace, name, event, prior_revision, data \
                     FROM events WHERE revision > ? AND kind = ? ORDER BY revision",
                )
                .bind(to_i64(since)?)
                .bind(kind.as_str())
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT revision, kind, namespace, name, event, prior_revision, data \
                     FROM events WHERE revision > ? ORDER BY revision",
                )
                .bind(to_i64(since)?)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(backend)?;

        let mut backlog = Vec::with_capacity(rows.len());
        for row in &rows {
            backlog.push(event_from_row(row)?);
        }
        let mut last_seen = backlog.last().map_or(since, |ev| ev.object.revision);

        let live = futures_util::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Ok(event) => Some((event, rx)),
                // Lapped: this watcher can no longer be gapless. End the
                // stream; the client re-lists and re-watches, like a
                // Kubernetes client handling a 410.
                Err(broadcast::error::RecvError::Lagged(_))
                | Err(broadcast::error::RecvError::Closed) => None,
            }
        })
        .filter_map(move |event| {
            let fresh = event.object.revision > last_seen;
            let kind_matches = kind.is_none_or(|k| event.object.kind == k);
            if fresh {
                last_seen = event.object.revision;
            }
            std::future::ready((fresh && kind_matches).then_some(event))
        });

        Ok(Box::pin(futures_util::stream::iter(backlog).chain(live)))
    }
}

fn key_of(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{ns}/{name}"),
        None => name.to_owned(),
    }
}

fn check_expectation(
    expected: Option<u64>,
    current: Option<u64>,
    object: &StoredObject,
) -> Result<(), StoreError> {
    match (expected, current) {
        (None, _) => Ok(()),
        // Some(0) is "must not exist" — revisions start at 1.
        (Some(0), None) => Ok(()),
        (Some(0), Some(actual)) => Err(StoreError::Conflict {
            key: object.key(),
            expected: 0,
            actual,
        }),
        (Some(_), None) => Err(StoreError::NotFound {
            kind: object.kind,
            key: object.key(),
        }),
        (Some(expected), Some(actual)) if expected != actual => Err(StoreError::Conflict {
            key: object.key(),
            expected,
            actual,
        }),
        (Some(_), Some(_)) => Ok(()),
    }
}

fn event_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<StoreEvent, StoreError> {
    let revision: i64 = row.get("revision");
    let kind: String = row.get("kind");
    let namespace: String = row.get("namespace");
    let name: String = row.get("name");
    let event: String = row.get("event");
    let prior_revision: Option<i64> = row.get("prior_revision");
    let data: String = row.get("data");

    Ok(StoreEvent {
        kind: match event.as_str() {
            "put" => StoreEventKind::Put,
            "delete" => StoreEventKind::Delete,
            other => {
                return Err(StoreError::Backend(format!(
                    "unknown event type {other:?} in database"
                )));
            }
        },
        prior_revision: match prior_revision {
            Some(rev) => Some(to_u64(rev)?),
            None => None,
        },
        object: StoredObject {
            kind: kind_from_column(&kind)?,
            namespace: (!namespace.is_empty()).then_some(namespace),
            name,
            revision: to_u64(revision)?,
            data: serde_json::from_str(&data)?,
        },
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    fn pod(namespace: &str, name: &str) -> StoredObject {
        StoredObject {
            kind: ResourceKind::Pod,
            namespace: Some(namespace.to_owned()),
            name: name.to_owned(),
            revision: 0,
            data: serde_json::json!({
                "kind": "Pod",
                "metadata": { "name": name, "namespace": namespace },
            }),
        }
    }

    #[tokio::test]
    async fn put_stamps_monotonic_revisions_and_resource_version() {
        let store = SqliteStore::in_memory().await.unwrap();
        let first = store.put(pod("default", "a"), None).await.unwrap();
        let second = store.put(pod("default", "b"), None).await.unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        assert_eq!(first.data["metadata"]["resourceVersion"], 1);

        let updated = store.put(pod("default", "a"), None).await.unwrap();
        assert_eq!(updated.revision, 3);
        let fetched = store
            .get(ResourceKind::Pod, Some("default"), "a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.revision, 3);
        assert_eq!(fetched.data["metadata"]["resourceVersion"], 3);
    }

    #[tokio::test]
    async fn create_semantics_via_expected_zero() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.put(pod("default", "a"), Some(0)).await.unwrap();
        let err = store.put(pod("default", "a"), Some(0)).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict { actual: 1, .. }));
    }

    #[tokio::test]
    async fn conditional_put_enforces_revisions() {
        let store = SqliteStore::in_memory().await.unwrap();
        let stored = store.put(pod("default", "a"), None).await.unwrap();

        let err = store
            .put(pod("default", "a"), Some(stored.revision + 5))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }));

        store
            .put(pod("default", "a"), Some(stored.revision))
            .await
            .unwrap();

        let err = store
            .put(pod("default", "ghost"), Some(7))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn list_filters_by_namespace_and_reports_revision() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.put(pod("default", "a"), None).await.unwrap();
        store.put(pod("prod", "b"), None).await.unwrap();

        let (all, revision) = store.list(ResourceKind::Pod, None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(revision, 2);

        let (prod, _) = store.list(ResourceKind::Pod, Some("prod")).await.unwrap();
        assert_eq!(prod.len(), 1);
        assert_eq!(prod[0].name, "b");

        let (none, revision) = store.list(ResourceKind::Service, None).await.unwrap();
        assert!(none.is_empty());
        assert_eq!(revision, 2, "empty lists still report the revision");
    }

    #[tokio::test]
    async fn delete_returns_the_object_and_missing_is_not_found() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.put(pod("default", "a"), None).await.unwrap();

        let deleted = store
            .delete(ResourceKind::Pod, Some("default"), "a", None)
            .await
            .unwrap();
        assert_eq!(deleted.name, "a");
        assert_eq!(deleted.revision, 2, "delete consumes a revision");

        let err = store
            .delete(ResourceKind::Pod, Some("default"), "a", None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn watch_replays_backlog_then_streams_live() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.put(pod("default", "a"), None).await.unwrap(); // rev 1
        store.put(pod("default", "b"), None).await.unwrap(); // rev 2

        let mut watch = store.watch(Some(ResourceKind::Pod), 1).await.unwrap();

        // Backlog: only revision 2 (since = 1).
        let event = watch.next().await.unwrap();
        assert_eq!(event.object.revision, 2);
        assert_eq!(event.object.name, "b");
        assert!(event.prior_revision.is_none());

        // Live: an update and a delete arrive in order with prior revisions.
        store.put(pod("default", "a"), None).await.unwrap(); // rev 3
        store
            .delete(ResourceKind::Pod, Some("default"), "a", None)
            .await
            .unwrap(); // rev 4

        let event = watch.next().await.unwrap();
        assert_eq!(event.object.revision, 3);
        assert_eq!(event.prior_revision, Some(1));
        assert!(matches!(event.kind, StoreEventKind::Put));

        let event = watch.next().await.unwrap();
        assert_eq!(event.object.revision, 4);
        assert!(matches!(event.kind, StoreEventKind::Delete));
    }

    #[tokio::test]
    async fn watch_filters_by_kind() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut node = pod("", "n1");
        node.kind = ResourceKind::Node;
        node.namespace = None;

        store.put(node, None).await.unwrap(); // rev 1, Node
        store.put(pod("default", "a"), None).await.unwrap(); // rev 2, Pod

        let mut watch = store.watch(Some(ResourceKind::Pod), 0).await.unwrap();
        let event = watch.next().await.unwrap();
        assert_eq!(event.object.kind, ResourceKind::Pod);
        assert_eq!(event.object.revision, 2);

        // Live: a Node event must not leak into a Pod watch.
        let mut node2 = pod("", "n2");
        node2.kind = ResourceKind::Node;
        node2.namespace = None;
        store.put(node2, None).await.unwrap(); // rev 3
        store.put(pod("default", "b"), None).await.unwrap(); // rev 4

        let event = watch.next().await.unwrap();
        assert_eq!(event.object.kind, ResourceKind::Pod);
        assert_eq!(event.object.revision, 4);
    }

    #[tokio::test]
    async fn file_backed_store_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rustkube.db");

        {
            let store = SqliteStore::open(&path).await.unwrap();
            store.put(pod("default", "a"), None).await.unwrap();
        }

        let store = SqliteStore::open(&path).await.unwrap();
        let fetched = store
            .get(ResourceKind::Pod, Some("default"), "a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.revision, 1);

        // The revision counter continues, not restarts.
        let next = store.put(pod("default", "b"), None).await.unwrap();
        assert_eq!(next.revision, 2);
    }

    #[tokio::test]
    async fn cluster_scoped_objects_use_no_namespace() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut node = pod("", "worker-1");
        node.kind = ResourceKind::Node;
        node.namespace = None;
        store.put(node, None).await.unwrap();

        let fetched = store
            .get(ResourceKind::Node, None, "worker-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.namespace, None);
        assert_eq!(fetched.key(), "worker-1");
    }
}
