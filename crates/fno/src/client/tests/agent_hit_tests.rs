//! The agent_hit gesture-resolution family, moved verbatim out of
//! client_tests.rs (file budget shrink). Parent helpers resolve
//! through the glob.
use super::*;

#[test]
fn agent_hit_resolves_pane_then_attach_then_notice() {
    // The shared seam (x-653d): a keyboard goto and a mouse click resolve an
    // agent to the SAME ChromeHit. pane > attach > notice.
    let hosted = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "a".into(),
        pane_id: Some(7),
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        liveness_age_s: None,
        harness_title: None,
        answerable: None,
        attach_id: None,
        external: false,
        seen: false,
        cwd_base: None,
        tombstone: false,
        subline: None,
        tab: None,
        account: None,
        updated_at: None,
        pr: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        basis: None,
        last_activity_age_s: None,
        resumable: false,
        no_pane_reason: None,
        pane_activity: None,
    };
    // A pane-hosted row focuses regardless of the active squad.
    assert!(
        matches!(agent_hit(&hosted, 2), ChromeHit::Cmds(c) if c == vec![Command::FocusPane(7)])
    );
    // (x-07c2) A watch-only attachable row (any workspace) reaches the ONE
    // dedicated thread pane: one AttachAgent with the thread_pane flag,
    // no placement dialog. The server owns the tier.
    let bg = AgentRow {
        harness: None,
        model: None,
        route: None,
        pane_id: None,
        portal: None,
        attach_id: Some("job1".into()),
        ..hosted.clone()
    };
    assert!(matches!(
        agent_hit(&bg, 2),
        ChromeHit::Cmds(c) if c == vec![Command::AttachAgent {
            id: "job1".into(),
            placement: PanePlacement { portal: Some(0), ..Default::default() },
        }]
    ));
    // A TOMBSTONE row keeps its attach_id (the client needs it to dismiss),
    // so the reach arm gates on !exited: a dead agent never reaches, it
    // notices.
    let dead = AgentRow {
        harness: None,
        model: None,
        route: None,
        pane_id: None,
        portal: None,
        attach_id: Some("job1".into()),
        exited: true,
        dnd: false,
        unmeasured: false,
        liveness_age_s: None,
        harness_title: None,
        tombstone: true,
        ..hosted.clone()
    };
    assert!(matches!(agent_hit(&dead, 2), ChromeHit::Notice(_)));
    // A live paneless row with NO attach id reaches BY NAME - the Follow
    // and Locate tiers (the dedicated pane tails or explains).
    let orphan = AgentRow {
        harness: None,
        model: None,
        route: None,
        name: "t-live-paneless".into(),
        pane_id: None,
        portal: None,
        attach_id: None,
        no_pane_reason: Some(AgentNoPaneReason::LivePaneless),
        ..hosted.clone()
    };
    assert!(matches!(
        agent_hit(&orphan, 2),
        ChromeHit::Cmds(c) if c == vec![Command::AttachAgent {
            id: "t-live-paneless".into(),
            placement: PanePlacement { portal: Some(0), ..Default::default() },
        }]
    ));
    // The LivePaneless notice itself still exists (render paths and the
    // server refusal echo it); its actionable peek command must survive
    // narrow clipping. Driven directly, not through agent_hit: the click
    // path now EXECUTES that advice in the dedicated pane instead.
    let mut narrow = two_pane_view();
    narrow.set_notice(no_pane_notice(&orphan));
    let (_, clipped) = narrow.notice_overlay(80).expect("notice is set");
    assert!(
        clipped.contains("fno agents peek t-live-paneless --follow"),
        "the actionable command must survive narrow clipping: {clipped}"
    );

    for (reason, marker) in [
        (AgentNoPaneReason::MissingHarness, "no harness recorded"),
        (
            AgentNoPaneReason::MissingSessionId,
            "supported harness has no session id",
        ),
        (AgentNoPaneReason::UnsupportedHarness, "unsupported harness"),
    ] {
        let dead = AgentRow {
            portal: None,
            harness: None,
            model: None,
            route: None,
            name: "t-dead-paneless".into(),
            exited: true,
            no_pane_reason: Some(reason),
            pane_activity: None,
            ..orphan.clone()
        };
        match agent_hit(&dead, 2) {
            ChromeHit::Notice(text) => {
                assert!(text.contains("t-dead-paneless"), "dead notice: {text}");
                assert!(text.contains(marker), "dead notice: {text}");
                assert!(!text.contains("live"), "dead notice misclassified: {text}");
            }
            other => panic!(
                "dead paneless reason must remain a notice: {}",
                chrome_hit_label(&Some(other))
            ),
        }
    }
}

#[test]
fn agent_hit_resumes_a_resumable_paneless_row() {
    // x-5f7f: a paneless row the server marked resumable (its harness owns
    // a resume form and the row carries the session id) resolves to
    // ResumeAgent - the operator's explicit gesture. Ordering: attach
    // still wins while a claude bg row is live and carries a jobId;
    // resumable takes the dead-and-nameless cases the notice used to eat.
    let row = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "t-codex-one".into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited: true,
        dnd: false,
        unmeasured: false,
        liveness_age_s: None,
        harness_title: None,
        answerable: None,
        attach_id: None,
        external: false,
        seen: false,
        cwd_base: None,
        tombstone: false,
        subline: None,
        tab: None,
        account: None,
        updated_at: None,
        pr: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        basis: None,
        last_activity_age_s: None,
        resumable: true,
        no_pane_reason: None,
        pane_activity: None,
    };
    assert!(matches!(
        agent_hit(&row, 2),
        ChromeHit::Cmds(c)
            if c == vec![Command::ResumeAgent { name: "t-codex-one".into() }]
    ));
    // A live attachable row reaches the dedicated thread pane even if a
    // stale server also flagged it resumable: while the daemon owns the
    // session, attaching is the cheaper truth, and resuming a live row
    // would mint a second writer (the LivePaneless warning).
    let attachable = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        attach_id: Some("c19cd2c3".into()),
        exited: false,
        resumable: true,
        no_pane_reason: None,
        pane_activity: None,
        ..row.clone()
    };
    assert!(matches!(
        agent_hit(&attachable, 2),
        ChromeHit::Cmds(c) if matches!(
            c.as_slice(),
            [Command::AttachAgent { placement, .. }] if placement.portal_target() == Some(0)
        )
    ));
}

#[test]
fn agent_hit_watch_only_reaches_the_thread_pane() {
    // (x-07c2) A watch-only attachable row (any workspace) resolves to the
    // dedicated thread pane: one AttachAgent carrying the thread_pane
    // flag and the row's attach id, no placement dialog. The explicit
    // placement gestures (picker `p`, menu splits, open-here, drag) still
    // pin a persisted pane when the operator wants one.
    let row = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Drive,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "sib".into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        liveness_age_s: None,
        harness_title: None,
        answerable: None,
        attach_id: Some("job1".into()),
        external: false,
        seen: false,
        cwd_base: None,
        tombstone: false,
        tab: None,
        subline: None,
        account: None,
        updated_at: None,
        pr: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        basis: None,
        last_activity_age_s: None,
        resumable: false,
        no_pane_reason: None,
        pane_activity: None,
    };
    match agent_hit(&row, 1) {
        ChromeHit::Cmds(c) => assert!(
            matches!(
                c.as_slice(),
                [Command::AttachAgent { id, placement }] if id == "job1" && placement.portal_target() == Some(0)
            ),
            "expected a portal 0 reach, got {c:?}"
        ),
        other => panic!(
            "expected a thread-pane reach, got {}",
            chrome_hit_label(&Some(other))
        ),
    }
}

// A mission squad is a render-time grouping header, not a real squad
// `place_spawned_pane` can route a pane into - a mission-grouped row's
// placement must fall back to a real target, and the picker must never
// offer the virtual id as a choice (codex review of x-1a47 change 2/3,
// P1-b). Driven through the `p`-key door (attach_dst_squads +
// open_attach_place), the only door left since x-07c2 moved every
// deliberate attach gesture to the dedicated thread pane.
