//! The watch endpoint: store events as Server-Sent Events.
//!
//! `GET /api/v1/watch` streams every kind; `GET /api/v1/watch/{plural}` one
//! kind. `?since=<revision>` resumes after a known revision (clients pass a
//! list's `resourceVersion` to see exactly what changed since that list);
//! `?namespace=<ns>` filters server-side. Each SSE `data:` line is one
//! [`WatchEvent`] in the Kubernetes wire shape:
//!
//! ```text
//! data: {"type":"ADDED","object":{...}}
//! ```
//!
//! SSE over plain HTTP/1.1 keeps the client side trivial (a line reader) —
//! agents and controllers from Phase 3 on, and `rk-ctl --watch` today.

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use futures_util::StreamExt as _;
use rk_store::{StoreEvent, StoreEventKind};
use rk_types::{EventType, WatchEvent};
use serde::Deserialize;

use crate::error::ApiError;
use crate::resources::{AppState, parse_kind};

#[derive(Debug, Default, Deserialize)]
pub struct WatchParams {
    #[serde(default)]
    pub since: u64,
    pub namespace: Option<String>,
}

/// `ADDED` for a put that created the object, `MODIFIED` for one that
/// replaced it, `DELETED` for deletes — straight from `prior_revision`.
fn event_type(event: &StoreEvent) -> EventType {
    match (event.kind, event.prior_revision) {
        (StoreEventKind::Put, None) => EventType::Added,
        (StoreEventKind::Put, Some(_)) => EventType::Modified,
        (StoreEventKind::Delete, _) => EventType::Deleted,
    }
}

fn to_sse(event: StoreEvent) -> Result<Event, Infallible> {
    let watch_event = WatchEvent {
        event_type: event_type(&event),
        object: event.object.data,
    };
    Ok(Event::default()
        .json_data(&watch_event)
        .unwrap_or_else(|err| {
            // A stored object that fails to re-serialize is a bug, not a client
            // problem; surface it in-band rather than killing the stream.
            Event::default()
                .event("error")
                .data(format!("serialization error: {err}"))
        }))
}

async fn watch_stream(
    state: AppState,
    kind: Option<rk_types::ResourceKind>,
    params: WatchParams,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let events = state.store.watch(kind, params.since).await?;
    let namespace = params.namespace;
    let stream = events
        .filter(move |event| {
            let keep = match &namespace {
                Some(ns) => event.object.namespace.as_deref() == Some(ns.as_str()),
                None => true,
            };
            std::future::ready(keep)
        })
        .map(to_sse);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn watch_all(
    State(state): State<AppState>,
    Query(params): Query<WatchParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    watch_stream(state, None, params).await
}

pub async fn watch_kind(
    State(state): State<AppState>,
    Path(plural): Path<String>,
    Query(params): Query<WatchParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let kind = parse_kind(&plural)?;
    watch_stream(state, Some(kind), params).await
}

#[cfg(test)]
mod tests {
    use rk_store::StoredObject;
    use rk_types::ResourceKind;

    use super::*;

    fn store_event(kind: StoreEventKind, prior: Option<u64>) -> StoreEvent {
        StoreEvent {
            kind,
            prior_revision: prior,
            object: StoredObject {
                kind: ResourceKind::Pod,
                namespace: Some("default".to_owned()),
                name: "web".to_owned(),
                revision: 9,
                data: serde_json::json!({"kind": "Pod"}),
            },
        }
    }

    #[test]
    fn event_types_derive_from_prior_revision() {
        assert_eq!(
            event_type(&store_event(StoreEventKind::Put, None)),
            EventType::Added
        );
        assert_eq!(
            event_type(&store_event(StoreEventKind::Put, Some(3))),
            EventType::Modified
        );
        assert_eq!(
            event_type(&store_event(StoreEventKind::Delete, Some(3))),
            EventType::Deleted
        );
    }
}
