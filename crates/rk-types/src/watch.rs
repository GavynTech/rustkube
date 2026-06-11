//! Wire types for list and watch responses, shared by the API server and all
//! watching clients (agents, controllers, scheduler, CLI).

use serde::{Deserialize, Serialize};

/// Event types use Kubernetes' uppercase spelling on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EventType {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchEvent<T> {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub object: T,
}

/// A list response: items plus the store revision the list was taken at, so a
/// client can start a watch from exactly that point and miss nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_event_uses_kubernetes_wire_format() {
        let event = WatchEvent {
            event_type: EventType::Added,
            object: 42u32,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"ADDED","object":42}"#);
        let back: WatchEvent<u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_type, EventType::Added);
    }
}
