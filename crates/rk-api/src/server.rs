//! Route table. All resource routes share the generic handlers in
//! [`crate::resources`]; axum's router prefers static segments, so
//! `/api/v1/namespaces/{ns}/...` and `/api/v1/watch/...` win over the
//! `{plural}` captures they overlap with.

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::resources::{self, AppState};
use crate::watch;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/api/v1/watch", get(watch::watch_all))
        .route("/api/v1/watch/{plural}", get(watch::watch_kind))
        .route(
            "/api/v1/{plural}",
            get(resources::list_cluster).post(resources::create_cluster),
        )
        .route(
            "/api/v1/{plural}/{name}",
            get(resources::get_cluster)
                .put(resources::update_cluster)
                .delete(resources::delete_cluster),
        )
        .route(
            "/api/v1/namespaces/{ns}/{plural}",
            get(resources::list_namespaced).post(resources::create_namespaced),
        )
        .route(
            "/api/v1/namespaces/{ns}/{plural}/{name}",
            get(resources::get_namespaced)
                .put(resources::update_namespaced)
                .delete(resources::delete_namespaced),
        )
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Serialize)]
struct VersionInfo {
    component: &'static str,
    version: &'static str,
}

async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        component: "rk-api",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt as _;
    use rk_store::SqliteStore;
    use tower::ServiceExt as _;

    use super::*;

    async fn test_router() -> Router {
        let store = SqliteStore::in_memory().await.unwrap();
        router(AppState {
            store: Arc::new(store),
        })
    }

    fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn pod_manifest(name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name },
            "spec": { "containers": [{ "name": "web", "image": "nginx:1.27" }] }
        })
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let response = test_router()
            .await
            .oneshot(get_request("/healthz"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn version_reports_the_component() {
        let response = test_router()
            .await
            .oneshot(get_request("/version"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let info = body_json(response).await;
        assert_eq!(info["component"], "rk-api");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn create_get_list_delete_round_trip() {
        let app = test_router().await;

        // Create assigns uid, creationTimestamp and resourceVersion.
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/namespaces/default/pods",
                pod_manifest("web-0"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let created = body_json(response).await;
        assert_eq!(created["metadata"]["namespace"], "default");
        assert_eq!(created["metadata"]["resourceVersion"], 1);
        assert_eq!(created["metadata"]["uid"].as_str().unwrap().len(), 32);
        assert!(created["metadata"]["creationTimestamp"].is_string());

        // Get returns the stored object.
        let response = app
            .clone()
            .oneshot(get_request("/api/v1/namespaces/default/pods/web-0"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // List carries the resume revision.
        let response = app
            .clone()
            .oneshot(get_request("/api/v1/namespaces/default/pods"))
            .await
            .unwrap();
        let list = body_json(response).await;
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert_eq!(list["resourceVersion"], 1);

        // Delete returns the deleted object; a second get is 404.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/namespaces/default/pods/web-0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(get_request("/api/v1/namespaces/default/pods/web-0"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let app = test_router().await;
        let request = || json_request("POST", "/api/v1/pods", pod_manifest("web-0"));
        assert_eq!(
            app.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::CREATED
        );
        let response = app.oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error = body_json(response).await;
        assert!(error["error"].as_str().unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn update_honors_resource_versions() {
        let app = test_router().await;
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/namespaces/default/pods",
                pod_manifest("web-0"),
            ))
            .await
            .unwrap();

        // Stale resourceVersion conflicts.
        let mut stale = pod_manifest("web-0");
        stale["metadata"]["resourceVersion"] = 99.into();
        let response = app
            .clone()
            .oneshot(json_request(
                "PUT",
                "/api/v1/namespaces/default/pods/web-0",
                stale,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        // Correct resourceVersion updates and preserves server-owned fields.
        let mut fresh = pod_manifest("web-0");
        fresh["metadata"]["resourceVersion"] = 1.into();
        fresh["spec"]["containers"][0]["image"] = "nginx:1.28".into();
        let response = app
            .clone()
            .oneshot(json_request(
                "PUT",
                "/api/v1/namespaces/default/pods/web-0",
                fresh,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let updated = body_json(response).await;
        assert_eq!(updated["metadata"]["resourceVersion"], 2);
        assert_eq!(updated["spec"]["containers"][0]["image"], "nginx:1.28");
        assert!(
            updated["metadata"]["uid"].is_string(),
            "uid survives replace"
        );

        // Updating a missing object is 404, not create.
        let response = app
            .oneshot(json_request(
                "PUT",
                "/api/v1/namespaces/default/pods/ghost",
                pod_manifest("ghost"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn scope_and_kind_misuse_is_rejected() {
        let app = test_router().await;

        // Namespaced kind through the cluster item path.
        let response = app
            .clone()
            .oneshot(get_request("/api/v1/pods/web-0"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Cluster-scoped kind through a namespaced path.
        let response = app
            .clone()
            .oneshot(get_request("/api/v1/namespaces/default/nodes"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Unknown plural.
        let response = app
            .clone()
            .oneshot(get_request("/api/v1/gizmos"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Body kind not matching the path.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/v1/namespaces/default/services",
                pod_manifest("web-0"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cluster_scoped_kinds_use_the_short_paths() {
        let app = test_router().await;
        let node = serde_json::json!({
            "kind": "Node",
            "metadata": { "name": "worker-1", "labels": { "zone": "a" } }
        });
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/v1/nodes", node))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/nodes/worker-1"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let fetched = body_json(response).await;
        assert!(fetched["metadata"].get("namespace").is_none());
    }

    #[tokio::test]
    async fn watch_streams_backlog_and_live_events() {
        let store = Arc::new(SqliteStore::in_memory().await.unwrap());
        let app = router(AppState {
            store: store.clone(),
        });

        // One pod before the watch starts (backlog), one after (live).
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/namespaces/default/pods",
                pod_manifest("before"),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/watch/pods?since=0"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let mut body = response.into_body();

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let first = String::from_utf8(first.into_data().unwrap().to_vec()).unwrap();
        assert!(first.contains(r#""type":"ADDED""#), "{first}");
        assert!(first.contains(r#""name":"before""#), "{first}");

        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/namespaces/default/pods",
                pod_manifest("after"),
            ))
            .await
            .unwrap();

        let second = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let second = String::from_utf8(second.into_data().unwrap().to_vec()).unwrap();
        assert!(second.contains(r#""name":"after""#), "{second}");
    }

    #[tokio::test]
    async fn watch_filters_by_namespace() {
        let app = test_router().await;
        let response = app
            .clone()
            .oneshot(get_request("/api/v1/watch/pods?namespace=prod"))
            .await
            .unwrap();
        let mut body = response.into_body();

        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/namespaces/default/pods",
                pod_manifest("noise"),
            ))
            .await
            .unwrap();
        let mut prod_pod = pod_manifest("signal");
        prod_pod["metadata"]["namespace"] = "prod".into();
        app.clone()
            .oneshot(json_request("POST", "/api/v1/pods", prod_pod))
            .await
            .unwrap();

        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(
            frame.contains(r#""name":"signal""#),
            "default-ns noise leaked: {frame}"
        );
    }
}
