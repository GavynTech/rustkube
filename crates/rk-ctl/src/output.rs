//! Table rendering and the per-kind column definitions.

use chrono::{DateTime, Utc};
use rk_types::ResourceKind;
use serde_json::Value;

/// Render rows with each column padded to its widest cell. Plain spaces, no
/// box drawing — the same flavor as kubectl.
pub fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }
    let render_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}   ", width = widths[i]));
            }
        }
        line.trim_end().to_owned()
    };

    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    let mut out = render_row(&header_cells);
    for row in rows {
        out.push('\n');
        out.push_str(&render_row(row));
    }
    out
}

/// Compact k8s-style age: 45s, 12m, 7h, 3d.
pub fn humanize_age(created: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - created).num_seconds().max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

fn field<'v>(object: &'v Value, pointer: &str) -> Option<&'v str> {
    object.pointer(pointer).and_then(Value::as_str)
}

pub fn age_of(object: &Value, now: DateTime<Utc>) -> String {
    field(object, "/metadata/creationTimestamp")
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map_or_else(
            || "<unknown>".to_owned(),
            |ts| humanize_age(ts.with_timezone(&Utc), now),
        )
}

/// Column headers for a kind. `with_namespace` prepends NAMESPACE (used for
/// cross-namespace listings).
pub fn headers(kind: ResourceKind, with_namespace: bool) -> Vec<&'static str> {
    let mut headers = if with_namespace {
        vec!["NAMESPACE"]
    } else {
        Vec::new()
    };
    headers.push("NAME");
    match kind {
        ResourceKind::Pod => headers.extend(["READY", "STATUS", "RESTARTS", "IP", "NODE"]),
        ResourceKind::Node => headers.push("STATUS"),
        ResourceKind::Deployment => headers.extend(["READY", "UP-TO-DATE", "AVAILABLE"]),
        ResourceKind::ReplicaSet => headers.extend(["DESIRED", "CURRENT", "READY"]),
        ResourceKind::Service => headers.extend(["TYPE", "CLUSTER-IP", "PORT(S)"]),
        _ => {}
    }
    headers.push("AGE");
    headers
}

/// kubectl-style `PORT(S)` column: `80/TCP`, or `80:30080/TCP` for NodePort.
fn service_ports(object: &Value) -> String {
    let Some(ports) = object.pointer("/spec/ports").and_then(Value::as_array) else {
        return "<none>".to_owned();
    };
    ports
        .iter()
        .map(|p| {
            let port = p.get("port").and_then(Value::as_u64).unwrap_or(0);
            let proto = p.get("protocol").and_then(Value::as_str).unwrap_or("TCP");
            match p.get("nodePort").and_then(Value::as_u64) {
                Some(np) => format!("{port}:{np}/{proto}"),
                None => format!("{port}/{proto}"),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `<ready>/<desired>` the way kubectl renders workload readiness.
fn ready_fraction(object: &Value, desired_ptr: &str) -> String {
    let desired = object
        .pointer(desired_ptr)
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let ready = object
        .pointer("/status/readyReplicas")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    format!("{ready}/{desired}")
}

fn u64_field(object: &Value, pointer: &str) -> String {
    object
        .pointer(pointer)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .to_string()
}

/// A pod's display status: the waiting reason when a container is stuck
/// (CrashLoopBackOff, ErrImagePull) reads better than the bare phase.
fn pod_display_status(object: &Value) -> String {
    if let Some(reason) = object
        .pointer("/status/containerStatuses/0/state/waiting/reason")
        .and_then(Value::as_str)
        .filter(|r| *r != "ContainerCreating")
    {
        return reason.to_owned();
    }
    field(object, "/status/phase")
        .unwrap_or("Pending")
        .to_owned()
}

fn pod_restarts(object: &Value) -> String {
    object
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .map(|statuses| {
            statuses
                .iter()
                .filter_map(|s| s.get("restartCount").and_then(Value::as_u64))
                .sum::<u64>()
        })
        .unwrap_or(0)
        .to_string()
}

pub fn row(
    kind: ResourceKind,
    object: &Value,
    with_namespace: bool,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut row = Vec::new();
    if with_namespace {
        row.push(
            field(object, "/metadata/namespace")
                .unwrap_or("<none>")
                .to_owned(),
        );
    }
    row.push(
        field(object, "/metadata/name")
            .unwrap_or("<unnamed>")
            .to_owned(),
    );
    match kind {
        ResourceKind::Pod => {
            // READY = ready containers / total (readiness probes gate this).
            let statuses = object
                .pointer("/status/containerStatuses")
                .and_then(Value::as_array);
            let total = object
                .pointer("/spec/containers")
                .and_then(Value::as_array)
                .map_or(1, Vec::len);
            let ready = statuses.map_or(0, |s| {
                s.iter()
                    .filter(|c| c.get("ready").and_then(Value::as_bool) == Some(true))
                    .count()
            });
            row.push(format!("{ready}/{total}"));
            // A statusless pod reads Pending, matching what Kubernetes shows
            // before a kubelet picks the pod up.
            row.push(pod_display_status(object));
            row.push(pod_restarts(object));
            row.push(
                field(object, "/status/podIP")
                    .unwrap_or("<none>")
                    .to_owned(),
            );
            row.push(
                field(object, "/spec/nodeName")
                    .unwrap_or("<none>")
                    .to_owned(),
            );
        }
        ResourceKind::Node => {
            row.push(
                field(object, "/status/condition")
                    .unwrap_or("Unknown")
                    .to_owned(),
            );
        }
        ResourceKind::Deployment => {
            row.push(ready_fraction(object, "/spec/replicas"));
            row.push(u64_field(object, "/status/updatedReplicas"));
            row.push(u64_field(object, "/status/readyReplicas"));
        }
        ResourceKind::ReplicaSet => {
            row.push(u64_field(object, "/spec/replicas"));
            row.push(u64_field(object, "/status/replicas"));
            row.push(u64_field(object, "/status/readyReplicas"));
        }
        ResourceKind::Service => {
            row.push(
                field(object, "/spec/type")
                    .unwrap_or("ClusterIP")
                    .to_owned(),
            );
            row.push(
                field(object, "/spec/clusterIP")
                    .unwrap_or("<none>")
                    .to_owned(),
            );
            row.push(service_ports(object));
        }
        _ => {}
    }
    row.push(age_of(object, now));
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_pads_columns_to_the_widest_cell() {
        let table = render_table(
            &["NAME", "AGE"],
            &[
                vec!["web-0".to_owned(), "5s".to_owned()],
                vec!["a-much-longer-name".to_owned(), "2d".to_owned()],
            ],
        );
        assert_eq!(
            table,
            "NAME                 AGE\n\
             web-0                5s\n\
             a-much-longer-name   2d"
        );
    }

    #[test]
    fn ages_humanize_by_magnitude() {
        let now = DateTime::parse_from_rfc3339("2026-06-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let cases = [
            ("2026-06-11T11:59:15Z", "45s"),
            ("2026-06-11T11:48:00Z", "12m"),
            ("2026-06-11T05:00:00Z", "7h"),
            ("2026-06-08T12:00:00Z", "3d"),
            ("2026-06-11T12:00:30Z", "0s"), // clock skew clamps to zero
        ];
        for (created, expected) in cases {
            let created = DateTime::parse_from_rfc3339(created)
                .unwrap()
                .with_timezone(&Utc);
            assert_eq!(humanize_age(created, now), expected, "{created}");
        }
    }

    #[test]
    fn pod_rows_show_status_restarts_ip_and_node() {
        let now = Utc::now();
        let pod = serde_json::json!({
            "metadata": { "name": "web-0", "namespace": "prod",
                          "creationTimestamp": (now - chrono::Duration::seconds(30)).to_rfc3339() },
            "spec": { "nodeName": "worker-1" },
            "status": {
                "phase": "Running",
                "podIP": "10.42.0.6",
                "containerStatuses": [{ "name": "nginx", "restartCount": 2, "ready": true,
                                        "state": { "running": {} } }]
            },
            "spec": { "nodeName": "worker-1", "containers": [{ "name": "nginx", "image": "nginx" }] }
        });
        assert_eq!(
            row(ResourceKind::Pod, &pod, true, now),
            vec![
                "prod",
                "web-0",
                "1/1",
                "Running",
                "2",
                "10.42.0.6",
                "worker-1",
                "30s"
            ]
        );

        let bare = serde_json::json!({ "metadata": { "name": "new-pod" } });
        assert_eq!(
            row(ResourceKind::Pod, &bare, false, now),
            vec![
                "new-pod",
                "0/1",
                "Pending",
                "0",
                "<none>",
                "<none>",
                "<unknown>"
            ]
        );
    }

    #[test]
    fn not_ready_pods_show_a_zero_ready_fraction() {
        let pod = serde_json::json!({
            "metadata": { "name": "web-0" },
            "spec": { "containers": [{ "name": "nginx", "image": "nginx" }] },
            "status": {
                "phase": "Running",
                "containerStatuses": [{ "name": "nginx", "ready": false, "state": { "running": {} } }]
            }
        });
        // Running but its readiness probe hasn't passed: 0/1 ready.
        assert_eq!(row(ResourceKind::Pod, &pod, false, Utc::now())[1], "0/1");
    }

    #[test]
    fn crash_loops_show_their_waiting_reason() {
        let pod = serde_json::json!({
            "metadata": { "name": "web-0" },
            "status": {
                "phase": "Running",
                "containerStatuses": [{ "state": { "waiting": { "reason": "CrashLoopBackOff" } },
                                        "restartCount": 4 }]
            }
        });
        let row = row(ResourceKind::Pod, &pod, false, Utc::now());
        assert_eq!(row[2], "CrashLoopBackOff");
        assert_eq!(row[3], "4");
    }

    #[test]
    fn node_rows_show_the_condition() {
        let node = serde_json::json!({
            "metadata": { "name": "worker-1" },
            "status": { "condition": "Ready" }
        });
        assert_eq!(
            row(ResourceKind::Node, &node, false, Utc::now()),
            vec!["worker-1", "Ready", "<unknown>"]
        );
    }

    #[test]
    fn headers_match_row_shapes() {
        assert_eq!(headers(ResourceKind::Pod, true).len(), 8);
        assert_eq!(
            headers(ResourceKind::Node, false),
            vec!["NAME", "STATUS", "AGE"]
        );
        assert_eq!(
            headers(ResourceKind::Deployment, false),
            vec!["NAME", "READY", "UP-TO-DATE", "AVAILABLE", "AGE"]
        );
        assert_eq!(headers(ResourceKind::ReplicaSet, false).len(), 5);
        assert_eq!(
            headers(ResourceKind::Service, false),
            vec!["NAME", "TYPE", "CLUSTER-IP", "PORT(S)", "AGE"]
        );
        assert_eq!(headers(ResourceKind::ConfigMap, false), vec!["NAME", "AGE"]);
    }

    #[test]
    fn service_rows_render_type_ip_and_ports() {
        let now = Utc::now();
        let svc = serde_json::json!({
            "metadata": { "name": "web" },
            "spec": {
                "type": "NodePort",
                "clusterIP": "10.96.0.10",
                "ports": [{ "port": 80, "nodePort": 30080, "protocol": "TCP" }]
            }
        });
        assert_eq!(
            row(ResourceKind::Service, &svc, false, now),
            vec!["web", "NodePort", "10.96.0.10", "80:30080/TCP", "<unknown>"]
        );
    }

    #[test]
    fn workload_rows_show_replica_fractions() {
        let now = Utc::now();
        let deploy = serde_json::json!({
            "metadata": { "name": "web" },
            "spec": { "replicas": 3 },
            "status": { "replicas": 3, "updatedReplicas": 2, "readyReplicas": 2 }
        });
        assert_eq!(
            row(ResourceKind::Deployment, &deploy, false, now),
            vec!["web", "2/3", "2", "2", "<unknown>"]
        );

        let rs = serde_json::json!({
            "metadata": { "name": "web-abc12" },
            "spec": { "replicas": 3 },
            "status": { "replicas": 3, "readyReplicas": 1 }
        });
        assert_eq!(
            row(ResourceKind::ReplicaSet, &rs, false, now),
            vec!["web-abc12", "3", "3", "1", "<unknown>"]
        );
    }
}
