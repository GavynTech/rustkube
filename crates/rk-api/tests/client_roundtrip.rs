//! The shared client (`rk_types::client`) against a real rk-api over real
//! HTTP — the exact stack the agent, scheduler and controllers ride.

// Integration tests live outside #[cfg(test)] modules, so the clippy.toml
// test exemptions don't reach the helpers here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use futures_util::StreamExt as _;
use rk_api::resources::AppState;
use rk_store::SqliteStore;
use rk_types::client::ApiClient;
use rk_types::pod::PodSpec;
use rk_types::{EventType, Node, ObjectMeta, Pod};

async fn serve() -> ApiClient {
    let store = SqliteStore::in_memory().await.unwrap();
    let state = AppState {
        store: Arc::new(store),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, rk_api::server::router(state))
            .await
            .unwrap();
    });
    ApiClient::new(format!("http://{addr}")).unwrap()
}

fn pod(name: &str, node: Option<&str>) -> Pod {
    Pod {
        api_version: "v1".to_owned(),
        kind: "Pod".to_owned(),
        metadata: ObjectMeta::named(name),
        spec: PodSpec {
            node_name: node.map(str::to_owned),
            containers: vec![rk_types::Container {
                name: "web".to_owned(),
                image: "nginx:1.27".to_owned(),
                command: None,
                args: None,
                env: Vec::new(),
                ports: Vec::new(),
                resources: Default::default(),
                volume_mounts: Vec::new(),
                liveness_probe: None,
                readiness_probe: None,
            }],
            ..PodSpec::default()
        },
        status: Default::default(),
    }
}

#[tokio::test]
async fn typed_crud_round_trips() {
    let client = serve().await;

    // Create assigns server-owned fields.
    let created = client.create(&pod("web-0", None)).await.unwrap();
    assert_eq!(created.metadata.resource_version, Some(1));
    assert!(created.metadata.uid.is_some());
    assert_eq!(created.metadata.namespace.as_deref(), Some("default"));

    // Typed get and list see it.
    let fetched: Pod = client.get(Some("default"), "web-0").await.unwrap().unwrap();
    assert_eq!(fetched.spec.containers[0].image, "nginx:1.27");
    let list = client.list::<Pod>(None).await.unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.resource_version, Some(1));

    // Replace at the observed version (a status update, agent-style).
    let mut updated = fetched.clone();
    updated.status.phase = rk_types::PodPhase::Running;
    let replaced = client.replace(&updated).await.unwrap();
    assert_eq!(replaced.metadata.resource_version, Some(2));
    assert_eq!(replaced.metadata.uid, fetched.metadata.uid, "uid survives");

    // Stale replace conflicts as a 409 the caller can match on.
    let err = client.replace(&fetched).await.unwrap_err();
    assert!(matches!(
        err,
        rk_types::client::ClientError::Api { status: 409, .. }
    ));

    // Delete, then absence.
    client
        .delete::<Pod>(Some("default"), "web-0")
        .await
        .unwrap();
    assert!(
        client
            .get::<Pod>(Some("default"), "web-0")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cluster_scoped_resources_round_trip() {
    let client = serve().await;
    let node = Node {
        api_version: "v1".to_owned(),
        kind: "Node".to_owned(),
        metadata: ObjectMeta::named("worker-1"),
        spec: Default::default(),
        status: Default::default(),
    };
    client.create(&node).await.unwrap();
    let fetched: Node = client.get(None, "worker-1").await.unwrap().unwrap();
    assert_eq!(fetched.metadata.name, "worker-1");
    assert!(fetched.metadata.namespace.is_none());
}

#[tokio::test]
async fn typed_watch_streams_in_order() {
    let client = serve().await;

    client.create(&pod("before", Some("n1"))).await.unwrap();
    let list = client.list::<Pod>(None).await.unwrap();
    let since = list.resource_version.unwrap();

    let mut stream = client.watch::<Pod>(None, since).await.unwrap();
    client.create(&pod("after", Some("n1"))).await.unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("watch delivered nothing in 2s")
        .unwrap()
        .unwrap();
    assert_eq!(event.event_type, EventType::Added);
    assert_eq!(event.object.metadata.name, "after");
    assert_eq!(event.object.spec.node_name.as_deref(), Some("n1"));

    // A modification arrives as MODIFIED with the new content.
    let mut updated = event.object.clone();
    updated.status.phase = rk_types::PodPhase::Running;
    client.replace(&updated).await.unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("watch delivered nothing in 2s")
        .unwrap()
        .unwrap();
    assert_eq!(event.event_type, EventType::Modified);
    assert_eq!(event.object.status.phase, rk_types::PodPhase::Running);
}
