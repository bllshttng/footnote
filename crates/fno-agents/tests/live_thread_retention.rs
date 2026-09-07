use fno::agents_view::{Liveness, RegistryAgent};
use fno::proto::AgentRow;
use fno::squad_store::{MemberEvidence, MemberLiveness, StoredMember};
use fno_agents::gc::{gc_decide, GcAction, GcRow, KeepReason};

#[test]
fn live_thread_identity_survives_store_gc_and_sideline_facts() {
    let worker = "thread-worker";
    let harness = "codex";
    let session_id = "01abcdef-full-session";
    let member = StoredMember {
        attach_id: String::new(),
        tombstone: false,
        detached: false,
        tab_name: Some("worker-tab".into()),
        cwd: Some("/repo/worktree".into()),
        worker: Some(worker.into()),
        harness: Some(harness.into()),
        harness_session_id: Some(session_id.into()),
    };
    let registry = RegistryAgent {
        name: worker.into(),
        cwd: "/repo/worktree".into(),
        harness: Some(harness.into()),
        harness_session_id: Some(session_id.into()),
        ..Default::default()
    };
    let mut evidence = MemberEvidence::from_sets(
        [worker.to_string(), session_id.to_string()]
            .into_iter()
            .collect(),
        std::collections::HashSet::new(),
    );
    evidence.add_live_pair(harness, session_id);
    assert_eq!(evidence.verdict(&member), MemberLiveness::Live);
    let sideline = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        squad: Some(7),
        name: registry.name.clone(),
        pane_id: None,
        badge: None,
        reason: None,
        exited: registry.exited,
        unmeasured: false,
        liveness_age_s: None,
        harness_title: None,
        answerable: None,
        attach_id: None,
        external: false,
        seen: false,
        tab: None,
        cwd_base: Some("worktree".into()),
        tombstone: false,
        subline: None,
        account: None,
        updated_at: None,
        pr: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        spawned_by_session: None,
        harness_session_id: registry.harness_session_id.clone(),
        basis: None,
        last_activity_age_s: None,
        resumable: false,
        no_pane_reason: None,
        pane_activity: None,
        reach: Default::default(),
        dnd: false,
    };
    assert_eq!(sideline.harness_session_id.as_deref(), Some(session_id));

    // A thread whose transcript was just written is WRITING now, whatever a
    // stored status says: retirement keys on the served activity and the
    // graph, and both hold it.
    let live = GcRow {
        origin: Some("spawn".into()),
        crowned: false,
        work: fno_agents::graph_store::WorkState::AllDone {
            nodes: vec!["N1".into()],
        },
        transcript_age_s: Some(4),
        owns_worktree: false,
        worktree_clean: None,
        branch_merged: None,
    };
    assert_eq!(gc_decide(&live, 900).0, GcAction::Keep);
    assert_eq!(
        gc_decide(&live, 900).1,
        Some(KeepReason::Active { age_s: 4 })
    );
    assert_eq!(member.harness.as_deref(), Some(harness));
    assert_eq!(member.harness_session_id.as_deref(), Some(session_id));
}
