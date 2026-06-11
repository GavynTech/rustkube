//! Generic REST CRUD over every resource kind.
//!
//! Paths follow the Kubernetes shape, minus API groups (everything is
//! `/api/v1`):
//!
//! ```text
//! /api/v1/{plural}                                list (all namespaces) | POST create
//! /api/v1/{plural}/{name}                         get/put/delete (cluster-scoped kinds)
//! /api/v1/namespaces/{ns}/{plural}                list | POST create
//! /api/v1/namespaces/{ns}/{plural}/{name}         get/put/delete
//! ```
//!
//! One handler set serves all kinds: the `{plural}` segment resolves through
//! [`ResourceKind::from_str`], the body validates through [`AnyObject`] (so a
//! Pod manifest with a malformed spec is rejected with serde's error text),
//! and scope rules (`Pod` is namespaced, `Node` is not) are enforced before
//! any store call.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rk_store::{Store, StoredObject};
use rk_types::{AnyObject, List, ResourceKind};
use serde_json::Value;

use crate::error::ApiError;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
}

pub fn parse_kind(plural: &str) -> Result<ResourceKind, ApiError> {
    plural
        .parse()
        .map_err(|_| ApiError::NotFound(format!("unknown resource type {plural:?}")))
}

/// Names become path segments, store keys and (for pods) hostnames — keep
/// them DNS-friendly: lowercase alphanumerics, `-` and `.`, 253 chars max.
fn validate_name(what: &str, name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(ApiError::BadRequest(format!(
            "invalid {what} {name:?}: use lowercase letters, digits, '-' and '.', \
             starting and ending alphanumeric (max 253 chars)"
        )))
    }
}

/// 32 hex chars from the kernel CSPRNG; assigned once at creation.
fn generate_uid() -> Result<String, ApiError> {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|err| ApiError::Internal(format!("urandom: {err}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Validate a manifest against the path it was sent to and normalize it into
/// a [`StoredObject`]. `path_ns` is `None` on cluster-scoped routes,
/// `path_name` is `Some` on item routes (PUT) where it must match the body.
fn prepare(
    kind: ResourceKind,
    path_ns: Option<&str>,
    path_name: Option<&str>,
    body: Value,
) -> Result<StoredObject, ApiError> {
    // Shape-check through the typed model; the raw Value is what gets stored
    // (serializing the typed object back would silently drop unknown fields,
    // and erroring loudly beats erasing what the user wrote).
    let object: AnyObject = serde_json::from_value(body.clone())
        .map_err(|err| ApiError::BadRequest(format!("invalid manifest: {err}")))?;
    if object.kind() != kind {
        return Err(ApiError::BadRequest(format!(
            "manifest kind {} does not match request path ({})",
            object.kind(),
            kind
        )));
    }

    let meta = object.metadata();
    validate_name("name", &meta.name)?;
    if let Some(expected) = path_name
        && meta.name != expected
    {
        return Err(ApiError::BadRequest(format!(
            "manifest name {:?} does not match request path name {expected:?}",
            meta.name
        )));
    }

    let namespace = if kind.namespaced() {
        let ns = match (path_ns, meta.namespace.as_deref()) {
            (Some(path), Some(body)) if path != body => {
                return Err(ApiError::BadRequest(format!(
                    "manifest namespace {body:?} does not match request path namespace {path:?}"
                )));
            }
            (Some(path), _) => path,
            (None, Some(body)) => body,
            (None, None) => "default",
        };
        validate_name("namespace", ns)?;
        Some(ns.to_owned())
    } else {
        if meta.namespace.is_some() {
            return Err(ApiError::BadRequest(format!(
                "{kind} is cluster-scoped; metadata.namespace must not be set"
            )));
        }
        None
    };

    // Re-root the normalized namespace into the stored JSON so reads always
    // show where the object actually lives.
    let mut data = body;
    if let Some(metadata) = data.get_mut("metadata").and_then(Value::as_object_mut) {
        match &namespace {
            Some(ns) => {
                metadata.insert("namespace".to_owned(), Value::String(ns.clone()));
            }
            None => {
                metadata.remove("namespace");
            }
        }
    }

    Ok(StoredObject {
        kind,
        namespace,
        name: meta.name.clone(),
        revision: 0, // assigned by the store
        data,
    })
}

fn meta_mut(data: &mut Value) -> Result<&mut serde_json::Map<String, Value>, ApiError> {
    data.get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::Internal("object lost its metadata".to_owned()))
}

async fn do_create(
    state: &AppState,
    kind: ResourceKind,
    path_ns: Option<&str>,
    body: Value,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let mut object = prepare(kind, path_ns, None, body)?;

    let metadata = meta_mut(&mut object.data)?;
    metadata.insert("uid".to_owned(), Value::String(generate_uid()?));
    metadata.insert(
        "creationTimestamp".to_owned(),
        Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    );
    // The store stamps the authoritative resourceVersion.
    metadata.remove("resourceVersion");

    let key = object.key();
    let stored = state.store.put(object, Some(0)).await.map_err(|err| {
        if matches!(err, rk_store::StoreError::Conflict { .. }) {
            ApiError::Conflict(format!("{kind} {key:?} already exists"))
        } else {
            err.into()
        }
    })?;
    Ok((StatusCode::CREATED, Json(stored.data)))
}

async fn do_update(
    state: &AppState,
    kind: ResourceKind,
    path_ns: Option<&str>,
    name: &str,
    body: Value,
) -> Result<Json<Value>, ApiError> {
    let mut object = prepare(kind, path_ns, Some(name), body)?;

    let current = state
        .store
        .get(kind, object.namespace.as_deref(), name)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "{kind} {:?} not found (use POST to create)",
                object.key()
            ))
        })?;

    // The client's resourceVersion (when given) is the optimistic-concurrency
    // token; uid and creationTimestamp are server-owned and survive replaces
    // that omit them.
    let expected = object
        .data
        .get("metadata")
        .and_then(|m| m.get("resourceVersion"))
        .and_then(Value::as_u64);
    let metadata = meta_mut(&mut object.data)?;
    for inherited in ["uid", "creationTimestamp"] {
        if !metadata.contains_key(inherited)
            && let Some(value) = current.data.get("metadata").and_then(|m| m.get(inherited))
        {
            metadata.insert(inherited.to_owned(), value.clone());
        }
    }

    let stored = state.store.put(object, expected).await?;
    Ok(Json(stored.data))
}

async fn do_list(
    state: &AppState,
    kind: ResourceKind,
    namespace: Option<&str>,
) -> Result<Json<List<Value>>, ApiError> {
    let (objects, revision) = state.store.list(kind, namespace).await?;
    Ok(Json(List {
        items: objects.into_iter().map(|o| o.data).collect(),
        resource_version: Some(revision),
    }))
}

async fn do_get(
    state: &AppState,
    kind: ResourceKind,
    namespace: Option<&str>,
    name: &str,
) -> Result<Json<Value>, ApiError> {
    let object = state
        .store
        .get(kind, namespace, name)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "{kind} {:?} not found",
                display_key(namespace, name)
            ))
        })?;
    Ok(Json(object.data))
}

async fn do_delete(
    state: &AppState,
    kind: ResourceKind,
    namespace: Option<&str>,
    name: &str,
) -> Result<Json<Value>, ApiError> {
    let deleted = state.store.delete(kind, namespace, name, None).await?;
    Ok(Json(deleted.data))
}

pub fn display_key(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{ns}/{name}"),
        None => name.to_owned(),
    }
}

/// Item routes under `/api/v1/{plural}/{name}` are only for cluster-scoped
/// kinds; pods and friends must go through their namespace.
fn require_cluster_scope(kind: ResourceKind) -> Result<(), ApiError> {
    if kind.namespaced() {
        Err(ApiError::BadRequest(format!(
            "{kind} is namespaced; use /api/v1/namespaces/{{namespace}}/{}/{{name}}",
            kind.plural()
        )))
    } else {
        Ok(())
    }
}

fn require_namespaced(kind: ResourceKind) -> Result<(), ApiError> {
    if kind.namespaced() {
        Ok(())
    } else {
        Err(ApiError::BadRequest(format!(
            "{kind} is cluster-scoped; use /api/v1/{}",
            kind.plural()
        )))
    }
}

// ---- cluster-path handlers ----

pub async fn list_cluster(
    State(state): State<AppState>,
    Path(plural): Path<String>,
) -> Result<Json<List<Value>>, ApiError> {
    let kind = parse_kind(&plural)?;
    // For namespaced kinds the cluster path lists across all namespaces.
    do_list(&state, kind, None).await
}

pub async fn create_cluster(
    State(state): State<AppState>,
    Path(plural): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let kind = parse_kind(&plural)?;
    // Namespaced kinds may be created here too; the namespace comes from the
    // manifest (or defaults), matching `kubectl create -f` ergonomics.
    do_create(&state, kind, None, body).await
}

pub async fn get_cluster(
    State(state): State<AppState>,
    Path((plural, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_cluster_scope(kind)?;
    do_get(&state, kind, None, &name).await
}

pub async fn update_cluster(
    State(state): State<AppState>,
    Path((plural, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_cluster_scope(kind)?;
    do_update(&state, kind, None, &name, body).await
}

pub async fn delete_cluster(
    State(state): State<AppState>,
    Path((plural, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_cluster_scope(kind)?;
    do_delete(&state, kind, None, &name).await
}

// ---- namespaced-path handlers ----

pub async fn list_namespaced(
    State(state): State<AppState>,
    Path((ns, plural)): Path<(String, String)>,
) -> Result<Json<List<Value>>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_namespaced(kind)?;
    do_list(&state, kind, Some(&ns)).await
}

pub async fn create_namespaced(
    State(state): State<AppState>,
    Path((ns, plural)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let kind = parse_kind(&plural)?;
    require_namespaced(kind)?;
    do_create(&state, kind, Some(&ns), body).await
}

pub async fn get_namespaced(
    State(state): State<AppState>,
    Path((ns, plural, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_namespaced(kind)?;
    do_get(&state, kind, Some(&ns), &name).await
}

pub async fn update_namespaced(
    State(state): State<AppState>,
    Path((ns, plural, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_namespaced(kind)?;
    do_update(&state, kind, Some(&ns), &name, body).await
}

pub async fn delete_namespaced(
    State(state): State<AppState>,
    Path((ns, plural, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let kind = parse_kind(&plural)?;
    require_namespaced(kind)?;
    do_delete(&state, kind, Some(&ns), &name).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod_manifest(name: &str) -> Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name },
            "spec": { "containers": [{ "name": "web", "image": "nginx:1.27" }] }
        })
    }

    #[test]
    fn prepare_defaults_the_namespace_for_namespaced_kinds() {
        let object = prepare(ResourceKind::Pod, None, None, pod_manifest("web-0")).unwrap();
        assert_eq!(object.namespace.as_deref(), Some("default"));
        assert_eq!(object.data["metadata"]["namespace"], "default");

        let object = prepare(ResourceKind::Pod, Some("prod"), None, pod_manifest("web-0")).unwrap();
        assert_eq!(object.namespace.as_deref(), Some("prod"));
    }

    #[test]
    fn prepare_rejects_path_body_mismatches() {
        let mut manifest = pod_manifest("web-0");
        manifest["metadata"]["namespace"] = "staging".into();
        let err = prepare(ResourceKind::Pod, Some("prod"), None, manifest).unwrap_err();
        assert!(err.to_string().contains("namespace"));

        let err = prepare(
            ResourceKind::Pod,
            None,
            Some("other"),
            pod_manifest("web-0"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("name"));

        let err = prepare(ResourceKind::Service, None, None, pod_manifest("web-0")).unwrap_err();
        assert!(err.to_string().contains("does not match request path"));
    }

    #[test]
    fn prepare_rejects_namespaces_on_cluster_scoped_kinds() {
        let manifest = serde_json::json!({
            "kind": "Node",
            "metadata": { "name": "worker-1", "namespace": "default" }
        });
        let err = prepare(ResourceKind::Node, None, None, manifest).unwrap_err();
        assert!(err.to_string().contains("cluster-scoped"));
    }

    #[test]
    fn prepare_surfaces_serde_errors_for_bad_specs() {
        let manifest = serde_json::json!({
            "kind": "Pod",
            "metadata": { "name": "web-0" },
            "spec": { "containers": [{ "name": "web" }] } // image is required
        });
        let err = prepare(ResourceKind::Pod, None, None, manifest).unwrap_err();
        assert!(err.to_string().contains("image"), "{err}");
    }

    #[test]
    fn names_and_uids_are_validated_and_generated() {
        for bad in ["", "Web", "-x", "x-", &"a".repeat(254)] {
            assert!(validate_name("name", bad).is_err(), "{bad:?}");
        }
        validate_name("name", "web-0.staging").unwrap();

        let uid = generate_uid().unwrap();
        assert_eq!(uid.len(), 32);
        assert_ne!(generate_uid().unwrap(), uid);
    }

    #[test]
    fn unknown_plurals_are_not_found() {
        assert!(parse_kind("gizmos").is_err());
        assert_eq!(parse_kind("pods").unwrap(), ResourceKind::Pod);
    }
}
