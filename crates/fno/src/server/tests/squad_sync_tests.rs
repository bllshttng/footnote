//! Squad-store sync (v71): a prune that only writes `squads.json` is undone
//! by the next `persist_squad`, because the server's in-memory member list is
//! authoritative and rewrites the file on every pane event. These tests pin
//! the fix (`reload_members_from_store` between the file pass and the next
//! persist) and keep the negative control that documents the original defect:
//! a "the file shrank" assertion passes today and proves nothing, because
//! today's file shrinks and then refills. Mounted as a child of server.rs's
//! `mod tests` (use super::*), so server.rs pays one line for it.

use super::*;

/// The operator's sequence with the fix in place: the CLI's file pass, the
/// reload, then one pane event's persist. The reaped member is absent from
/// BOTH the store and memory - the positive marker.
#[test]
fn prune_reload_survives_the_next_persist() {
    let _s = StoreScratch::new("squad-sync-marker");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.squad_members.insert(
        7,
        vec![
            stored_member("deadbee1", true),
            stored_member("feed0002", false),
        ],
    );
    core.persist_squad(7);
    // The CLI's file pass: evidence marks deadbee1 dead; every squad row is
    // kept (--dead-only shape), so only its dead member is reaped.
    let evidence = crate::squad_store::MemberEvidence::from_sets(
        std::collections::HashSet::new(),
        ["deadbee1".to_string()].into_iter().collect(),
    );
    let outcome = crate::squad_store::prune_with_evidence(
        |_| crate::squad_store::PruneDecision::Keep,
        &evidence,
    )
    .unwrap();
    assert_eq!(
        outcome.members_reaped, 1,
        "the file pass reaped the dead member"
    );
    // The fix: the live server re-reads the file it did not write.
    let receipt = core.reload_members_from_store();
    assert_eq!((receipt.squads, receipt.members), (1, 1));
    // One pane event's write: with the reload, it cannot resurrect the reaped
    // member from memory.
    core.persist_squad(7);
    let expected = vec![stored_member("feed0002", false)];
    assert_eq!(
        crate::squad_store::load().squads[0].members,
        expected,
        "the reaped member stays reaped in the store"
    );
    assert_eq!(
        core.squad_members[&7], expected,
        "and in the server's own member list"
    );
}

/// The same sequence WITHOUT the reload: the next persist writes the reaped
/// member back. This is the bug; it is why "the file shrank" proves nothing.
#[test]
fn prune_without_reload_is_undone_by_the_next_persist() {
    let _s = StoreScratch::new("squad-sync-control");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.squad_members.insert(
        7,
        vec![
            stored_member("deadbee1", true),
            stored_member("feed0002", false),
        ],
    );
    core.persist_squad(7);
    let evidence = crate::squad_store::MemberEvidence::from_sets(
        std::collections::HashSet::new(),
        ["deadbee1".to_string()].into_iter().collect(),
    );
    crate::squad_store::prune_with_evidence(|_| crate::squad_store::PruneDecision::Keep, &evidence)
        .unwrap();
    // No reload: memory still holds both members, and the next pane event
    // persists that list over the pruned file.
    core.persist_squad(7);
    assert_eq!(
        crate::squad_store::load().squads[0].members.len(),
        2,
        "the reaped member is back - the control that shows the marker test can fail"
    );
}

/// The receipt counts: a squad whose store row survives with one of two
/// members, and a squad the store no longer carries at all.
#[test]
fn reload_receipt_counts_squads_members_and_emptied() {
    let _s = StoreScratch::new("squad-sync-receipt");
    let mut core = empty_core();
    core.session.add_squad(
        7,
        vec!["/repo".into()],
        Some("harden".into()),
        Tab {
            name: None,
            id: 5,
            root: Node::Leaf(1),
            focus: 1,
        },
    );
    core.session.add_squad(
        8,
        vec!["/gone".into()],
        Some("ghost".into()),
        Tab {
            name: None,
            id: 6,
            root: Node::Leaf(2),
            focus: 2,
        },
    );
    core.squad_members.insert(
        7,
        vec![
            stored_member("deadbee1", true),
            stored_member("feed0002", false),
        ],
    );
    core.squad_members
        .insert(8, vec![stored_member("bee50003", false)]);
    core.persist_squad(7);
    // Squad 8 was never persisted: the store does not carry it.
    let receipt = core.reload_members_from_store();
    assert_eq!(
        (receipt.squads, receipt.members, receipt.emptied),
        (2, 2, 1),
        "both squads reloaded; squad 7 keeps its two members, squad 8 empties"
    );
    assert_eq!(core.squad_members[&8], Vec::new());
}
