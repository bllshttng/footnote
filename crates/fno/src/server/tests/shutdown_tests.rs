use super::*;

#[test]
fn capture_topology_now_writes_restored_clean_layout() {
    let _scratch = StoreScratch::new("shutdown-capture-restored");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = false;

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
fn capture_topology_now_skips_never_restored_server() {
    let scratch = StoreScratch::new("shutdown-capture-skipped");
    let (mut core, _) = template_core();

    assert!(!core.capture_topology_now());
    assert!(
        !scratch.dir.join("squads.json").exists(),
        "a never-restored server leaves the store untouched"
    );
}
