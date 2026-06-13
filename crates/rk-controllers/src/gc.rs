//! Owner-reference garbage collection: an object whose controller owner no
//! longer exists is an orphan and gets deleted. Run after the controllers
//! each pass, this is what cascades a delete — remove a Deployment and its
//! ReplicaSets orphan and are collected, then their Pods orphan and follow.
//! Pure: it decides orphanhood; the loop does the deletes.

use std::collections::HashSet;

use rk_types::ResourceKind;
use rk_types::meta::ObjectMeta;

use crate::support;

/// True if `meta` is controlled by an object of `owner_kind` whose uid is not
/// among the live ones. Objects with no controller, or controlled by some
/// other kind, are left alone.
pub fn is_orphan(meta: &ObjectMeta, owner_kind: ResourceKind, live_uids: &HashSet<&str>) -> bool {
    match support::controller_of(meta) {
        Some((kind, uid)) if kind == owner_kind => !live_uids.contains(uid),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rk_types::OwnerReference;

    use super::*;

    fn owned_by(kind: ResourceKind, uid: &str) -> ObjectMeta {
        let mut meta = ObjectMeta::named("child");
        meta.owner_references.push(OwnerReference {
            kind,
            name: "owner".to_owned(),
            uid: Some(uid.to_owned()),
            controller: true,
        });
        meta
    }

    #[test]
    fn orphan_when_owner_is_gone() {
        let live: HashSet<&str> = HashSet::from(["deploy-1"]);
        let child = owned_by(ResourceKind::Deployment, "deploy-deleted");
        assert!(is_orphan(&child, ResourceKind::Deployment, &live));

        let kept = owned_by(ResourceKind::Deployment, "deploy-1");
        assert!(!is_orphan(&kept, ResourceKind::Deployment, &live));
    }

    #[test]
    fn other_owners_and_no_owner_are_untouched() {
        let live: HashSet<&str> = HashSet::new();
        // Controlled by a different kind than we're collecting: not our orphan.
        let other = owned_by(ResourceKind::ReplicaSet, "rs-1");
        assert!(!is_orphan(&other, ResourceKind::Deployment, &live));
        // No controller at all (e.g. a hand-applied pod): never collected.
        let standalone = ObjectMeta::named("standalone");
        assert!(!is_orphan(&standalone, ResourceKind::ReplicaSet, &live));
    }
}
