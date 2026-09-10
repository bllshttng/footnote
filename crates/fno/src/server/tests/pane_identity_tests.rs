//! The per-pane orphan verdict (v71): a pane whose stored member the evidence
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

/// (x-688b) The name tier: a pane whose spawn captured a worker name the
/// registry join never resolved is fno's worker pane, and it is orphaned the
/// moment the shared fold judges that name dead. A held receipt (resumable
/// worker) and a nameless shell pane both stay kept.
#[test]
fn a_spawned_name_pane_is_orphaned_only_when_the_name_is_dead() {
    use crate::server::pane_identity::orphaned_by_spawned_name;
    let fold = |held: &[&str]| {
        let mut evidence = crate::squad_store::MemberEvidence::from_sets(
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        );
        evidence.fold_registry_rows(
            &[],
            ["t-688b-muxtabs".to_string()].into_iter().collect(),
            held.iter().map(|s| s.to_string()).collect(),
            true,
        );
        evidence
    };
    assert!(
        orphaned_by_spawned_name(Some("t-688b-muxtabs"), &fold(&[])),
        "a reaped spawned worker's pane closes as orphaned under default flags"
    );
    assert!(
        !orphaned_by_spawned_name(Some("t-688b-muxtabs"), &fold(&["t-688b-muxtabs"])),
        "a held receipt keeps the worker resumable and the pane kept"
    );
    assert!(
        !orphaned_by_spawned_name(None, &fold(&[])),
        "a shell pane with no spawned name never reaches the tier"
    );
}

#[test]
fn member_pane_reads_the_recorded_pane_id_first_and_falls_through_when_it_is_gone() {
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let recorded = core.spawn_pane(24, 80, "/a").unwrap();
    let joined = core.spawn_pane(24, 80, "/a").unwrap();
    core.worker_session_pane
        .insert(("codex".into(), "session-one".into()), joined);
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("t-worker".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("session-one".into()),
        pane_id: Some(recorded),
    };
    assert_eq!(
        core.member_pane(&member),
        Some(recorded),
        "a recorded live pane id wins over the derived join"
    );
    let gone = crate::squad_store::StoredMember {
        pane_id: Some(joined + 100),
        ..member.clone()
    };
    assert_eq!(
        core.member_pane(&gone),
        Some(joined),
        "a dead recorded id falls through to the member's own session join"
    );
    let stranger = crate::squad_store::StoredMember {
        harness_session_id: Some("session-two".into()),
        ..gone.clone()
    };
    assert_eq!(
        core.member_pane(&stranger),
        None,
        "a dead recorded id never lands on another worker's pane"
    );
}
