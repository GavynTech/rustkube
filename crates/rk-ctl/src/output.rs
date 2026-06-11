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
    if kind == ResourceKind::Pod {
        headers.extend(["STATUS", "NODE"]);
    }
    headers.push("AGE");
    headers
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
    if kind == ResourceKind::Pod {
        // No agent yet (Phase 3) — unscheduled, statusless pods read Pending,
        // matching what Kubernetes shows before kubelet picks a pod up.
        row.push(
            field(object, "/status/phase")
                .unwrap_or("Pending")
                .to_owned(),
        );
        row.push(
            field(object, "/spec/nodeName")
                .unwrap_or("<none>")
                .to_owned(),
        );
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
    fn pod_rows_show_status_and_node() {
        let now = Utc::now();
        let pod = serde_json::json!({
            "metadata": { "name": "web-0", "namespace": "prod",
                          "creationTimestamp": (now - chrono::Duration::seconds(30)).to_rfc3339() },
            "spec": { "nodeName": "worker-1" },
            "status": { "phase": "Running" }
        });
        assert_eq!(
            row(ResourceKind::Pod, &pod, true, now),
            vec!["prod", "web-0", "Running", "worker-1", "30s"]
        );

        let bare = serde_json::json!({ "metadata": { "name": "new-pod" } });
        assert_eq!(
            row(ResourceKind::Pod, &bare, false, now),
            vec!["new-pod", "Pending", "<none>", "<unknown>"]
        );
    }

    #[test]
    fn headers_match_row_shapes() {
        assert_eq!(headers(ResourceKind::Pod, true).len(), 5);
        assert_eq!(headers(ResourceKind::ConfigMap, false), vec!["NAME", "AGE"]);
    }
}
