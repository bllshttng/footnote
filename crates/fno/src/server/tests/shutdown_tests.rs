use super::*;

#[test]
fn capture_topology_now_writes_restored_clean_layout() {
    let _scratch = StoreScratch::new("shutdown-capture-restored");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = false;
    core.store_generation = Some(crate::squad_store::load().generation);

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
    core.store_generation = Some(crate::squad_store::load().generation);

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
    let baseline = older.store_generation.expect("older capture generation");

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
    assert!(
        crate::squad_store::load().generation > baseline,
        "the newer writer advances the squad generation"
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
fn pane_id_reservation_does_not_advance_squad_generation() {
    let _scratch = StoreScratch::new("shutdown-capture-pane-id");
    let before = crate::squad_store::load().generation;
    crate::squad_store::reserve_next_pane_id(1).unwrap();
    assert_eq!(crate::squad_store::load().generation, before);
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
