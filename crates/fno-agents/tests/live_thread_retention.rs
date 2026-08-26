use fno::agents_view::{Liveness, RegistryAgent};
use fno::proto::AgentRow;
use fno::squad_store::{MemberEvidence, MemberLiveness, StoredMember};
use fno_agents::gc::{gc_action, GcAction, GcRow, KeepReason};
use fno_agents::AgentStatus;

#[test]
fn live_thread_identity_survives_store_gc_and_sideline_facts() {
    let worker = "thread-worker";
    let harness = "codex";
    let session_id = "01abcdef-full-session";
    let member = StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tab_name: Some("worker-tab".into()),
        cwd: Some("/repo/worktree".into()),
        worker: Some(worker.into()),
        harness: Some(harness.into()),
        harness_session_id: Some(session_id.into()),
    };
    let registry = RegistryAgent {
        spawned_by_session: None,
        session_id: None,
        harness_session_id: Some(session_id.into()),
        name: worker.into(),
        cwd: "/repo/worktree".into(),
        harness: Some(harness.into()),
        exited: false,
        badge: None,
        reason: None,
        mux: None,
        answerable: None,
        attach_id: None,
        external: false,
        account: None,
        claude_session_uuid: None,
        updated_at: None,
        crown_level: None,
        crown_scope: None,
        liveness: Liveness::Alive,
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
        squad: Some(7),
        name: registry.name.clone(),
        pane_id: None,
        badge: None,
        reason: None,
        exited: registry.exited,
        unmeasured: false,
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
    };
    assert_eq!(sideline.harness_session_id.as_deref(), Some(session_id));

    let live = GcRow {
        status: AgentStatus::Exited,
        is_live: true,
        pid_confirmed_dead: true,
        owns_worktree: false,
        exited_at: Some(9),
        liveness_surface: true,
        transcript_fresh: Some(false),
        harness_session_gone: Some(true),
        dormant_done: false,
        worktree_clean: Some(true),
    };
    assert_eq!(gc_action(&live, 10_000, 1), GcAction::Keep);
    assert_eq!(
        fno_agents::gc::keep_reason(&live, 10_000, 1),
        Some(KeepReason::Live)
    );
    assert_eq!(member.harness.as_deref(), Some(harness));
    assert_eq!(member.harness_session_id.as_deref(), Some(session_id));
}
