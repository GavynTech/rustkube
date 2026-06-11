//! Mapping between pods and runtime container names.
//!
//! A runtime container is named `p<uid12>-<container>`: the pod UID prefix
//! makes the name unique per pod *instance* (delete + recreate gets a new
//! UID, so the old container is recognizably an orphan), and the embedded
//! container name keeps `rkrt ps` output readable on the node. The runtime
//! caps names at 40 chars, which bounds the container name to 25.

pub const MAX_CONTAINER_NAME: usize = 25;
const UID_PREFIX_LEN: usize = 12;

pub fn uid12(pod_uid: &str) -> String {
    pod_uid.chars().take(UID_PREFIX_LEN).collect()
}

pub fn runtime_name(pod_uid: &str, container: &str) -> String {
    format!("p{}-{container}", uid12(pod_uid))
}

/// Recover `(uid12, container)` from a runtime container name. `None` for
/// containers this agent does not manage (e.g. hand-started `rkrt` ones).
pub fn parse_runtime_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix('p')?;
    let (uid, container) = rest.split_at_checked(UID_PREFIX_LEN)?;
    if uid.len() != UID_PREFIX_LEN || !uid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let container = container.strip_prefix('-')?;
    if container.is_empty() {
        return None;
    }
    Some((uid, container))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let uid = "4f9d2c81aa55b2e7c01d9f33ab12cd34";
        let name = runtime_name(uid, "nginx");
        assert_eq!(name, "p4f9d2c81aa55-nginx");
        assert_eq!(parse_runtime_name(&name), Some(("4f9d2c81aa55", "nginx")));
    }

    #[test]
    fn foreign_names_do_not_parse() {
        for foreign in [
            "web",             // hand-started container
            "p123-x",          // uid too short
            "pzzzzzzzzzzzz-x", // uid not hex
            "p4f9d2c81aa55",   // missing container
            "p4f9d2c81aa55-",  // empty container
            "q4f9d2c81aa55-x", // wrong prefix
        ] {
            assert_eq!(parse_runtime_name(foreign), None, "{foreign:?}");
        }
    }

    #[test]
    fn max_length_fits_the_runtime_limit() {
        let uid = "4f9d2c81aa55b2e7c01d9f33ab12cd34";
        let container = "c".repeat(MAX_CONTAINER_NAME);
        assert!(runtime_name(uid, &container).len() <= 40);
    }
}
