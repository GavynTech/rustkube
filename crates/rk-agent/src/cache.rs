//! The agent's view of desired state: a list+watch cache of pods bound to
//! this node. List gives a consistent snapshot plus the revision to watch
//! from; the watch keeps it current without polling. Any stream failure or
//! lag falls back to a fresh list — the standard reflector loop.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt as _;
use rk_types::client::ApiClient;
use rk_types::{EventType, Pod};
use tokio::sync::{Mutex, Notify};

pub type SharedPods = Arc<Mutex<HashMap<String, Pod>>>;

fn assigned_to(pod: &Pod, node: &str) -> bool {
    pod.spec.node_name.as_deref() == Some(node)
}

fn uid_of(pod: &Pod) -> Option<String> {
    pod.metadata.uid.clone()
}

/// Maintain `pods` (keyed by UID) forever; ping `notify` after every change
/// so the reconcile loop wakes immediately instead of waiting for resync.
pub async fn run(client: Arc<ApiClient>, node_name: String, pods: SharedPods, notify: Arc<Notify>) {
    loop {
        let list = match client.list::<Pod>(None).await {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!(error = %err, "pod list failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        let since = list.resource_version.unwrap_or(0);
        {
            let mut map = pods.lock().await;
            map.clear();
            for pod in list.items {
                if assigned_to(&pod, &node_name)
                    && let Some(uid) = uid_of(&pod)
                {
                    map.insert(uid, pod);
                }
            }
            tracing::debug!(pods = map.len(), "pod cache rebuilt from list");
        }
        notify.notify_one();

        let mut stream = match client.watch::<Pod>(None, since).await {
            Ok(stream) => stream,
            Err(err) => {
                tracing::warn!(error = %err, "pod watch failed; re-listing");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        while let Some(item) = stream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(err) => {
                    tracing::warn!(error = %err, "watch stream error; re-listing");
                    break;
                }
            };
            let Some(uid) = uid_of(&event.object) else {
                continue;
            };
            let mut map = pods.lock().await;
            let changed = match event.event_type {
                EventType::Added | EventType::Modified => {
                    if assigned_to(&event.object, &node_name) {
                        map.insert(uid, event.object);
                        true
                    } else {
                        // Not ours — but if it *was* ours, it has been
                        // reassigned away and must be torn down here.
                        map.remove(&uid).is_some()
                    }
                }
                EventType::Deleted => map.remove(&uid).is_some(),
            };
            drop(map);
            if changed {
                notify.notify_one();
            }
        }
        // Stream ended (server cut a lagging watch, or rk-api restarted):
        // loop back to a fresh list.
        tracing::info!("pod watch ended; re-listing");
    }
}

#[cfg(test)]
mod tests {
    use rk_types::pod::PodSpec;

    use super::*;

    fn pod_on(node: Option<&str>) -> Pod {
        Pod {
            api_version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            metadata: rk_types::ObjectMeta::named("web"),
            spec: PodSpec {
                node_name: node.map(str::to_owned),
                ..PodSpec::default()
            },
            status: Default::default(),
        }
    }

    #[test]
    fn assignment_filter_matches_exactly() {
        assert!(assigned_to(&pod_on(Some("worker-1")), "worker-1"));
        assert!(!assigned_to(&pod_on(Some("worker-2")), "worker-1"));
        assert!(
            !assigned_to(&pod_on(None), "worker-1"),
            "unscheduled pods are not ours"
        );
    }
}
