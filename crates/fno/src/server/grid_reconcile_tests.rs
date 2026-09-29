use super::super::tests::empty_core;

#[test]
fn tick_reconciliation_converges_a_grid_whose_resize_was_lost() {
    // Nothing before this pass ever re-checked `requested_size` against the
    // grid, so one lost resize met the client blit's content clamp and the
    // child's bottom rows silently vanished. Also carries the old vt-layer
    // clamp proof (resize clamps to 1x1, feed and frame stay safe), driven
    // through the real caller: no production path sends zeros today (layout
    // floors + the keeper's max(1)), so the clamp is defensive armor and
    // this is its strongest boundary.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(24, 80, "/tmp").expect("pane");

    // A lost resize: requested moved on, the grid never followed.
    core.panes.get_mut(&pid).unwrap().requested_size = (12, 40);
    core.reconcile_grid_sizes();
    assert_eq!(core.panes.get(&pid).unwrap().vt.size(), (12, 40));
    // Idempotent: a second pass over a converged grid stays a no-op.
    core.reconcile_grid_sizes();
    assert_eq!(core.panes.get(&pid).unwrap().vt.size(), (12, 40));

    // A degenerate requested size rides the vt clamp and stays safe.
    core.panes.get_mut(&pid).unwrap().requested_size = (0, 0);
    core.reconcile_grid_sizes();
    assert_eq!(core.panes.get(&pid).unwrap().vt.size(), (1, 1));
    core.panes.get_mut(&pid).unwrap().vt.feed(b"q");
    assert_eq!(core.panes.get(&pid).unwrap().vt.frame().cells.len(), 1);
}
