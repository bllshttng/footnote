//! Squad-store sync (v71): a prune that only writes `squads.json` is undone
//! by the next `persist_squad`, because the server's in-memory member list is
//! authoritative and rewrites the file on every pane event. These tests pin
//! the fix (`reload_members_from_store` between the file pass and the next
//! persist) and keep the negative control that documents the original defect:
//! a "the file shrank" assertion passes today and proves nothing, because
//! today's file shrinks and then refills. Mounted as a child of server.rs's
//! `mod tests` (use super::*), so server.rs pays one line for it.

use super::*;

/// (x-78c1) The reload is the prune's server-side arm: it must end in a
/// layout push so EVERY attached client's catalog re-converges, not just the
/// caller's receipt. Two clients, one reload, both reliable channels carry a
/// Layout naming the live squads.
#[test]
fn squad_reload_pushes_a_layout_to_every_attached_client() {
    let _s = StoreScratch::new("squad-sync-reload-push");
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
    let (c1, mut rx1) = client_with_rx(1);
    let (c2, mut rx2) = client_with_rx(2);
    core.clients.push(c1);
    core.clients.push(c2);
    let layout_tabs = |rx: &mut mpsc::Receiver<ServerMsg>| -> Option<usize> {
        let mut tabs = None;
        while let Ok(ServerMsg::Layout { squads, .. }) = rx.try_recv() {
            tabs = squads.iter().find(|s| s.id == 7).map(|s| s.tabs.len());
        }
        tabs
    };
    // The receipt counts squads the server holds members for.
    core.squad_members
        .insert(7, vec![stored_member("feed0002", false)]);

    // Clients pushed directly onto the roster emit nothing until a mutation;
    // drain both so only POST-reload traffic can satisfy the marker.
    assert!(layout_tabs(&mut rx1).is_none());
    assert!(layout_tabs(&mut rx2).is_none());

    let (tx, mut rrx) = tokio::sync::oneshot::channel();
    core.handle_squad_reload(tx);
    let receipt = match rrx.try_recv() {
        Ok(ServerMsg::SquadReloaded { squads, .. }) => squads,
        other => panic!("reload answered with a receipt: {other:?}"),
    };
    assert_eq!(receipt, 1);
    assert_eq!(
        layout_tabs(&mut rx1),
        Some(1),
        "client 1 got a post-reload Layout"
    );
    assert_eq!(
        layout_tabs(&mut rx2),
        Some(1),
        "client 2 got a post-reload Layout"
    );
}

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

/// The same sequence without reload: generation CAS still protects the prune.
#[test]
fn prune_without_reload_survives_the_next_persist() {
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
    // Memory still holds both members, but its stale generation cannot replace
    // the pruned file.
    core.persist_squad(7);
    assert_eq!(
        crate::squad_store::load().squads[0].members.len(),
        1,
        "generation CAS preserves the externally-pruned membership"
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

/// (x-688b) The daemon's cached-row fold agrees with the CLI's file-read
/// fold: both go through `fold_registry_rows`, so a journal-spawned name the
/// published rows no longer carry reads Dead - but only while the reader's
/// read itself succeeded (`read_ok`); a reader that never resolved its stores
/// keeps every member fail-safe Unknown.
#[test]
fn daemon_fold_reaps_absent_spawned_names_only_on_a_good_read() {
    let mut core = empty_core();
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("w1".into()),
        harness: Some("claude".into()),
        harness_session_id: None,
        pane_id: None,
    };
    let journal = crate::spawn_journal::SpawnJournal {
        receipts: HashMap::new(),
        never_bound: HashMap::new(),
        spawned_names: ["w1".to_string()].into_iter().collect(),
        error: None,
    };
    core.handle_msg(CoreMsg::AgentRows {
        rows: Vec::new(),
        branches: HashMap::new(),
        tails: HashMap::new(),
        read_ok: true,
    });
    assert_eq!(
        core.member_evidence_with_journal(&journal).verdict(&member),
        crate::squad_store::MemberLiveness::Dead,
        "the daemon sweep and the CLI apply reap the same reaped worker"
    );
    core.handle_msg(CoreMsg::AgentRows {
        rows: Vec::new(),
        branches: HashMap::new(),
        tails: HashMap::new(),
        read_ok: false,
    });
    assert_eq!(
        core.member_evidence_with_journal(&journal).verdict(&member),
        crate::squad_store::MemberLiveness::Unknown,
        "an unresolved read keeps the daemon fail-safe"
    );
}
