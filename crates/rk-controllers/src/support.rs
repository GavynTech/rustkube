//! Shared helpers across the controllers: ownership, pod-template hashing,
//! pod liveness predicates, and id generation. Pure and unit-tested; the
//! impure controller loops build on these.

use rk_types::meta::{ObjectMeta, OwnerReference, ResourceKind};
use rk_types::pod::{Pod, PodPhase};
use rk_types::workloads::PodTemplateSpec;
use sha2::{Digest as _, Sha256};

/// Label key carrying a pod template's hash, mirroring Kubernetes'
/// `pod-template-hash`. It distinguishes the pods (and ReplicaSets) of one
/// Deployment revision from another so their selectors never overlap.
pub const POD_TEMPLATE_HASH_LABEL: &str = "rustkube.io/pod-template-hash";

/// A short, stable hash of a pod template — the identity of a Deployment
/// revision. Computed over canonical JSON so field order can't change it.
pub fn pod_template_hash(template: &PodTemplateSpec) -> String {
    // The hash must ignore the template-hash label itself (it's injected
    // after hashing), so hash a copy with that label removed.
    let mut clean = template.clone();
    clean.metadata.labels.remove(POD_TEMPLATE_HASH_LABEL);
    let canonical = serde_json::to_vec(&clean).unwrap_or_default();
    let digest = Sha256::digest(&canonical);
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// The controller (kind, uid) that owns this object, if any.
pub fn controller_of(meta: &ObjectMeta) -> Option<(ResourceKind, &str)> {
    meta.owner_references
        .iter()
        .find(|owner| owner.controller)
        .and_then(|owner| owner.uid.as_deref().map(|uid| (owner.kind, uid)))
}

/// True if `meta` is controlled by an object of `kind` with `uid`.
pub fn owned_by(meta: &ObjectMeta, kind: ResourceKind, uid: &str) -> bool {
    controller_of(meta) == Some((kind, uid))
}

/// Build an owner reference marking `owner` as the controlling parent.
pub fn owner_ref(kind: ResourceKind, owner: &ObjectMeta) -> OwnerReference {
    OwnerReference {
        kind,
        name: owner.name.clone(),
        uid: owner.uid.clone(),
        controller: true,
    }
}

/// A pod is terminal once it has run to completion or failed for good; such
/// pods no longer count toward a ReplicaSet's live replicas.
pub fn is_terminal(pod: &Pod) -> bool {
    matches!(pod.status.phase, PodPhase::Succeeded | PodPhase::Failed)
}

/// A pod counts as ready when it is Running with its container reporting
/// ready (probes arrive in Phase 7; until then "running" is "ready").
pub fn is_ready(pod: &Pod) -> bool {
    pod.status.phase == PodPhase::Running
        && pod
            .status
            .container_statuses
            .iter()
            .all(|status| status.ready)
        && !pod.status.container_statuses.is_empty()
}

/// Five lowercase alphanumerics, the suffix style Kubernetes uses for
/// controller-generated names (`web-7d8f9`).
pub fn rand_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 5];
    // /dev/urandom is always present on Linux; fall back to a time-seed only
    // so this never panics in a non-Linux unit test.
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read as _;
        let _ = file.read_exact(&mut bytes);
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bytes = (nanos as u64).to_le_bytes()[..5]
            .try_into()
            .unwrap_or([0; 5]);
    }
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rk_types::pod::{ContainerStatus, PodSpec, PodStatus};
    use rk_types::workloads::TemplateMeta;

    use super::*;

    fn template(image: &str) -> PodTemplateSpec {
        PodTemplateSpec {
            metadata: TemplateMeta {
                labels: BTreeMap::from([("app".to_owned(), "web".to_owned())]),
                annotations: BTreeMap::new(),
            },
            spec: PodSpec {
                containers: vec![rk_types::Container {
                    name: "c".to_owned(),
                    image: image.to_owned(),
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
        }
    }

    #[test]
    fn hash_is_stable_and_image_sensitive() {
        let a = pod_template_hash(&template("nginx:1.27"));
        let b = pod_template_hash(&template("nginx:1.27"));
        let c = pod_template_hash(&template("nginx:1.28"));
        assert_eq!(a, b, "same template hashes the same");
        assert_ne!(a, c, "image change changes the hash");
        assert_eq!(a.len(), 10);
    }

    #[test]
    fn hash_ignores_the_injected_label() {
        let bare = template("nginx:1.27");
        let mut labeled = bare.clone();
        labeled
            .metadata
            .labels
            .insert(POD_TEMPLATE_HASH_LABEL.to_owned(), "deadbeef".to_owned());
        assert_eq!(
            pod_template_hash(&bare),
            pod_template_hash(&labeled),
            "the template-hash label must not feed back into the hash"
        );
    }

    #[test]
    fn ownership_round_trips() {
        let owner = ObjectMeta {
            uid: Some("rs-uid-123".to_owned()),
            ..ObjectMeta::named("web-7d8f9")
        };
        let reference = owner_ref(ResourceKind::ReplicaSet, &owner);
        assert!(reference.controller);

        let mut child = ObjectMeta::named("web-7d8f9-abcde");
        child.owner_references.push(reference);
        assert!(owned_by(&child, ResourceKind::ReplicaSet, "rs-uid-123"));
        assert!(!owned_by(&child, ResourceKind::ReplicaSet, "other-uid"));
        assert!(!owned_by(&child, ResourceKind::Deployment, "rs-uid-123"));
        assert_eq!(
            controller_of(&child),
            Some((ResourceKind::ReplicaSet, "rs-uid-123"))
        );
    }

    #[test]
    fn liveness_predicates() {
        let mut pod = Pod {
            api_version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            metadata: ObjectMeta::named("p"),
            spec: PodSpec::default(),
            status: PodStatus::default(),
        };
        assert!(!is_terminal(&pod));
        assert!(!is_ready(&pod), "Pending is not ready");

        pod.status.phase = PodPhase::Running;
        pod.status.container_statuses = vec![ContainerStatus {
            name: "c".to_owned(),
            ready: true,
            restart_count: 0,
            state: Default::default(),
        }];
        assert!(is_ready(&pod));

        pod.status.phase = PodPhase::Failed;
        assert!(is_terminal(&pod));
        assert!(!is_ready(&pod));
    }

    #[test]
    fn rand_suffix_shape() {
        let s = rand_suffix();
        assert_eq!(s.len(), 5);
        assert!(
            s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
    }
}
