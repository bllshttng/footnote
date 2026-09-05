//! The per-pane orphan verdict (v69): a pane whose stored member the evidence
//! judges Dead, with no live registry row on it, is orphaned and its tab
//! closes under the default prune. Mounted as a child of server.rs's
//! `mod tests` (use super::*).

use super::*;

fn dead_evidence(attach: &str) -> crate::squad_store::MemberEvidence {
    crate::squad_store::MemberEvidence::from_sets(
        std::collections::HashSet::new(),
        [attach.to_string()].into_iter().collect(),
    )
}

#[test]
fn dead_member_bound_to_a_pane_reads_orphaned() {
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 1, "deadbee1");
    let evidence = dead_evidence("deadbee1");
    assert!(
        core.orphaned_worker_for_pane(1, &[], &evidence),
        "a Dead member's pane is orphaned"
    );
}

#[test]
fn a_live_registry_row_on_the_pane_beats_the_dead_evidence() {
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 1, "deadbee1");
    let evidence = dead_evidence("deadbee1");
    let mut row = exited_claude_row("harden-worker", None);
    row.mux = Some(("test".into(), 1));
    row.liveness = agents_view::Liveness::Alive;
    assert!(
        !core.orphaned_worker_for_pane(1, &[row], &evidence),
        "a live row on the pane means the worker is not orphaned"
    );
}

#[test]
fn a_pane_with_no_member_binding_is_not_orphaned() {
    let mut core = empty_core();
    named_member_squad(&mut core, 7, "harden", 1, "deadbee1");
    // Pane 2 hosts nothing: no member binds to it.
    let evidence = dead_evidence("deadbee1");
    assert!(
        !core.orphaned_worker_for_pane(2, &[], &evidence),
        "no binding, no verdict"
    );
}
