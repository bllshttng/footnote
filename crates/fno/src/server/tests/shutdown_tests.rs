use super::*;

fn deadbeef_member() -> crate::squad_store::StoredMember {
    crate::squad_store::StoredMember {
        attach_id: "deadbeef".into(),
        tombstone: false,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: None,
        harness: None,
        harness_session_id: None,
    }
}

#[test]
fn capture_topology_now_writes_restored_clean_layout() {
    let _scratch = StoreScratch::new("shutdown-capture-restored");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = false;
    core.store_generations = crate::squad_store::load().generations;

    assert!(core.capture_topology_now());
    assert!(!core.topology_dirty, "capture drains the dirty flag");
    let stored = crate::squad_store::load();
    let squad = stored
        .squads
        .iter()
        .find(|s| s.name == "sq")
        .expect("capture persists the live squad");
    assert_eq!(squad.tab_trees.len(), 1);
    assert_eq!(squad.tab_trees[0].slots.len(), 1);
}

#[test]
fn capture_topology_now_skips_pending_startup_restore() {
    let scratch = StoreScratch::new("shutdown-capture-pending");
    let (mut core, _) = template_core();
    core.restored = true;
    core.restore_pending = true;
    core.store_generations = crate::squad_store::load().generations;

    assert!(!core.capture_topology_now());
    assert!(
        !scratch.dir.join("squads.json").exists(),
        "pending startup restore leaves the durable snapshot untouched"
    );
}

#[test]
fn capture_topology_now_skips_never_restored_server() {
    let scratch = StoreScratch::new("shutdown-capture-skipped");
    let (mut core, _) = template_core();

    assert!(!core.capture_topology_now());
    assert!(
        !scratch.dir.join("squads.json").exists(),
        "a never-restored server leaves the store untouched"
    );
}

#[test]
fn clean_shutdown_does_not_overwrite_a_newer_store_generation() {
    let _scratch = StoreScratch::new("shutdown-capture-concurrent");
    let (mut older, _) = template_core();
    older.restored = true;
    older.topology_dirty = true;
    older.flush_topology();
    let baseline = older.store_generations.clone();

    crate::squad_store::upsert("sq", "", &["/a".into()], &[deadbeef_member()]).unwrap();
    assert!(
        crate::squad_store::load().generations != baseline,
        "the newer writer advances the overlapping squad generation"
    );

    assert!(!older.capture_topology_now());

    let stored = crate::squad_store::load();
    let squad = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert!(
        squad.members.iter().any(|m| m.attach_id == "deadbeef"),
        "an older clean server must preserve the newer writer's snapshot"
    );
}

#[test]
fn unrelated_squad_write_does_not_block_clean_shutdown_capture() {
    let _scratch = StoreScratch::new("shutdown-capture-disjoint");
    let (mut older, _) = template_core();
    older.restored = true;
    older.topology_dirty = true;
    older.flush_topology();
    older.session.squad_mut(1).unwrap().tabs[0].name = Some("new-local-name".into());

    crate::squad_store::upsert("other", "", &["/other".into()], &[]).unwrap();

    assert!(older.capture_topology_now());
    let stored = crate::squad_store::load();
    let local = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert_eq!(
        local.tab_trees[0].tab_name.as_deref(),
        Some("new-local-name")
    );
    assert!(stored.squads.iter().any(|s| s.name == "other"));
}

#[test]
fn dirty_shutdown_does_not_overwrite_a_newer_overlapping_snapshot() {
    let _scratch = StoreScratch::new("shutdown-capture-dirty-conflict");
    let (mut older, _) = template_core();
    older.restored = true;
    older.topology_dirty = true;
    older.flush_topology();
    crate::squad_store::upsert("sq", "", &["/a".into()], &[deadbeef_member()]).unwrap();
    older.topology_dirty = true;

    assert!(!older.capture_topology_now());
    let stored = crate::squad_store::load();
    let squad = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert!(squad.members.iter().any(|m| m.attach_id == "deadbeef"));
}

#[test]
fn normal_flush_cannot_erase_an_external_write_before_shutdown() {
    let _scratch = StoreScratch::new("shutdown-capture-normal-flush-conflict");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = true;
    core.flush_topology();
    crate::squad_store::upsert("sq", "", &["/a".into()], &[deadbeef_member()]).unwrap();
    core.session.squad_mut(1).unwrap().tabs[0].name = Some("stale-local".into());
    core.topology_dirty = true;

    core.flush_topology();
    assert!(!core.capture_topology_now());
    let stored = crate::squad_store::load();
    let squad = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert!(squad.members.iter().any(|m| m.attach_id == "deadbeef"));
}

#[test]
fn local_store_write_refreshes_the_shutdown_generation_baseline() {
    let _scratch = StoreScratch::new("shutdown-capture-local-write");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = true;
    core.flush_topology();
    core.persist_stored("sq", "", &["/a".into()], &[deadbeef_member()]);
    core.session.squad_mut(1).unwrap().tabs[0].name = Some("shutdown-only".into());

    assert!(core.capture_topology_now());
    let stored = crate::squad_store::load();
    let squad = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert_eq!(
        squad.tab_trees[0].tab_name.as_deref(),
        Some("shutdown-only")
    );
}

#[test]
fn store_reload_refreshes_the_shutdown_generation_baseline() {
    let _scratch = StoreScratch::new("shutdown-capture-store-reload");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = true;
    core.flush_topology();
    crate::squad_store::upsert("sq", "", &["/a".into()], &[deadbeef_member()]).unwrap();
    core.reload_members_from_store();
    core.session.squad_mut(1).unwrap().tabs[0].name = Some("after-reload".into());

    assert!(core.capture_topology_now());
    let stored = crate::squad_store::load();
    let squad = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert_eq!(squad.tab_trees[0].tab_name.as_deref(), Some("after-reload"));
}

#[test]
fn conflicting_squad_does_not_discard_a_fresh_disjoint_snapshot() {
    let _scratch = StoreScratch::new("shutdown-capture-partial");
    let (mut older, pane) = template_core();
    older
        .session
        .add_squad(2, vec!["/b".into()], Some("sq2".into()), leaf_tab(6, pane));
    older.squad_members.insert(2, Vec::new());
    older.restored = true;
    older.topology_dirty = true;
    older.flush_topology();
    let newer_member = crate::squad_store::StoredMember {
        attach_id: "deadbeef".into(),
        tombstone: false,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: None,
        harness: None,
        harness_session_id: None,
    };
    crate::squad_store::upsert("sq", "", &["/a".into()], &[newer_member]).unwrap();
    older.session.squad_mut(2).unwrap().tabs[0].name = Some("fresh-local".into());

    assert!(!older.capture_topology_now());
    let stored = crate::squad_store::load();
    let disjoint = stored.squads.iter().find(|s| s.name == "sq2").unwrap();
    assert_eq!(
        disjoint.tab_trees[0].tab_name.as_deref(),
        Some("fresh-local")
    );
}

#[test]
fn pane_id_reservation_does_not_advance_squad_generation() {
    let _scratch = StoreScratch::new("shutdown-capture-pane-id");
    let before = crate::squad_store::load().generations;
    crate::squad_store::reserve_next_pane_id(1).unwrap();
    assert_eq!(crate::squad_store::load().generations, before);
}

#[test]
fn completed_batch_restore_clears_the_pending_guard() {
    let _scratch = StoreScratch::new("shutdown-capture-batch-complete");
    let mut core = empty_core();
    core.restore_pending = true;
    core.handle(CoreMsg::BatchPlansReady {
        id: 1,
        plans: HashMap::new(),
        replay: Box::new(BatchReplay::Restore {
            home_sid: 999,
            rows: 24,
            cols: 80,
        }),
    });
    assert!(!core.restore_pending);
}

#[test]
fn clean_shutdown_batches_multiple_squads_into_one_generation() {
    let _scratch = StoreScratch::new("shutdown-capture-batch");
    let (mut core, pane) = template_core();
    core.session
        .add_squad(2, vec!["/b".into()], Some("sq2".into()), leaf_tab(6, pane));
    core.squad_members.insert(2, Vec::new());
    core.restored = true;
    core.topology_dirty = true;
    core.flush_topology();
    let before = core.store_generations.clone();

    assert!(core.capture_topology_now());

    assert_eq!(before.len(), 2, "both squads have generation baselines");
    for (key, generation) in before {
        assert_eq!(
            core.store_generations.get(&key),
            Some(&(generation + 1)),
            "each shutdown snapshot advances exactly once"
        );
    }
}
