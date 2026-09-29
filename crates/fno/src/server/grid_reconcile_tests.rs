use super::super::tests::empty_core;

#[test]
fn tick_reconciliation_converges_a_grid_whose_resize_was_lost() {
    // Nothing before this pass ever re-checked `requested_size` against the
    // grid, so one lost resize met the client blit's content clamp and the
    // child's bottom rows silently vanished.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(24, 80, "/tmp").expect("pane");
    core.panes.get_mut(&pid).unwrap().requested_size = (12, 40);
    core.reconcile_grid_sizes();
    assert_eq!(core.panes.get(&pid).unwrap().vt.size(), (12, 40));
    // Idempotent: a second pass over a converged grid stays a no-op.
    core.reconcile_grid_sizes();
    assert_eq!(core.panes.get(&pid).unwrap().vt.size(), (12, 40));
}
