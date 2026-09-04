use super::*;
use crate::proto::{AnswerOption, AnswerablePrompt, PaneMeta, Reach, TabMeta};
use crate::vt::frame_text;

// (x-0719) The nav filter/overlay test run lives in its own module; this
// file is shrink-only under the file-budget gate.
#[path = "client/tests/nav_tests.rs"]
mod nav_tests;

// The x-9fd0 portal-placement-picker family lives in its own module too.
#[path = "client/tests/portal_pick_tests.rs"]
mod portal_pick_tests;

// The status-glyph family joined the nav family. The rename-overlay family
// (tab, squad, agent targets) lives in its own module too.
#[path = "client/tests/glyph_tests.rs"]
mod glyph_tests;

#[path = "client/tests/rename_tests.rs"]
mod rename_tests;

#[test]
fn config_says_off_matches_only_trimmed_off() {
    // Bridges config.toml -> the env the interactive server latches
    // (x-6165). Must mirror `pty::integration_disabled`: exactly `off`.
    assert!(config_says_off("off"));
    assert!(config_says_off("off\n")); // config get trailing newline
    assert!(config_says_off("  off  "));
    assert!(!config_says_off("mux-panes\n")); // the default -> stays on
    assert!(!config_says_off("OFF")); // case-sensitive, like the Rust side
    assert!(!config_says_off("")); // unknown key / empty -> default on
}

#[test]
fn mail_question_fold_item_renders_squadless_not_dropped() {
    // The client match arm must map mail_question -> a row, not `_ =>
    // continue` -- the second silent-eat path this node closes (the fold
    // produces the item; the client must not drop it). Always-live + no
    // roster row -> squadless render with the mail glyph.
    let mut view = two_pane_view();
    view.needs_fold = Some(vec![crate::needs_overlay::FoldItem {
        kind: "mail_question".into(),
        session_id: "web".into(),
        node: None,
        name: Some("web".into()),
        title: None,
        ts: "2026-07-03T02:00:00Z".into(),
        evidence: "question: etl -> web: which schema?".into(),
        live: true,
    }]);
    let rows = view.needs_queue();
    let mail = rows
        .iter()
        .find(|r| r.kind == NeedKind::MailQuestion)
        .expect("mail_question maps to a row, not dropped by _ => continue");
    assert_eq!(mail.name, "web");
    assert!(mail.pane_id.is_none(), "squadless: no roster join");
    assert_eq!(need_glyph(mail.kind), '✉');
}

#[test]
fn mail_delivery_miss_leaves_the_operator_queue_and_a_question_stays() {
    // The other half of the split: the fold now emits mail_delivery_miss for
    // a reachable-miss, and this arm has to DROP it. Asserted against the
    // rendered queue rather than the match arm, so it pins the destination
    // (no operator row) and not merely the tag.
    //
    // The question row is the positive control. Without it a client that
    // dropped every fold item would satisfy the absence half and read as
    // proof of a split that is not there.
    let mut view = two_pane_view();
    let item = |kind: &str, name: &str| crate::needs_overlay::FoldItem {
        kind: kind.into(),
        session_id: name.into(),
        node: None,
        name: Some(name.into()),
        title: None,
        ts: "2026-07-03T02:00:00Z".into(),
        evidence: format!("{kind}: sender -> {name}: ping"),
        live: true,
    };
    view.needs_fold = Some(vec![
        item("mail_delivery_miss", "019f48e1"),
        item("mail_question", "web"),
    ]);
    let rows = view.needs_queue();
    assert!(
        rows.iter().any(|r| r.name == "web"),
        "a real question still reaches the operator"
    );
    assert!(
        !rows.iter().any(|r| r.name == "019f48e1"),
        "a delivery miss is not an operator decision"
    );
}

#[test]
fn fold_search_input_swallows_multibyte_csi_without_leaking() {
    // gemini review (HIGH): a multi-byte CSI must be consumed whole, never
    // leak its param/final tail into the typed query.
    let mut esc = Vec::new();
    // Printables pass through 1:1.
    assert_eq!(
        fold_search_input(&mut esc, b"ab"),
        vec![SearchKey::Byte(b'a'), SearchKey::Byte(b'b')]
    );
    assert!(esc.is_empty());
    // Arrow (3-byte), PageUp (`ESC [ 5 ~`), Ctrl-Arrow (`ESC [ 1 ; 5 A`):
    // fully swallowed, nothing reaches the query.
    assert!(fold_search_input(&mut esc, b"\x1b[A").is_empty());
    assert!(fold_search_input(&mut esc, b"\x1b[5~").is_empty());
    assert!(fold_search_input(&mut esc, b"\x1b[1;5A").is_empty());
    assert!(esc.is_empty(), "each CSI sequence consumed whole");
    // Split across reads: the tail in the next chunk still never leaks.
    assert!(fold_search_input(&mut esc, b"\x1b[").is_empty());
    assert!(fold_search_input(&mut esc, b"5~").is_empty());
    assert!(esc.is_empty());
    // A bare Esc then a printable: Esc surfaces, the printable is NOT eaten.
    assert_eq!(
        fold_search_input(&mut esc, b"\x1bx"),
        vec![SearchKey::Esc, SearchKey::Byte(b'x')]
    );
    // gemini review (HIGH): an ESC arriving mid-CSI aborts the sequence and
    // restarts, so Esc-to-cancel works even with a split CSI pending. A
    // partial CSI (`ESC [ 1 ;`) then ESC then `x` must yield exactly one Esc
    // and the `x`, never swallow the cancel as a CSI param byte.
    assert!(fold_search_input(&mut esc, b"\x1b[1;").is_empty());
    assert_eq!(
        fold_search_input(&mut esc, b"\x1bx"),
        vec![SearchKey::Esc, SearchKey::Byte(b'x')]
    );
    assert!(esc.is_empty());
}

#[test]
fn pane_state_derives_worst_first_from_badge_and_seen() {
    // The x-653d state vocabulary: badge + seen -> PaneState. x-4328 flips
    // the seen bit later; today every Done is called with seen=false.
    assert_eq!(
        pane_state(Some(AgentBadge::Blocked), false, None),
        PaneState::Blocked
    );
    assert_eq!(
        pane_state(Some(AgentBadge::Working), false, None),
        PaneState::Working
    );
    assert_eq!(
        pane_state(Some(AgentBadge::Done), false, None),
        PaneState::DoneUnseen
    );
    assert_eq!(
        pane_state(Some(AgentBadge::Done), true, None),
        PaneState::Idle
    );
    // (x-d401) The blind fold is gone: no badge and no activity reading is
    // a marked absence, never a measured idle.
    assert_eq!(pane_state(None, false, None), PaneState::Unmeasured);
    // Worst-first ordering (Invariant): the squad rollup takes the `min`, so
    // the worst state must be the Ord-minimum - x-d140's `min` and the
    // navigator filter must agree on this ordering.
    assert!(PaneState::Blocked < PaneState::Working);
    assert!(PaneState::Working < PaneState::DoneUnseen);
    assert!(PaneState::DoneUnseen < PaneState::Idle);
    assert!(
        PaneState::Unmeasured < PaneState::Idle,
        "no-reading outranks settled idle in a worst-wins rollup"
    );
    assert!(
        PaneState::Idle < PaneState::Empty,
        "a pristine shell is the least severe reading"
    );
    let rollup = [PaneState::Idle, PaneState::Blocked, PaneState::Working]
        .into_iter()
        .min();
    assert_eq!(rollup, Some(PaneState::Blocked), "the worst state wins");
}

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
#[tokio::test]
async fn open_attach_place_excludes_mission_squad_from_placement_targets() {
    let mut view = two_pane_view();
    let mut layout = two_squad_layout(1);
    let mid = mission_meta(9, "mux-squad  1/1").id;
    layout.squads.push(mission_meta(9, "mux-squad  1/1"));
    view.set_layout(layout);
    let squads = view.attach_dst_squads();
    view.open_attach_place("job1".into(), Some(mid), squads);
    let picker = view.attach_place.expect("picker opened");
    assert_ne!(
        picker.target(),
        Some(mid),
        "target must not be the virtual mission id"
    );
    assert!(
        !picker.squads.contains(&mid),
        "the mission id must not be offered as a placement choice"
    );
    assert_eq!(
        picker.target(),
        Some(1),
        "falls back to the active real squad"
    );
}

fn meta(id: u64, name: &str, tabs: usize, active_tab: usize) -> SquadMeta {
    SquadMeta {
        id,
        name: name.into(),
        canonical_cwd: format!("/code/{name}"),
        tabs: (1..=tabs)
            .map(|i| TabMeta {
                id: (i - 1) as u64,
                name: i.to_string(),
                named: false,
                panes: Vec::new(),
            })
            .collect(),
        active_tab,
        // One pane per tab is the test fixture's shape (each tab is a leaf).
        panes: tabs,
    }
}

/// A synthetic mission-squad `SquadMeta` shaped like the server mints it:
/// no tabs, no cwd, high-bit id.
fn mission_meta(id: u64, name: &str) -> SquadMeta {
    SquadMeta {
        id: crate::proto::MISSION_SQUAD_BASE | id,
        name: name.into(),
        canonical_cwd: String::new(),
        tabs: Vec::new(),
        active_tab: 0,
        panes: 0,
    }
}

#[test]
fn new_tab_prompt_arms_rename_on_the_materialized_tab() {
    // x-0f9d US1: a bare NewTab arms a create-time name prompt; the layout
    // that adds a higher-id tab opens the x-c150 rename overlay on it. A
    // non-create command never arms it (frictionless: only an explicit
    // create prompts).
    let mut v = two_pane_view();
    v.note_command_sent(&Command::SelectTab(0));
    assert_eq!(v.pending_new_tab, None, "only NewTab arms the prompt");

    // Active squad 1 has tabs 0,1 -> max id 1.
    v.note_command_sent(&Command::NewTab);
    assert_eq!(
        v.pending_new_tab,
        Some(Some(1)),
        "armed with the current max tab id"
    );

    // Race guard: a layout with no higher-id tab leaves the prompt armed
    // and opens no rename (a scrape tick can precede the server's NewTab).
    v.maybe_prompt_new_tab_name();
    assert!(v.rename.is_none(), "no new tab yet -> no prompt");
    assert_eq!(v.pending_new_tab, Some(Some(1)), "still armed");

    // Multi-client guard (codex review): another client creates tab id 5 in
    // the same squad, so it appears in the broadcast layout with an id past
    // the baseline - but it is NOT this client's active tab, so the prompt
    // must NOT open on it.
    v.layout.squads[0].tabs.push(TabMeta {
        id: 5,
        name: "6".into(),
        named: false,
        panes: Vec::new(),
    });
    v.maybe_prompt_new_tab_name();
    assert!(
        v.rename.is_none(),
        "a concurrent client's tab never steals the prompt"
    );
    assert_eq!(
        v.pending_new_tab,
        Some(Some(1)),
        "still armed for our own tab"
    );

    // Our own NewTab lands: tab id 2, and the server switched THIS client's
    // view to it (active_tab). Rename opens on it, once.
    v.layout.squads[0].tabs.push(TabMeta {
        id: 2,
        name: "3".into(),
        named: false,
        panes: Vec::new(),
    });
    v.layout.squads[0].active_tab = v.layout.squads[0].tabs.len() - 1;
    v.maybe_prompt_new_tab_name();
    assert_eq!(
        v.rename.as_ref().map(|(t, _)| t.clone()),
        Some(RenameTarget::Tab(2)),
        "rename armed on our own new tab, not the concurrent id-5 tab"
    );
    assert_eq!(v.pending_new_tab, None, "prompt consumed once");

    // Baseline-None (gemini): an active squad with NO tabs arms with
    // Some(None), and the FIRST tab - even id 0 - triggers the prompt, so
    // the nested Option is not conflating "no tabs" with "max id 0".
    let mut v2 = two_pane_view();
    v2.layout.squads[0].tabs.clear();
    v2.note_command_sent(&Command::NewTab);
    assert_eq!(
        v2.pending_new_tab,
        Some(None),
        "no baseline when squad is empty"
    );
    v2.layout.squads[0].tabs.push(TabMeta {
        id: 0,
        name: "1".into(),
        named: false,
        panes: Vec::new(),
    });
    v2.layout.squads[0].active_tab = 0;
    v2.maybe_prompt_new_tab_name();
    assert_eq!(
        v2.rename.as_ref().map(|(t, _)| t.clone()),
        Some(RenameTarget::Tab(0)),
        "the first tab (id 0) still triggers the prompt"
    );
}

#[test]
fn tab_bar_spans_label_named_tabs_and_collapse_bare_digits() {
    // x-0f9d US2 (supersedes x-c150 Locked 5): an UNNAMED tab renders
    // today's ordinal span byte-identically; a CHOSEN name renders ALONE,
    // no forced ordinal, truncated to TAB_LABEL_W.
    let mut view = two_pane_view();
    let spans = view.tab_bar_spans();
    assert_eq!(
        spans[1].text, " 1 ",
        "unnamed digit collapse: zero regression"
    );
    assert_eq!(spans[2].text, "[2]");
    view.layout.squads[0].tabs[0].name = "x-abcd".into();
    view.layout.squads[0].tabs[0].named = true;
    view.layout.squads[0].tabs[1].name = "a-very-long-worktree-name".into();
    view.layout.squads[0].tabs[1].named = true;
    let spans = view.tab_bar_spans();
    assert_eq!(spans[1].text, " x-abcd ", "chosen name renders alone");
    assert_eq!(
        spans[2].text, "[a-very-long-wo]",
        "name alone truncates to 14"
    );
}

#[test]
fn tab_label_text_collapses_only_the_exact_ordinal() {
    // Collapse (x-0f9d AC7): a name equal to its own ordinal is the bare
    // digit whether chosen or not - byte-identical to the unnamed render.
    assert_eq!(tab_label_text("1", 0, false), "1");
    assert_eq!(
        tab_label_text("1", 0, true),
        "1",
        "chosen name == ordinal collapses"
    );
    assert_eq!(
        tab_label_text("2", 1, true),
        "2",
        "AC7: tab@2 renamed '2' is bare digit"
    );
    // A non-ordinal name: unnamed/derived keeps `{ordinal}:{label}`, a
    // chosen name (US2) renders alone.
    assert_eq!(
        tab_label_text("2", 0, false),
        "1:2",
        "unnamed digit off-position"
    );
    assert_eq!(
        tab_label_text("2", 0, true),
        "2",
        "chosen '2' at ordinal 1 renders alone"
    );
    assert_eq!(
        tab_label_text("debug", 2, false),
        "3:debug",
        "derived keeps ordinal"
    );
    assert_eq!(
        tab_label_text("debug", 2, true),
        "debug",
        "chosen renders alone"
    );
}

// x-df4c US4 helper: an AgentRow in squad 1 with the given tab/badge/exit.
fn tab_agent(tab: Option<TabId>, badge: Option<AgentBadge>, exited: bool) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "worker".into(),
        pane_id: Some(1),
        badge,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
        answerable: None,
        attach_id: None,
        external: false,
        seen: false,
        cwd_base: None,
        tombstone: false,
        subline: None,
        tab,
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
        // (x-d401) A badgeless LIVE row in these fixtures means "an idle
        // worker"; under the absence predicate that must be SAID (an
        // explicit Idle reading), not implied by badge absence - absence
        // now renders Unmeasured.
        pane_activity: if badge.is_none() && !exited {
            Some(ShellActivity::Idle)
        } else {
            None
        },
    }
}

#[test]
fn tab_rollup_folds_worst_live_state_ignoring_exited() {
    // Empty tab -> no rollup (AC2-EDGE).
    assert_eq!(tab_rollup_state(&[], 1, 0), None);
    // Live-idle -> the outline `○`: the tab state machine distinguishes a
    // live-idle tab from a dead one (only "no live panes" omits the glyph).
    assert_eq!(
        tab_rollup_state(&[tab_agent(Some(0), None, false)], 1, 0),
        Some(LatticeState::Idle)
    );
    // All-exited -> no rollup: exited panes are filtered before the fold,
    // leaving no live panes, so the tab renders stateless (AC2-EDGE).
    assert_eq!(
        tab_rollup_state(&[tab_agent(Some(0), Some(AgentBadge::Blocked), true)], 1, 0),
        None
    );
    // Worst-first: a blocked pane beats a working one in the same tab.
    assert_eq!(
        tab_rollup_state(
            &[
                tab_agent(Some(0), Some(AgentBadge::Working), false),
                tab_agent(Some(0), Some(AgentBadge::Blocked), false),
            ],
            1,
            0
        ),
        Some(LatticeState::Blocked)
    );
    // A pane in a DIFFERENT tab never leaks into this tab's rollup.
    assert_eq!(
        tab_rollup_state(
            &[tab_agent(Some(1), Some(AgentBadge::Blocked), false)],
            1,
            0
        ),
        None
    );
}

#[test]
fn tab_strip_rollup_surfaces_hidden_attention_with_accent() {
    // AC2-HP: a background (inactive) tab whose only pane is Blocked shows a
    // leading `▲` in the accent color at the strip, without opening it.
    let mut view = two_pane_view();
    view.layout
        .agents
        .push(tab_agent(Some(0), Some(AgentBadge::Blocked), false));
    let spans = view.tab_bar_spans();
    // spans[0] = squad name, [1] = tab 0 (blocked, inactive), [2] = tab 1 (no live panes).
    assert_eq!(spans[1].text, " ▲ 1 ", "blocked tab: label preceded by ▲");
    assert_eq!(
        spans[1].fg, LATTICE_ACCENT,
        "blocked rollup carries the accent"
    );
    assert_eq!(
        spans[1].flags & cell_flags::BOLD,
        cell_flags::BOLD,
        "blocked rollup carries BOLD"
    );
    // AC2-EDGE: a tab with no live panes shows no rollup glyph and no accent -
    // byte-identical to a pre-feature stateless tab.
    assert_eq!(spans[2].text, "[2]");
    assert_eq!(spans[2].fg, Color::Default);
}

#[test]
fn tab_strip_renders_the_fno_brand_bracketed() {
    // US4/AC3-HP: the mux's home workspace surfaces the bare brand in the
    // tab strip's leading label - render `f[no]`, not `fno`. Other names
    // pass through untouched.
    assert_eq!(brand_label("fno"), "f[no]");
    assert_eq!(brand_label("footnote"), "footnote");
    let mut view = two_pane_view();
    let active = view.layout.active_squad;
    view.layout
        .squads
        .iter_mut()
        .find(|s| s.id == active)
        .expect("active squad")
        .name = "fno".into();
    let spans = view.tab_bar_spans();
    assert_eq!(
        spans[0].text, " f[no] ",
        "the leading brand label is bracketed"
    );
}

#[test]
fn active_blocked_tab_keeps_accent_and_inverse_in_composed_cells() {
    // Domain pitfall + AC2-HP under selection: the ACTIVE (INVERSE) tab whose
    // pane is Blocked must keep the amber fg on every composed cell, so the
    // accent survives the fg/bg swap rather than washing out. tab 1 is the
    // active tab in two_pane_view's squad 1.
    let mut view = two_pane_view();
    view.layout
        .agents
        .push(tab_agent(Some(1), Some(AgentBadge::Blocked), false));
    let frame = view.compose();
    let cols = frame.cols as usize;
    // The tab strip lives on row 0, right of the sideline. Scope the search
    // to the strip columns (>= panel_w): the sideline's own header band now
    // carries `▲N` rollup counts (x-6851 US2), so an unscoped row-0 scan
    // would hit the band glyph first.
    let panel_w = view.panel_w() as usize;
    let glyph_col = (panel_w..cols)
        .find(|&c| frame.cells[c].c == '\u{25b2}')
        .expect("active blocked tab renders ▲ on the strip");
    let glyph = frame.cells[glyph_col];
    assert_eq!(
        glyph.fg, LATTICE_ACCENT,
        "active-blocked ▲: amber under INVERSE"
    );
    assert_eq!(
        glyph.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "active tab keeps INVERSE"
    );
    assert_eq!(
        glyph.flags & cell_flags::BOLD,
        cell_flags::BOLD,
        "blocked rollup keeps BOLD"
    );
    // The label cells inside the same `[...]` span carry the accent too
    // (whole-span amber, deliberate): the cell just after `▲ ` is the label.
    let label_cell = frame.cells[glyph_col + 2];
    assert_eq!(
        label_cell.fg, LATTICE_ACCENT,
        "the blocked active tab's label shares the accent span"
    );
}

fn text_frame(rows: u16, cols: u16, ch: char) -> Frame {
    Frame {
        rows,
        cols,
        cells: vec![
            Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: 0,
            };
            rows as usize * cols as usize
        ],
        cursor_row: 0,
        cursor_col: 0,
        cursor_visible: true,
        scroll_offset: 0,
    }
}

pub(super) fn two_pane_view() -> View {
    // 30x100 terminal, panel visible (100 >= 28+40). Content = 28x72
    // (tab bar + status row). Two panes split H: 35 + divider + 36 cols.
    let mut view = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
            active_squad: 1,
            panes: vec![
                (
                    10,
                    Rect {
                        x: 0,
                        y: 0,
                        rows: 29,
                        cols: 35,
                    },
                ),
                (
                    11,
                    Rect {
                        x: 36,
                        y: 0,
                        rows: 29,
                        cols: 36,
                    },
                ),
            ],
            focus: 11,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    view.frames.insert(10, text_frame(29, 35, 'a'));
    view.frames.insert(11, text_frame(29, 36, 'b'));
    view
}

#[test]
fn client_compose_places_panes_divider_and_chrome() {
    let view = two_pane_view();
    let frame = view.compose();
    assert!(frame.geometry_ok());
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    // Tab strip (x-cd67 US1): scoped to the content columns on row 0, so
    // line 0 carries both the sideline's squad-1 row (cols 0..27) and the
    // strip (cols 28+) - the active squad name + bracketed active tab.
    assert!(lines[0].contains("[2]"), "{:?}", lines[0]);
    // Sideline (x-0090 agents-first): tab rows left the sideline, so an
    // expanded squad with no agents shows only its name row; the next squad
    // follows directly. Active squad carries the `*` glyph (x-2f99). The
    // sideline now owns row 0, so squad 1 leads line 0; a US3 Blank spacer
    // sits on line 1 and squad 2 follows on line 2.
    assert!(lines[0].contains("▾*footnote"), "{:?}", lines[0]);
    assert!(lines[2].contains("▸ notes"), "{:?}", lines[2]);
    // Content row 1 (pane a at content origin): the sideline cols are the
    // blank spacer, then the divider and pane content.
    let row1: Vec<char> = lines[1].chars().collect();
    assert_eq!(row1[27], '│', "panel divider column");
    assert_eq!(row1[28], 'a', "pane 10 starts at content origin");
    assert_eq!(row1[28 + 35], '│', "pane divider between the panes");
    assert_eq!(row1[28 + 36], 'b', "pane 11 after the divider");
    // Cursor: focused pane 11's (0,0) offset by chrome + rect.
    assert_eq!(frame.cursor_row, 1);
    assert_eq!(frame.cursor_col, 28 + 36);
    assert!(frame.cursor_visible);
}

#[test]
fn pane_id_reveal_labels_each_id_inside_its_own_rectangle() {
    let mut view = two_pane_view();
    let t0 = Instant::now();
    view.reveal_pane_ids_at(t0);
    let frame = view.compose_at(t0 + Duration::from_millis(100));
    let cols = view.term.1 as usize;
    for (pid, rect) in &view.layout.panes {
        let label = format!("pane {pid}");
        let start =
            view.panel_w() as usize + rect.x as usize + rect.cols as usize - label.chars().count();
        let row = TAB_BAR_ROWS as usize + rect.y as usize;
        let painted: String = label
            .chars()
            .enumerate()
            .map(|(offset, _)| frame.cells[row * cols + start + offset].c)
            .collect();
        assert_eq!(painted, label, "pane {pid} label is not in its rectangle");
        assert!(start >= view.panel_w() as usize + rect.x as usize);
        assert!(
            start + label.chars().count()
                <= view.panel_w() as usize + rect.x as usize + rect.cols as usize
        );
    }
}

#[test]
fn pane_id_reveal_tracks_tab_layout_and_expires_without_layout_space() {
    let mut view = two_pane_view();
    let t0 = Instant::now();
    view.reveal_pane_ids_at(t0);
    let first = frame_text(&view.compose_at(t0 + Duration::from_millis(100)));
    assert!(first.contains("pane 10"));
    assert!(first.contains("pane 11"));

    view.layout.panes = vec![
        (
            91,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 35,
            },
        ),
        (
            94,
            Rect {
                x: 36,
                y: 0,
                rows: 29,
                cols: 36,
            },
        ),
    ];
    view.frames.insert(91, text_frame(29, 35, 'c'));
    view.frames.insert(94, text_frame(29, 36, 'd'));
    let second = frame_text(&view.compose_at(t0 + Duration::from_millis(200)));
    assert!(second.contains("pane 91"));
    assert!(second.contains("pane 94"));
    assert!(!second.contains("pane 10"));
    assert!(!second.contains("pane 11"));

    let expired =
        frame_text(&view.compose_at(t0 + PANE_ID_REVEAL_WINDOW + Duration::from_millis(1)));
    assert!(!expired.contains("pane 91"));
    assert!(!expired.contains("pane 94"));
}

#[test]
fn pane_id_reveal_skips_only_a_rectangle_too_narrow_for_its_label() {
    let mut view = two_pane_view();
    view.layout.panes[1].1.cols = 5;
    let t0 = Instant::now();
    view.reveal_pane_ids_at(t0);
    let frame = frame_text(&view.compose_at(t0 + Duration::from_millis(1)));
    assert!(frame.contains("pane 10"));
    assert!(!frame.contains("pane 11"));
}

#[test]
fn focus_outline_accents_focused_pane_seams_and_moves_with_focus() {
    // x-5a52 US1 / AC1-HP: the divider cells bounding the focused pane render
    // in the lattice accent at full brightness; a seam between two unfocused
    // panes stays DIM. Moving focus moves the accent in the same compose.
    let view = three_pane_view(); // focus = pane 10
    let frame = view.compose();
    let cols = frame.cols as usize;
    let seam_10_11 = 28 + 23; // divider left of pane 11: borders focused 10
    let seam_11_12 = 28 + 47; // divider between unfocused 11 and 12
    let row = 5;
    let accented = frame.cells[row * cols + seam_10_11];
    assert_eq!(
        accented.c, '│',
        "the accented cell is still a divider glyph"
    );
    assert_eq!(accented.fg, LATTICE_ACCENT, "focused-pane seam is amber");
    assert_eq!(
        accented.flags & cell_flags::DIM,
        0,
        "focus outline is full-bright, never dimmed"
    );
    let dim = frame.cells[row * cols + seam_11_12];
    assert_eq!(
        dim.fg,
        Color::Default,
        "unfocused seam keeps the default fg"
    );
    assert_eq!(
        dim.flags & cell_flags::DIM,
        cell_flags::DIM,
        "unfocused seam stays the DIM chrome"
    );

    // Move focus to pane 12: the accent follows to its seam in the same
    // frame, and the old seam reverts to DIM (AC1-HP "in the same frame").
    let mut moved = three_pane_view();
    moved.layout.focus = 12;
    let frame = moved.compose();
    assert_eq!(
        frame.cells[row * cols + seam_11_12].fg,
        LATTICE_ACCENT,
        "accent follows focus to pane 12"
    );
    assert_eq!(
        frame.cells[row * cols + seam_10_11].flags & cell_flags::DIM,
        cell_flags::DIM,
        "the previously-focused seam reverts to DIM"
    );
}

#[test]
fn single_pane_tab_paints_no_focus_outline() {
    // x-5a52 AC5-EDGE: one pane fills the content area, so there are no
    // interior seams and nothing paints the accent - the sideline markers
    // alone carry the "you are here" state.
    let mut view = three_pane_view();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(
            10,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 10,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    let frame = view.compose();
    // The sideline still marks the active squad, so scope the check to the
    // content area (col >= panel_w) where the outline would live.
    let cols = frame.cols as usize;
    let panel_w = view.panel_w() as usize;
    let outline_in_content = (0..frame.rows as usize)
        .any(|r| (panel_w..cols).any(|c| frame.cells[r * cols + c].fg == LATTICE_ACCENT));
    assert!(
        !outline_in_content,
        "a single-pane tab paints no accent outline in the content area"
    );
}

// A 2x2 grid over two_pane_view's geometry: A|B on top, C|D below, meeting
// at a `┼` junction. focus = A (pane 10).
fn four_pane_view() -> View {
    let mut view = two_pane_view();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![
            (
                10,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 14,
                    cols: 35,
                },
            ),
            (
                11,
                Rect {
                    x: 36,
                    y: 0,
                    rows: 14,
                    cols: 36,
                },
            ),
            (
                12,
                Rect {
                    x: 0,
                    y: 15,
                    rows: 14,
                    cols: 35,
                },
            ),
            (
                13,
                Rect {
                    x: 36,
                    y: 15,
                    rows: 14,
                    cols: 36,
                },
            ),
        ],
        focus: 10,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view
}

#[test]
fn focus_outline_wraps_both_seams_of_a_2x2_pane() {
    // x-5a52 AC1-HP (the 2x2 case the horizontal-split test missed): the
    // focused top-left pane borders on TWO interior sides, so the outline
    // must accent both its right `│` seam and its bottom `─` seam - and a
    // seam bordering only the unfocused panes stays dim.
    let frame = four_pane_view().compose(); // focus = pane 10 (top-left)
    let cols = frame.cols as usize;
    // A's right seam: vertical divider at content col 35 -> outer col 63,
    // within A's rows (outer 1..14). Sample outer row 5.
    let right_seam = frame.cells[5 * cols + (28 + 35)];
    assert_eq!(right_seam.c, '│', "A's right border is a vertical divider");
    assert_eq!(right_seam.fg, LATTICE_ACCENT, "A's right seam is accented");
    // A's bottom seam: horizontal divider at content row 14 -> outer row 15,
    // within A's cols (outer 28..62). Sample outer col 40.
    let bottom_seam = frame.cells[15 * cols + (28 + 10)];
    assert_eq!(
        bottom_seam.c, '─',
        "A's bottom border is a horizontal divider"
    );
    assert_eq!(
        bottom_seam.fg, LATTICE_ACCENT,
        "A's bottom seam is accented"
    );
    // The C/D vertical seam (below A, outer row 20 col 63) borders only the
    // unfocused panes and stays dim.
    let cd_seam = frame.cells[20 * cols + (28 + 35)];
    assert_eq!(
        cd_seam.flags & cell_flags::DIM,
        cell_flags::DIM,
        "a seam not bordering the focused pane stays dim"
    );
}

// An agent row hosting a given pane, under squad 1.
pub(super) fn focus_agent(pane: u64) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "worker".into(),
        pane_id: Some(pane),
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    }
}

#[test]
fn sideline_lane_color_and_deviation_token_render_on_the_row() {
    // x-1b35 AC3: the lane renders as zero-width color on the agent row
    // (here through the built-in codex table entry), the model-deviation
    // token appends ` glm` on a claude row OFF its default lane, and the
    // retired `@<account>` text prefix is gone from the composition.
    let mut view = two_pane_view();
    let mut codex_row = focus_agent(21);
    codex_row.harness = Some("codex".into());
    let mut glm_row = focus_agent(22);
    glm_row.harness = Some("claude".into());
    glm_row.model = Some("glm-5.3-flash[1m]".into());
    glm_row.account = Some("makers".into());
    view.layout.agents.push(codex_row);
    view.layout.agents.push(glm_row);
    let frame = view.compose();
    let cols = frame.cols as usize;
    let panel_w = view.panel_w() as usize;
    let line = |row: usize| -> (String, Color) {
        let start = row * cols;
        let end = start + panel_w.min(cols);
        let text: String = frame.cells[start..end].iter().map(|c| c.c).collect();
        // A name cell, not the row lead: the focused row's band and the
        // selector own the first columns.
        let fg = frame.cells[start + 3].fg;
        (text, fg)
    };
    // Row 1: the codex row - lane color from the built-in table (blue),
    // no account prefix.
    let (text, fg) = line(1);
    assert_eq!(fg, Color::Indexed(4), "builtin codex lane color");
    assert!(
        !text.contains('@'),
        "the @account prefix is retired: `{text}`"
    );
    // Row 2: the claude/glm row - the deviation token is the textual
    // channel; claude itself carries no builtin color, so fg stays
    // default and the token does the naming.
    let (text, fg) = line(2);
    assert!(
        text.contains(" glm"),
        "the deviation token renders: `{text}`"
    );
    assert!(!text.contains("@makers"), "no @account prefix: `{text}`");
    assert_eq!(fg, Color::Default, "claude carries no builtin lane color");
}

#[test]
fn sideline_marks_active_squad_and_focused_agent_row() {
    // x-4374 / AC2-HP: the active squad header accents its caret, and the
    // agent row whose pane holds focus wears the full-width INVERSE band
    // (replacing the near-invisible one-cell gutter x-5a52 painted). Both
    // stand regardless of the selector (parked elsewhere) or hover.
    let mut view = two_pane_view(); // active_squad = 1, focus = pane 11
    view.layout.agents.push(focus_agent(11));
    view.selector = Some(3); // squad 2's header, not row 0 or 1
    view.hover_row = None;
    let frame = view.compose();
    let cols = frame.cols as usize;
    let panel_w = view.panel_w() as usize;

    // Display row 0 -> outer row 0: the active squad header caret is amber.
    let caret = frame.cells[0];
    assert_eq!(caret.c, '▾', "active expanded squad shows the caret");
    assert_eq!(caret.fg, LATTICE_ACCENT, "active squad caret is accented");

    // Display row 1 -> outer row 1: the focused agent row is a full-width
    // INVERSE band, and the `▎` gutter glyph is gone.
    let lead = frame.cells[cols]; // outer row 1, col 0
    assert_ne!(
        lead.c, '▎',
        "the ▎ gutter is retired; the band is the signal"
    );
    assert_eq!(
        lead.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the focused row carries the standing INVERSE band"
    );
    // The band fills the panel width (a right-edge text cell is still INVERSE).
    assert_eq!(
        frame.cells[cols + panel_w - 2].flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the focus band fills the panel width"
    );
}

#[test]
fn active_marker_composes_with_selection_inverse() {
    // x-4374 / AC3-UI: when the selector sits on the focused row, the XOR
    // de-inverts its standing band so the selection reads under the cursor -
    // the same grammar the old header bands used - instead of band and cursor
    // masking each other.
    let mut view = two_pane_view();
    view.layout.agents.push(focus_agent(11));
    view.selector = Some(1); // the focused agent row
    let frame = view.compose();
    let cols = frame.cols as usize;
    let lead = frame.cells[cols]; // outer row 1, col 0
    assert_eq!(
        lead.flags & cell_flags::INVERSE,
        0,
        "the selector de-inverts the focused row's band so the cursor reads"
    );
}

#[test]
fn xf331_focus_band_and_selector_are_distinct_treatments() {
    // x-f331 US2/AC1-UI: the focus band wears the ACCENT colour while a
    // selector parked on a DIFFERENT row is a plain-INVERSE bar in the default
    // colour - three-distinguishable, and the distinction is colour (survives
    // weak-BOLD themes), not weight.
    let mut view = two_pane_view();
    view.layout.agents.push(focus_agent(11)); // owns focused pane 11 -> row 1
    view.selector = Some(3); // notes squad header, a different actionable row
    view.hover_row = None;
    let frame = view.compose();
    let cols = frame.cols as usize;

    let focus_cell = frame.cells[cols]; // display row 1: the focus band
    assert_eq!(
        focus_cell.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the focus row still wears a band"
    );
    assert_eq!(
        focus_cell.fg, LATTICE_ACCENT,
        "the focus band wears the accent colour"
    );

    let sel_cell = frame.cells[3 * cols]; // display row 3: the selector bar
    assert_eq!(
        sel_cell.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the selector row is an inverse bar"
    );
    assert_ne!(
        sel_cell.fg, LATTICE_ACCENT,
        "the selector bar is NOT the focus accent - the two read as distinct"
    );
}

#[test]
fn xf331_exited_focus_row_is_dim_accent_not_a_bright_band() {
    // x-f331 US2/AC1-UI: a focus band on an EXITED row drops the bright
    // INVERSE band for a DIM accent, so a dead "you are here" reads as dead
    // (the screenshot case that was indistinguishable from a selector).
    let mut view = two_pane_view();
    let mut agent = focus_agent(11);
    agent.exited = true;
    view.layout.agents.push(agent);
    // (x-c5ee) The squad's only agent is exited -> it would default LiveOnly
    // and hide the row; force Expanded so the exited focus row still paints.
    view.set_squad_view(1, SectionView::Expanded);
    let frame = view.compose();
    let cols = frame.cols as usize;
    let cell = frame.cells[cols]; // display row 1: the exited focus row
    assert_eq!(
        cell.flags & cell_flags::INVERSE,
        0,
        "an exited focus row drops the bright band"
    );
    assert_eq!(
        cell.flags & cell_flags::DIM,
        cell_flags::DIM,
        "it is dimmed - legibly dead"
    );
    assert_eq!(cell.fg, LATTICE_ACCENT, "still the accent colour");
}

#[test]
fn xf331_confirm_anchors_at_the_target_row_not_the_bottom() {
    // x-f331 US4/AC2-UI: the confirm prompt resolves its target by identity
    // (the squad id) and paints AT that row's outer position, never the
    // terminal's far bottom row.
    let mut view = two_pane_view();
    view.selector = Some(2); // the notes squad header (actionable)
    view.open_confirm(ConfirmAction {
        action: ConfirmKind::RemoveSquad {
            squad: 2,
            panes: 1,
            last: false,
        },
        label: "notes".into(),
    });
    assert_eq!(view.selector, None, "open_confirm clears the selector");

    let rows = view.term.0 as usize;
    let anchor = view.confirm_anchor_row(rows, view.confirm.as_ref().unwrap());
    assert_eq!(
        anchor, 2,
        "the prompt anchors at the target's outer row (squad 2 -> display row 2)"
    );
    assert_ne!(anchor, rows - 1, "not the far bottom row");

    let frame = view.compose();
    let cols = frame.cols as usize;
    let screen: String = (0..frame.rows as usize)
        .map(|r| {
            (0..cols)
                .map(|c| frame.cells[r * cols + c].c)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let layout = view.confirm_overlay_layout(rows, view.confirm.as_ref().unwrap());
    assert!(
        layout.origin.0 == anchor,
        "the framed confirm starts at the target row: {:?}",
        layout.origin
    );
    assert!(
        screen.contains("close workspace"),
        "the confirm prompt paints at the target row: {screen}"
    );
}

#[test]
fn close_tab_confirm_is_centered_in_the_content_viewport() {
    let view = two_pane_view();
    let action = ConfirmAction {
        action: ConfirmKind::CloseTab { tab: 1 },
        label: "2".into(),
    };
    let layout = view.confirm_overlay_layout(view.term.0 as usize, &action);
    let (content_origin, content_dims) = view.overlay_viewport();
    let centered = family_b_origin(
        OverlayAnchor::Center,
        layout.framed.width,
        layout.framed.lines.len(),
        content_origin,
        content_dims,
    );
    assert_eq!(
        layout.origin, centered,
        "Close tab must use the shared centered modal origin"
    );
    assert_ne!(
        layout.origin.0,
        view.term.0 as usize - 1,
        "Close tab must not fall back to the bottom row"
    );
}

#[test]
fn xf331_confirm_falls_back_to_bottom_when_target_vanishes() {
    // x-f331 AC1-FR (codex P2): the anchor is resolved by identity every
    // paint, so a target whose row is no longer in the catalog dismisses to
    // the bottom row - it never paints beside an unrelated row that drifted
    // under a stale numeric index.
    let view = two_pane_view();
    let rows = view.term.0 as usize;
    let gone = ConfirmAction {
        action: ConfirmKind::RemoveSquad {
            squad: 999, // no such squad in the catalog
            panes: 1,
            last: false,
        },
        label: "gone".into(),
    };
    assert_eq!(
        view.confirm_anchor_row(rows, &gone),
        rows - 1,
        "a vanished target dismisses to the bottom row, never a wrong row"
    );
}

#[test]
fn xf331_wheel_scrolls_not_walks_a_hover_armed_selector() {
    // x-f331 (codex P2): a hover-armed selector is a pointer-follow, so a
    // wheel event scrolls the list and disarms - it must NOT walk the selector
    // away from the pointer (which would strand hover_row and selector on two
    // different rows and misdirect the next x/r/space).
    let mut view = two_pane_view();
    for p in 100..140u64 {
        view.layout.agents.push(AgentRow {
            portal: None,
            harness: None,
            model: None,
            route: None,
            name: format!("w{p}"),
            // (x-c5ee) Working, not idle, so the top-K cap never folds them:
            // this test needs a long, fully-rendered scrollable list.
            badge: Some(AgentBadge::Working),
            ..focus_agent(p)
        })
    }
    assert!(
        view.display_rows().len() > view.sideline_visible_rows(),
        "sanity: the sideline exceeds the viewport so scroll is live"
    );
    view.selector = Some(1);
    view.sel_hover_armed = true;
    view.hover_row = Some(1);
    let before = view.sideline_offset;
    view.scroll_sideline(true);
    assert!(!view.sel_hover_armed, "the wheel disarms the hover-arm");
    assert_eq!(
        view.selector, None,
        "the wheel does not walk a hover-armed selector"
    );
    assert_eq!(
        view.sideline_offset,
        before + 1,
        "the wheel scrolls the list instead of moving the cursor"
    );
}

#[test]
fn overlay_viewport_matches_content_origin_and_dims() {
    // x-e9c3: overlay_viewport() is the single source of centering
    // geometry every popover shares - it must track content_dims()/
    // panel_w() exactly, not a separately hand-computed value.
    let view = two_pane_view();
    let (origin, dims) = view.overlay_viewport();
    let (content_rows, content_cols) = view.content_dims();
    assert_eq!(origin, (TAB_BAR_ROWS as usize, view.panel_w() as usize));
    assert_eq!(dims, (content_rows as usize, content_cols as usize));
}

#[test]
fn draw_lines_overlay_centers_within_viewport() {
    // x-e9c3: popovers used to anchor at the outer terminal's top-left
    // corner (origin_r = TAB_BAR_ROWS + 1, col 2), overlapping the
    // sideline. They now center within the content viewport passed in.
    let (rows, cols) = (20usize, 40usize);
    let mut cells = vec![Cell::default(); rows * cols];
    let content_origin = (2usize, 4usize);
    let content_dims = (10usize, 30usize); // roomy viewport, right of a sideline
    let lines = ["ab", "cd"];
    let chrome = chrome::Chrome::new("t", Anchor::Center);
    draw_lines_overlay(
        &mut cells,
        rows,
        cols,
        content_origin,
        content_dims,
        &chrome,
        &lines,
        &Theme::default_theme(),
        None,
    );

    // The framed block (top + 2 body + bottom = 4 rows) centers in the
    // 10-row viewport: top margin (10-4)/2 = 3, border starts at row 2+3 = 5.
    let origin_r = 2 + (10 - 4) / 2;
    // 'a' sits one row + one col inside the frame; locate it by scan so the
    // test does not hardcode the chrome-widened column.
    let a_col = (0..cols)
        .find(|&c| cells[(origin_r + 1) * cols + c].c == 'a')
        .expect("body row 'a' was drawn");
    assert_eq!(cells[(origin_r + 1) * cols + a_col].c, 'a');
    assert_eq!(
        cells[(origin_r + 1) * cols + a_col].flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "body cells stay inverse under terminal"
    );
    assert_eq!(cells[(origin_r + 2) * cols + a_col].c, 'c');
    // The top border corner sits one row up and one col left of the body.
    assert_eq!(cells[origin_r * cols + (a_col - 1)].c, '┌');
    // Nothing painted at the old hardcoded top-left corner.
    assert_eq!(cells[(TAB_BAR_ROWS as usize + 1) * cols + 2].c, ' ');
}

#[test]
fn draw_lines_overlay_windows_body_to_viewport_minus_chrome() {
    // x-f75e: chrome's border/footer borrow rows from the viewport, so a body
    // that filled it would lose its tail off-screen while those rows stayed
    // selectable. The overlay reserves the chrome overhead and top-pins a
    // window to the rows that remain, marking the cut with a scrollbar.
    let (rows, cols) = (8usize, 40usize);
    let mut cells = vec![Cell::default(); rows * cols];
    // A 4-row viewport vs a Full chrome (overhead 2) leaves a 2-row body
    // budget; four body lines must window to the first two.
    let chrome = chrome::Chrome::new("t", Anchor::Center);
    let lines = ["row0", "row1", "row2", "row3"];
    draw_lines_overlay(
        &mut cells,
        rows,
        cols,
        (2usize, 4usize),
        (4usize, 30usize),
        &chrome,
        &lines,
        &Theme::default_theme(),
        None,
    );
    let painted = |c: char| cells.iter().any(|cell| cell.c == c);
    // Positive markers: the windowed-in rows are painted.
    assert!(painted('0'), "first windowed body row must paint");
    assert!(painted('1'), "second windowed body row must paint");
    // The clipped tail rows never reach the buffer.
    assert!(!painted('2'), "third body row must be windowed out");
    assert!(!painted('3'), "fourth body row must be windowed out");
    // Positive control: a scrollbar glyph marks the cut.
    assert!(
        painted('█') || painted('░'),
        "an overflowing body must show a scrollbar"
    );
}

#[test]
fn draw_lines_overlay_zero_body_budget_paints_no_body() {
    // x-f75e: a viewport exactly the chrome overhead leaves a zero body
    // budget. The overlay must window to zero body rows rather than paint
    // the whole body plus its border past the content viewport. A Full
    // chrome is two rows of overhead; a two-row viewport fits the chrome
    // and no body line.
    let (rows, cols) = (6usize, 40usize);
    let mut cells = vec![Cell::default(); rows * cols];
    let chrome = chrome::Chrome::new("t", Anchor::Center);
    let lines = ["111", "222", "333"];
    draw_lines_overlay(
        &mut cells,
        rows,
        cols,
        (2usize, 4usize),
        (2usize, 30usize),
        &chrome,
        &lines,
        &Theme::default_theme(),
        None,
    );
    let painted = |c: char| cells.iter().any(|cell| cell.c == c);
    // No body content reaches the buffer: the body budget was zero.
    assert!(
        !painted('1'),
        "no body row should paint at zero body budget"
    );
    assert!(
        !painted('2'),
        "no body row should paint at zero body budget"
    );
    assert!(
        !painted('3'),
        "no body row should paint at zero body budget"
    );
    // Positive control: the chrome border still paints within the viewport.
    assert!(painted('┌'), "the chrome border must still paint");
}

#[test]
fn client_hit_test_maps_pane_and_swallows_chrome() {
    // US3 hit-test: content cells resolve to (pane, local row, local col);
    // chrome cells (tab bar, sideline) and dividers resolve to None so the
    // caller swallows them (AC3-UI: nothing forwards to a pane).
    let view = two_pane_view();
    // Inside pane 10 (content origin at outer (1, 28)).
    assert_eq!(view.hit_test(5, 30), Some((10, 4, 2)));
    // Inside pane 11 (content col 36 -> outer col 64), its top-left cell.
    // Pins press-cell == anchor-cell for a pane with a NONZERO x origin: the
    // first visible column of an offset pane maps to pane-col 0, so a drag
    // anchored there selects from that glyph, not N chars late.
    assert_eq!(view.hit_test(3, 64), Some((11, 2, 0)));
    // Tab bar row is chrome.
    assert_eq!(view.hit_test(0, 40), None);
    // Sideline column (< panel_w 28) is chrome.
    assert_eq!(view.hit_test(5, 10), None);
    // The divider column between the panes covers no pane.
    assert_eq!(view.hit_test(5, 28 + 35), None);
}

// A two-pane VERTICAL stack over two_pane_view's geometry: 20 above 21,
// divider on content row 14 (outer row 15).
fn stacked_view() -> View {
    let mut view = two_pane_view();
    let rect = |y, rows| Rect {
        x: 0,
        y,
        rows,
        cols: 72,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(20, rect(0, 14)), (21, rect(15, 14))],
        focus: 20,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view
}

#[test]
fn seam_at_addresses_a_divider_by_its_flanking_panes() {
    // US5: the client never sees the tree, so a seam is addressed by the
    // panes flanking it. Content origin is outer (1, 28); three_pane_view
    // tiles panes 10/11/12 at content x 0/24/48, each 23 wide, so the
    // dividers sit on content col 23 and 47 (outer 51 and 75).
    let view = three_pane_view();
    assert_eq!(
        view.seam_at(5, 51),
        Some(Seam {
            a: 10,
            b: 11,
            axis: Axis::Horizontal
        }),
        "vertical divider line addresses the panes left and right of it"
    );
    assert_eq!(
        view.seam_at(5, 75),
        Some(Seam {
            a: 11,
            b: 12,
            axis: Axis::Horizontal
        }),
        "the second divider addresses its own pair, not the first's"
    );
    // A covered cell is a pane hit, never a seam - hit_test still owns it.
    assert_eq!(view.seam_at(5, 30), None);
    // Chrome: tab bar row and sideline columns.
    assert_eq!(view.seam_at(0, 51), None);
    assert_eq!(view.seam_at(5, 10), None);
}

#[test]
fn seam_at_addresses_a_stacked_divider_on_the_vertical_axis() {
    let view = stacked_view();
    assert_eq!(
        view.seam_at(15, 40),
        Some(Seam {
            a: 20,
            b: 21,
            axis: Axis::Vertical
        }),
        "horizontal divider line addresses the panes above and below it"
    );
    assert_eq!(view.seam_at(5, 40), None, "inside the top pane");
}

#[test]
fn seam_pos_reads_the_divider_cell_not_a_ratio() {
    // The client reports WHERE the divider goes and leaves the ratio to the
    // server, which is the only side that can see the branch child's true
    // extent. Content origin is outer (1, 28).
    let view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    // Pane 10 spans content cols 0..22, so its divider sits at 23.
    assert_eq!(view.seam_pos(seam), Some(23));
    // Dragging to outer col 58 asks for the divider at content col 30.
    assert_eq!(view.seam_pos_at(seam, 5, 58), Some(30));
    // Off the content area on the sideline side resolves to nothing.
    assert_eq!(view.seam_pos_at(seam, 5, 10), None);
}

#[test]
fn seam_drag_emits_one_command_per_crossing_not_per_report() {
    // US1: a drag reports far more cells than the seam has positions. Only
    // a real move goes on the wire; the rest are silent.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    let t0 = Instant::now();
    view.begin_seam_drag(seam, t0);
    assert_eq!(
        view.seam_drag_to(5, 58, t0),
        Some(Command::ResizeSeam {
            a: 10,
            b: 11,
            pos: 30
        }),
        "crossing to content col 30 moves the seam"
    );
    assert_eq!(
        view.seam_drag_to(6, 58, t0),
        None,
        "same column, different row: the seam did not move"
    );
    assert_eq!(
        view.seam_drag_to(5, 59, t0),
        Some(Command::ResizeSeam {
            a: 10,
            b: 11,
            pos: 31
        }),
        "the next column is a new position"
    );
}

#[test]
fn hovered_seam_renders_a_distinct_accent_in_compose() {
    // AC3-UI: the accent is the whole draggability affordance (a terminal
    // cursor cannot portably change shape), so it must be visibly distinct
    // from idle chrome and assertable in the compose output.
    let mut view = three_pane_view();
    let cell_at = |view: &View, row: usize, col: usize| {
        let f = view.compose();
        f.cells[row * f.cols as usize + col]
    };
    // Two different idle states exist. The seam at col 75 (between the
    // unfocused 11 and 12) is plain dim chrome; the one at col 51 borders
    // the focused pane 10, so it already wears x-5a52's standing outline.
    // The hover accent has to be distinct from BOTH.
    let idle_chrome = cell_at(&view, 5, 75);
    let focus_outline = cell_at(&view, 5, 51);
    assert_eq!(idle_chrome.c, '│', "the divider glyph itself is unchanged");
    assert_eq!(idle_chrome.flags, cell_flags::DIM);
    assert_eq!(
        focus_outline.flags, 0,
        "the focus outline is undimmed accent"
    );

    view.on_hover(5, 75, Instant::now());
    let lit = cell_at(&view, 5, 75);
    assert_eq!(
        lit.c, '│',
        "hover accents the divider, it does not redraw it"
    );
    assert_eq!(lit.flags, cell_flags::BOLD);
    assert_eq!(lit.fg, LATTICE_ACCENT);
    assert!(
        (lit.flags, lit.fg) != (idle_chrome.flags, idle_chrome.fg),
        "distinct from idle chrome"
    );
    assert!(
        (lit.flags, lit.fg) != (focus_outline.flags, focus_outline.fg),
        "distinct from the focused pane's standing outline, so a hovered \
             seam beside the focused pane still reads as grabbable"
    );
    // Hovering one seam does not light another.
    assert_eq!(
        cell_at(&view, 5, 51).flags,
        0,
        "still just the focus outline"
    );

    // Leaving the band clears it.
    view.on_hover(5, 40, Instant::now());
    assert_eq!(cell_at(&view, 5, 75).flags, cell_flags::DIM);
}

#[test]
fn drag_keeps_the_accent_on_the_seam_it_grabbed() {
    // The pointer routinely runs ahead of the divider during a drag; the
    // thing being moved must stay the thing lit.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    view.begin_seam_drag(seam, Instant::now());
    view.on_hover(5, 60, Instant::now()); // pointer now over a pane
    let f = view.compose();
    assert_eq!(
        f.cells[5 * f.cols as usize + 51].flags,
        cell_flags::BOLD,
        "the grabbed seam stays accented while the pointer is off it"
    );
}

#[test]
fn layout_change_ends_a_drag_whose_seam_is_gone() {
    // AC4-ERR (client half): a concurrent close retires the pair, so the
    // drag ends visibly rather than resizing something else.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    view.begin_seam_drag(seam, Instant::now());
    view.on_hover(5, 51, Instant::now());
    assert!(view.seam_drag.is_some() && view.hover_seam.is_some());

    // Pane 11 closes elsewhere; 10 and 12 now tile the area.
    let rect = |x, cols| Rect {
        x,
        y: 0,
        rows: 29,
        cols,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(10, rect(0, 35)), (12, rect(36, 36))],
        focus: 10,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert!(
        view.seam_drag.is_none(),
        "the drag ended, it did not re-target"
    );
    assert!(
        view.hover_seam.is_none(),
        "a dead seam is not lit as draggable"
    );
    assert!(
        view.notice.is_some(),
        "the drag ending is reported, never silent"
    );
}

#[test]
fn drag_ends_when_a_split_lands_between_its_panes() {
    // Both ids survive a same-axis split between them, so a membership
    // check would call this seam live. It is not: the panes no longer
    // flank one divider, the server would refuse every command, and the
    // divider would look dead until release with no notice ever shown.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    view.begin_seam_drag(seam, Instant::now());
    view.on_hover(5, 51, Instant::now());

    let rect = |x, cols| Rect {
        x,
        y: 0,
        rows: 29,
        cols,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        // New pane 13 lands between 10 and 11; every original id lives on.
        panes: vec![
            (10, rect(0, 16)),
            (13, rect(17, 16)),
            (11, rect(34, 16)),
            (12, rect(51, 21)),
        ],
        focus: 13,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert!(
        view.seam_drag.is_none(),
        "the pair is no longer adjacent, so the drag ends"
    );
    assert!(view.hover_seam.is_none());
    assert!(
        view.notice.is_some(),
        "and says so, rather than going quiet"
    );
}

#[test]
fn drag_survives_a_layout_push_that_keeps_its_pair() {
    // The common case: every applied resize broadcasts a layout, and the
    // drag must ride through its own updates.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    view.begin_seam_drag(seam, Instant::now());
    let rect = |x, cols| Rect {
        x,
        y: 0,
        rows: 29,
        cols,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        // 10 grew, 11 shrank: the same pair, a moved seam.
        panes: vec![(10, rect(0, 30)), (11, rect(31, 16)), (12, rect(48, 23))],
        focus: 10,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert!(view.seam_drag.is_some(), "the drag survives its own resize");
    assert!(view.notice.is_none(), "a normal resize is not an error");
}

#[test]
fn esc_reverts_a_seam_drag_to_where_it_started() {
    // AC6-FR: the revert is an explicit final command, not a local
    // rollback - the server owns the layout.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    let t0 = Instant::now();
    view.begin_seam_drag(seam, t0);
    view.seam_drag_to(5, 58, t0);
    view.seam_drag_to(5, 62, t0);
    assert_eq!(
        view.revert_seam_drag(),
        Some(Command::ResizeSeam {
            a: 10,
            b: 11,
            pos: 23
        }),
        "reverts to where the divider sat when the drag began"
    );
    assert!(view.seam_drag.is_none(), "the revert also ends the drag");
}

#[test]
fn a_drag_that_never_moved_reverts_to_nothing() {
    // A press-and-release on a divider is a click, not a resize: it sent no
    // command, so cancelling it must not send one either.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");
    view.begin_seam_drag(seam, Instant::now());
    assert_eq!(view.revert_seam_drag(), None);
    assert!(view.seam_drag.is_none());
}

#[test]
fn sideline_border_accents_on_hover_and_during_drag() {
    // AC3-UI (the border half): the divider column reads BOLD accent when
    // hovered or dragged, distinct from idle DIM chrome. x-d807 shipped the
    // drag with this render missing, so the border was a draggable-but-
    // invisible 1-cell target; this is the regression guard.
    // One frame per state; assert against that snapshot.
    let mut view = two_pane_view();
    let border = view.panel_w() as usize - 1;
    let cell = |f: &Frame, row: usize, col: usize| f.cells[row * f.cols as usize + col];

    let idle_f = view.compose();
    let idle = cell(&idle_f, 5, border);
    assert_eq!(idle.c, '│', "the divider glyph itself is unchanged");
    assert_eq!(idle.flags, cell_flags::DIM, "idle border is dim chrome");

    view.on_hover(5, border as u16, Instant::now());
    assert!(view.hover_sideline_border, "hover state set");
    let lit_f = view.compose();
    let lit = cell(&lit_f, 5, border);
    assert_eq!(
        lit.c, '│',
        "hover accents the border, it does not redraw it"
    );
    assert_eq!(lit.flags, cell_flags::BOLD);
    assert_eq!(lit.fg, LATTICE_ACCENT);
    assert!(
        (lit.flags, lit.fg) != (idle.flags, idle.fg),
        "hovered border is visibly distinct from idle"
    );

    view.on_hover(5, border as u16 - 1, Instant::now());
    let off_f = view.compose();
    assert_eq!(
        cell(&off_f, 5, border).flags,
        cell_flags::DIM,
        "border returns to idle chrome off the column"
    );

    // A drag in flight keeps it accented even while the pointer runs ahead.
    view.sideline_drag = Some(SidelineDrag {
        start_width: view.sideline_width,
        last_at: Instant::now(),
    });
    let drag_f = view.compose();
    assert_eq!(
        cell(&drag_f, 5, border).flags,
        cell_flags::BOLD,
        "the border stays lit for the whole drag"
    );
}

#[test]
fn drag_release_off_a_target_clears_its_stale_accent() {
    // A drag ends off the thing it grabbed. Drag events never refresh hover
    // state, so without the release recompute the accent would linger until
    // the next bare Move. The release arms call this recompute; here it is
    // directly, proving stale-true fields clear at the release position.
    let mut view = three_pane_view();
    // Pretend a seam drag and a sideline hover both left stale accents on.
    view.hover_seam = view.seam_at(5, 51);
    view.hover_sideline_border = true;
    assert!(view.hover_seam.is_some());

    // Release lands inside a pane (10) - on no seam, no border, no grip.
    view.refresh_hover_affordances(5, 40);
    assert!(view.hover_seam.is_none(), "stale seam accent cleared");
    assert!(!view.hover_sideline_border, "stale border accent cleared");
    assert!(view.hover_grip.is_none());

    // And a release that lands back on a seam re-lights exactly that one.
    view.refresh_hover_affordances(5, 75);
    assert_eq!(
        view.hover_seam.map(|s| (s.a, s.b)),
        Some((11, 12)),
        "release on a seam accents that seam"
    );
}

#[test]
fn ending_a_drag_off_its_target_clears_the_stale_accent() {
    // The non-left cancellation arm (a wheel, another button) ends a drag
    // the same way a release does - both route through end_{seam,sideline}
    // _drag. A Drag event never refreshes hover, so a gesture that ends off
    // its target must recompute or the accent lingers until the next Move.
    // The release arms already had this; the cancel arms did not (codex peer
    // review), so both paths now share one method that recomputes.
    let mut view = three_pane_view();
    let seam = view.seam_at(5, 51).expect("seam between 10 and 11");

    view.begin_seam_drag(seam, Instant::now());
    view.hover_seam = Some(seam); // the drag left the accent lit
    view.end_seam_drag(5, 40); // ended inside pane 10 - off the seam
    assert!(view.seam_drag.is_none(), "the seam drag ended");
    assert!(
        view.hover_seam.is_none(),
        "stale seam accent cleared on cancel"
    );

    view.sideline_drag = Some(SidelineDrag {
        start_width: view.sideline_width,
        last_at: Instant::now(),
    });
    view.hover_sideline_border = true;
    view.end_sideline_drag(5, 40); // off the border column
    assert!(view.sideline_drag.is_none(), "the sideline drag ended");
    assert!(
        !view.hover_sideline_border,
        "stale border accent cleared on cancel"
    );
}

/// Put a live sideline drag on `view` so `drag_sideline_to` refreshes its
/// deadline (and Esc/timeout have something to end), then return the grab
/// instant. Mirrors how the press arm arms the drag.
fn arm_sideline_drag(view: &mut View, t0: Instant) {
    view.sideline_drag = Some(SidelineDrag {
        start_width: view.sideline_width,
        last_at: t0,
    });
}

/// (x-2e86) Set the density AND its canonical width, exactly as a preset
/// press does. Since x-2e86 the width is independent of the density, so a
/// test that only wants "the user is in mode X" must set both - a bare
/// `view.density = X` would leave the previous width and no longer widen the
/// rail. The transient `panel_w` clamp then reproduces the pre-x-2e86
/// per-density width on any terminal.
fn set_density(view: &mut View, d: Density) {
    view.density = d;
    view.sideline_width = canonical_width(d);
}

#[test]
fn sideline_border_drag_sets_a_free_continuous_width() {
    // AC1-HP: the drag sets ANY width, not one of three snapped states.
    let mut view = two_pane_view();
    view.term = (30, 120); // max = min(72, 80) = 72
    view.density = Density::Regular;
    arm_sideline_drag(&mut view, Instant::now());

    // Drag the border to column 44 -> 45 columns, exactly where the pointer
    // is, not the nearest canonical width.
    assert!(view.drag_sideline_to(44, Instant::now()));
    assert_eq!(view.sideline_width, 45);
    assert_eq!(view.panel_w(), 45);
    assert_eq!(view.density, Density::Regular, "a free drag keeps the mode");

    // One column further is a distinct width (no snapping quantum).
    assert!(view.drag_sideline_to(45, Instant::now()));
    assert_eq!(view.panel_w(), 46);

    // Re-reporting the same column is a no-op (keeps the wire quiet).
    assert!(
        !view.drag_sideline_to(45, Instant::now()),
        "no crossing, no change"
    );
}

#[test]
fn sideline_drag_clamps_to_min_slim_and_the_terminal_max() {
    // AC1-EDGE: on an 80-col terminal 60% = 48 but term - MIN_CONTENT_COLS
    // = 40, so the tighter content bound wins; the floor is MIN_SLIM, never
    // hidden.
    let mut view = two_pane_view();
    view.term = (30, 80);
    view.density = Density::Regular;
    arm_sideline_drag(&mut view, Instant::now());

    view.drag_sideline_to(200, Instant::now()); // far right
    assert_eq!(
        view.panel_w(),
        40,
        "content bound (40) beats the 60% cap (48)"
    );
    assert_eq!(
        view.content_dims().1,
        MIN_CONTENT_COLS,
        "exactly the minimum content"
    );

    view.drag_sideline_to(0, Instant::now()); // far left
    assert_eq!(view.panel_w(), MIN_SLIM_PANEL_W, "clamps at the slim floor");
    assert!(view.panel_on, "the drag never hides the rail - `b` does");
}

#[test]
fn sideline_drag_below_a_mode_floor_demotes_the_mode() {
    // AC2-EDGE: dragging Extended below MIN_EXTENDED_PANEL_W (30) demotes to
    // Regular, and the drag keeps shrinking past that floor to MIN_SLIM (the
    // lower clamp is the constant, not the mode's floor).
    let mut view = two_pane_view();
    view.term = (30, 120);
    set_density(&mut view, Density::Extended); // width -> EXTENDED_PANEL_W
    arm_sideline_drag(&mut view, Instant::now());

    view.drag_sideline_to(MIN_EXTENDED_PANEL_W - 2, Instant::now()); // want 29 < 30
    assert_eq!(
        view.density,
        Density::Regular,
        "demoted to the widest fitting mode"
    );
    assert!(view.sideline_width < MIN_EXTENDED_PANEL_W);

    view.drag_sideline_to(0, Instant::now()); // keeps shrinking past the old floor
    assert_eq!(view.panel_w(), MIN_SLIM_PANEL_W);
    assert_eq!(
        view.density,
        Density::Regular,
        "no further demote below Regular"
    );
}

#[test]
fn a_preset_press_jumps_width_over_a_dragged_one() {
    // AC2-HP: after a free drag, the density key is a preset - it jumps to
    // the new mode's canonical width, not back to the dragged value.
    let mut view = two_pane_view();
    view.term = (30, 120);
    set_density(&mut view, Density::Regular);
    arm_sideline_drag(&mut view, Instant::now());
    view.drag_sideline_to(50, Instant::now()); // dragged to 51
    view.end_sideline_drag(TAB_BAR_ROWS, view.panel_w().saturating_sub(1));
    assert_eq!(view.sideline_width, 51);

    view.cycle_density(); // Regular -> Extended preset
    assert_eq!(view.density, Density::Extended);
    assert_eq!(
        view.sideline_width,
        canonical_width(Density::Extended),
        "the preset overwrote the dragged width"
    );
    view.cycle_density(); // -> Slim
    assert_eq!(view.sideline_width, canonical_width(Density::Slim));
    view.cycle_density(); // -> Regular (28, not the earlier 51)
    assert_eq!(view.sideline_width, canonical_width(Density::Regular));
}

#[test]
fn a_drag_report_after_a_mid_drag_winch_does_not_panic() {
    // codex P2: a WINCH shrinking the terminal below MIN_SLIM + MIN_CONTENT
    // while a drag is live makes sideline_max_width < MIN_SLIM; the old
    // `clamp(MIN_SLIM, max)` with min > max panicked. It must no-op instead.
    let mut view = two_pane_view();
    view.term = (30, 120);
    set_density(&mut view, Density::Regular);
    arm_sideline_drag(&mut view, Instant::now());
    view.drag_sideline_to(50, Instant::now());
    // The terminal collapses below MIN_SLIM_PANEL_W + MIN_CONTENT_COLS.
    view.term = (30, MIN_CONTENT_COLS + MIN_SLIM_PANEL_W - 2);
    assert!(sideline_max_width(view.term.1) < MIN_SLIM_PANEL_W);
    // The next report must not panic; it is a no-op (the rail is hidden).
    assert!(!view.drag_sideline_to(60, Instant::now()));
    assert_eq!(view.panel_w(), 0, "the rail is hidden at this size");
}

#[test]
fn a_clamped_drag_report_still_refreshes_the_timeout() {
    // codex P2: while the pointer keeps moving past the max bound each report
    // clamps to the same width; last_at must still advance, or an actively
    // held drag expires under the hand. Only a report-less gap should time out.
    let mut view = two_pane_view();
    view.term = (30, 80); // max = 40
    set_density(&mut view, Density::Regular);
    let t0 = Instant::now();
    arm_sideline_drag(&mut view, t0);
    // Drag to the max bound (40), then report a further column: same width,
    // but a later timestamp.
    view.drag_sideline_to(200, t0);
    let t1 = t0 + Duration::from_secs(1);
    assert!(
        !view.drag_sideline_to(201, t1),
        "still pinned at the max: no width change"
    );
    assert_eq!(
        view.sideline_drag.unwrap().last_at,
        t1,
        "yet the stuck-drag deadline advanced with the motion"
    );
}

#[test]
fn density_key_is_ignored_during_a_live_drag() {
    // AC3-FR: a density press mid-drag does not fight the pointer - the drag
    // owns the width until release.
    let mut view = two_pane_view();
    view.term = (30, 120);
    view.density = Density::Regular;
    arm_sideline_drag(&mut view, Instant::now());
    view.drag_sideline_to(50, Instant::now());
    let (w, d) = (view.sideline_width, view.density);

    view.cycle_density(); // pressed while dragging
    assert_eq!(view.density, d, "density unchanged mid-drag");
    assert_eq!(view.sideline_width, w, "width unchanged mid-drag");
}

#[test]
fn esc_reverts_a_sideline_drag_to_its_start_width() {
    // Mirrors the seam-drag Esc revert: the width returns to the grab value.
    let mut view = two_pane_view();
    view.term = (30, 120);
    view.density = Density::Regular;
    view.sideline_width = PANEL_W;
    arm_sideline_drag(&mut view, Instant::now());
    view.drag_sideline_to(50, Instant::now());
    assert_ne!(view.sideline_width, PANEL_W);

    assert!(
        view.revert_sideline_drag(),
        "reverting a moved drag reports a change"
    );
    assert_eq!(view.sideline_width, PANEL_W, "back to the width at grab");
    assert!(view.sideline_drag.is_none(), "the drag ended");
}

#[test]
fn a_timed_out_sideline_drag_keeps_the_reached_width() {
    // AC2-FR: the stuck-drag timeout ends the drag as a release would - it
    // keeps the reached width, it does not revert to start_width.
    let mut view = two_pane_view();
    view.term = (30, 120);
    view.density = Density::Regular;
    view.sideline_width = PANEL_W;
    arm_sideline_drag(&mut view, Instant::now());
    view.drag_sideline_to(50, Instant::now());
    let reached = view.sideline_width;
    assert_ne!(reached, PANEL_W);

    // The reaper ends it via end_sideline_drag (as a release does).
    view.end_sideline_drag(TAB_BAR_ROWS, view.panel_w().saturating_sub(1));
    assert!(view.sideline_drag.is_none(), "the drag ended");
    assert_eq!(view.sideline_width, reached, "width kept, not reverted");
}

#[test]
fn sideline_border_is_grabbable_only_while_the_sideline_shows() {
    let mut view = two_pane_view();
    view.term = (30, 120);
    view.density = Density::Regular;
    let border = view.panel_w() - 1;
    assert!(view.on_sideline_border(5, border));
    assert!(!view.on_sideline_border(5, border - 1), "inside the rail");
    assert!(!view.on_sideline_border(5, border + 1), "content side");
    assert!(!view.on_sideline_border(0, border), "tab bar row is chrome");
    // Hidden: no border to grab, so revealing stays on the toggle.
    view.panel_on = false;
    assert!(!view.on_sideline_border(5, border));
}

#[test]
fn seam_drag_is_not_grabbable_once_its_panes_are_gone() {
    let mut view = three_pane_view();
    view.begin_seam_drag(
        Seam {
            a: 998,
            b: 999,
            axis: Axis::Horizontal,
        },
        Instant::now(),
    );
    assert!(
        view.seam_drag.is_none(),
        "a seam with no live panes has no share to remember, so no grab"
    );
}

#[test]
fn seam_at_refuses_an_ambiguous_crossing() {
    // A '┼' is the intersection of two seams; picking one would resize a
    // divider the operator was not pointing at, so the cell is not a target.
    let mut view = two_pane_view();
    let r = |x, y, rows, cols| Rect { x, y, rows, cols };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        // A 2x2 grid: the crossing sits at content (14, 35).
        panes: vec![
            (30, r(0, 0, 14, 35)),
            (31, r(36, 0, 14, 36)),
            (32, r(0, 15, 14, 35)),
            (33, r(36, 15, 14, 36)),
        ],
        focus: 30,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    // Outer (1+14, 28+35) = (15, 63) is the crossing: both axes resolve.
    assert_eq!(view.seam_at(15, 63), None);
    // One cell off the crossing, each seam still resolves cleanly.
    assert_eq!(
        view.seam_at(5, 63).map(|s| (s.a, s.b)),
        Some((30, 31)),
        "above the crossing the vertical divider is unambiguous"
    );
    assert_eq!(
        view.seam_at(15, 40).map(|s| (s.a, s.b)),
        Some((30, 32)),
        "left of the crossing the horizontal divider is unambiguous"
    );
}

// A three-pane layout over two_pane_view's geometry (focus on pane 10, so
// 11 and 12 are both hover targets). Panes tile the 72-col content area:
// 10 -> outer 28.., 11 -> outer 52.., 12 -> outer 76...
//
// Also the smallest fixture in which a seam DROP means anything (x-aa95):
// a two-pane tab has one seam and it flanks both panes, so every seam drop
// there is an origin drop.
fn three_pane_view() -> View {
    let mut view = two_pane_view();
    let rect = |x| Rect {
        x,
        y: 0,
        rows: 29,
        cols: 23,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(10, rect(0)), (11, rect(24)), (12, rect(48))],
        focus: 10,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view
}

// -- (hover affordance) the link-probe clock and the local underline -------

#[test]
fn link_hover_probe_debounces_the_cell_and_fires_once() {
    // The probe clock is per-CELL (every crossed cell restarts it) and
    // fires exactly once per rest: a still pointer emits no further
    // events, so re-firing would spam the server at wake cadence.
    let mut st = LinkHoverState::default();
    let t0 = Instant::now();
    st.retarget(Some((10, 2, 3)), t0);
    assert!(
        st.take_due_probe(t0 + LINK_HOVER_DEBOUNCE - Duration::from_millis(1))
            .is_none(),
        "the quiet period holds"
    );
    let t = st
        .take_due_probe(t0 + LINK_HOVER_DEBOUNCE)
        .expect("fires at the deadline");
    assert_eq!((t.pane, t.row, t.col, t.seq), (10, 2, 3, 0));
    assert!(
        st.take_due_probe(t0 + Duration::from_secs(5)).is_none(),
        "a resting pointer never re-sends"
    );
    // Same cell again is jitter, not motion: no new clock.
    st.retarget(Some((10, 2, 3)), t0 + Duration::from_secs(6));
    assert_eq!(st.deadline(), None, "jitter does not re-arm a fired probe");
    // A NEW cell restarts the clock with a fresh seq.
    st.retarget(Some((10, 2, 4)), t0 + Duration::from_secs(7));
    assert_eq!(
        st.deadline(),
        Some(t0 + Duration::from_secs(7) + LINK_HOVER_DEBOUNCE)
    );
}

#[test]
fn link_hover_reply_is_accepted_only_for_the_current_target() {
    // The seq guard is the whole stale-rejection story: a reply for a
    // target the pointer left paints nothing; a current reply installs
    // the span; a current MISS clears it (never leaves an earlier span).
    let mut st = LinkHoverState::default();
    let t0 = Instant::now();
    st.retarget(Some((10, 2, 3)), t0);
    let a = st.take_due_probe(t0 + LINK_HOVER_DEBOUNCE).unwrap();
    // Pointer moved before the reply landed: stale, dropped whole.
    st.retarget(Some((10, 2, 9)), t0 + Duration::from_millis(80));
    assert!(
        !st.on_reply(a.pane, a.seq, vec![(2, 3)]),
        "stale reply dropped"
    );
    assert!(st.accepted.is_none(), "and paints nothing");
    let b = st
        .take_due_probe(t0 + Duration::from_millis(80) + LINK_HOVER_DEBOUNCE)
        .unwrap();
    assert!(
        st.on_reply(b.pane, b.seq, vec![(2, 9), (2, 10)]),
        "current reply accepted"
    );
    assert_eq!(st.accepted, Some((10, vec![(2, 9), (2, 10)])));
    // A current miss clears rather than leaving the previous span.
    st.retarget(Some((10, 3, 3)), t0 + Duration::from_secs(2));
    let c = st
        .take_due_probe(t0 + Duration::from_secs(2) + LINK_HOVER_DEBOUNCE)
        .unwrap();
    assert!(st.on_reply(c.pane, c.seq, Vec::new()));
    assert!(st.accepted.is_none(), "an empty reply clears the underline");
}

#[test]
fn link_hover_frame_invalidates_and_resequences() {
    // A new frame for the probed pane clears the accepted span and
    // restarts the quiet period FROM THE FRAME, so streaming output
    // postpones the next probe instead of scanning at frame cadence, and
    // the pre-frame probe's reply is re-sequenced away.
    let mut st = LinkHoverState::default();
    let t0 = Instant::now();
    st.retarget(Some((10, 2, 3)), t0);
    let a = st.take_due_probe(t0 + LINK_HOVER_DEBOUNCE).unwrap();
    st.accepted = Some((10, vec![(2, 3)]));
    let tf = t0 + Duration::from_secs(1);
    st.on_frame(10, tf);
    assert!(st.accepted.is_none(), "the frame cleared the span");
    assert_eq!(
        st.deadline(),
        Some(tf + LINK_HOVER_DEBOUNCE),
        "postponed by the frame"
    );
    assert!(
        !st.on_reply(a.pane, a.seq, vec![(2, 3)]),
        "the pre-frame probe's reply is stale after re-sequencing"
    );
    // A frame for ANOTHER pane leaves this target alone.
    st.on_frame(99, tf + Duration::from_secs(1));
    assert_eq!(
        st.deadline(),
        Some(tf + LINK_HOVER_DEBOUNCE),
        "another pane's frame is a no-op"
    );
    // Chrome/divider/outside: everything drops, with no request.
    st.retarget(None, tf + Duration::from_secs(2));
    assert!(st.pending.is_none() && st.accepted.is_none());
    assert_eq!(st.deadline(), None);
}

#[test]
fn link_hover_compose_hides_the_span_while_a_modal_owns_the_screen() {
    // A modal opened by KEYBOARD emits no pointer event, so the event-side
    // clear never runs; the compose-side suppression is what keeps the
    // underline from painting beneath or around it. Control: the same
    // accepted span paints the moment the modal closes.
    let mut view = two_pane_view();
    view.link_hover.accepted = Some((10, vec![(0, 0)]));
    view.keys_modal = Some(build_keys_modal());
    let ul = cell_flags::UNDERLINE;
    let lit = |f: &Frame| f.cells.iter().any(|c| c.flags & ul == ul);
    assert!(!lit(&view.compose()), "no underline beneath an open modal");
    view.keys_modal = None;
    assert!(
        lit(&view.compose()),
        "control: the span paints once the modal closes"
    );
}

#[test]
fn link_hover_compose_underlines_exactly_the_accepted_cells() {
    // The affordance is client-local: compose ORs UNDERLINE onto exactly
    // the accepted pane cells, and the cached server Frame is untouched,
    // so clearing the span restores the byte-identical frame.
    let mut view = two_pane_view();
    let clean = view.compose();
    view.link_hover.accepted = Some((10, vec![(0, 3), (1, 4)]));
    let lit = view.compose();
    let ul = cell_flags::UNDERLINE;
    // Pane 10's rect sits at the content origin (row 1, col 28).
    let underlined = |f: &Frame| -> Vec<(usize, usize)> {
        (0..f.rows as usize)
            .flat_map(move |r| (0..f.cols as usize).map(move |c| (r, c)))
            .filter(|&(r, c)| f.cells[r * f.cols as usize + c].flags & ul == ul)
            .collect()
    };
    assert_eq!(
        underlined(&lit),
        vec![(1, 28 + 3), (2, 28 + 4)],
        "exactly the two accepted cells, at the pane's screen position"
    );
    assert!(
        underlined(&clean).is_empty(),
        "control: nothing is underlined without an accepted span"
    );
    // The cached server frame is untouched: a clear restores the exact
    // prior compose.
    view.link_hover.clear();
    assert_eq!(view.compose().cells, clean.cells);
}

#[test]
fn hover_focus_settles_on_a_landed_pane() {
    // AC1-HP: land in a non-focused pane and rest. on_hover records the
    // pending target on the SINGLE landing event (the land-and-stop gesture
    // emits nothing further); the settle timer then commits it once, and
    // take_settled_hover clears so it cannot re-fire.
    let mut view = two_pane_view(); // focus 11; pane 10 at outer col 28..
    let t0 = Instant::now();
    view.on_hover(5, 30, t0);
    assert_eq!(
        view.hover_pending.map(|(p, _)| p),
        Some(10),
        "landing recorded on one event"
    );
    assert_eq!(
        view.take_settled_hover(),
        Some(10),
        "timer commits the pane"
    );
    assert_eq!(view.take_settled_hover(), None, "cleared: no re-fire");
}

#[test]
fn hover_focus_keeps_landing_time_while_on_same_pane() {
    // Continued motion WITHIN the pane must not push the settle deadline
    // forward (else a slow drag never settles): the landing instant is kept.
    let mut view = two_pane_view();
    let t0 = Instant::now();
    view.on_hover(5, 30, t0);
    view.on_hover(5, 31, t0 + Duration::from_millis(40)); // still moving in 10
    assert_eq!(
        view.hover_pending,
        Some((10, t0)),
        "same pane -> original landing time preserved"
    );
}

#[test]
fn hover_focus_coalesces_fast_sweep_to_settled_pane() {
    // AC2-FR: a fast sweep across three panes leaves ONLY the pane the pointer
    // rests on pending, so the timer fires one FocusPane - not one per pane.
    // Each new pane replaces the last before its deadline; 11 is dropped.
    let mut view = three_pane_view(); // focus 10; sweep 11 -> 12
    let t0 = Instant::now();
    view.on_hover(5, 55, t0); // land on 11
    view.on_hover(5, 80, t0 + Duration::from_millis(10)); // sweep to 12: 11 dropped
    assert_eq!(
        view.hover_pending.map(|(p, _)| p),
        Some(12),
        "only 12 survives the sweep"
    );
    assert_eq!(view.take_settled_hover(), Some(12), "one FocusPane, to 12");
}

#[test]
fn hover_focus_off_switch_disables_follow() {
    // AC3-EDGE: config.mux.hover_focus=false -> nothing ever becomes pending,
    // so the timer has nothing to commit. The sideline highlight is
    // unaffected (it is independent of the focus-follows switch).
    let mut view = two_pane_view();
    view.hover_focus = false;
    let t0 = Instant::now();
    view.on_hover(5, 30, t0);
    assert_eq!(view.hover_pending, None, "no settle target while disabled");
    assert_eq!(view.take_settled_hover(), None);
    // Highlight still tracks the sideline. (x-cd67 US1) squad 2 "notes"
    // (display index 1) now sits at terminal row 1 (the sideline owns row 0).
    view.on_hover(1, 5, t0);
    assert_eq!(view.hover_row, Some(1));
}

#[test]
fn hover_focus_does_not_settle_on_the_focused_pane() {
    // Hovering the already-focused pane is a no-op: no pending target, so the
    // timer never fires a redundant FocusPane to the current focus.
    let mut view = two_pane_view(); // focus 11 at outer col 64..
    view.on_hover(5, 70, Instant::now());
    assert_eq!(view.hover_pending, None);
    assert_eq!(view.take_settled_hover(), None);
}

#[test]
fn hover_arms_selector_on_the_pointed_row_without_switching_squad() {
    // (x-f331 US1, was hover_highlights_sideline_row_without_switching_squad):
    // hovering an ACTIONABLE sideline row now ARMS the selector to it (one
    // regime, so x/X/r act on the pointed-at row), the highlight is still set,
    // and the active squad/tab never change. A spacer or the pane disarms.
    // Rows (two_pane_view): idx 0 footnote header (actionable), idx 1 Blank
    // spacer (inert), idx 2 notes header (actionable).
    let mut view = two_pane_view();
    let before = view.layout.active_squad;

    view.on_hover(0, 5, Instant::now()); // outer row 0 = footnote squad header
    assert_eq!(view.hover_row, Some(0));
    assert_eq!(view.selector, Some(0), "hover arms the selector to the row");
    assert!(
        view.sel_hover_armed,
        "the arm is motion-fresh (hover-armed)"
    );
    assert_eq!(
        view.layout.active_squad, before,
        "hover never switches squad"
    );

    // Hover onto the inert spacer: highlight tracks the cell, but nothing
    // actionable is there, so the hover-arm disarms rather than pointing the
    // verbs at a spacer.
    view.on_hover(1, 5, Instant::now());
    assert_eq!(view.hover_row, Some(1));
    assert_eq!(view.selector, None, "an inert row disarms the hover-arm");
    assert!(!view.sel_hover_armed);

    // Re-arm on a fresh actionable row (pointer motion re-arms).
    view.on_hover(2, 5, Instant::now());
    assert_eq!(view.selector, Some(2), "motion re-arms to the new row");
    assert!(view.sel_hover_armed);

    view.on_hover(5, 40, Instant::now()); // onto pane content
    assert_eq!(view.hover_row, None, "off the panel clears the highlight");
    assert_eq!(view.selector, None, "off the panel disarms the selector");
    assert!(!view.sel_hover_armed);
}

#[test]
fn hover_arm_does_not_clobber_an_explicit_selector() {
    // (x-f331) An explicit prefix+w selector (sel_hover_armed=false) keeps
    // keyboard control: a stray hover does not demote it to a motion-fresh arm
    // that j/k would disarm.
    let mut view = two_pane_view();
    view.selector = Some(2); // opened explicitly, not hover-armed
    view.sel_hover_armed = false;
    view.on_hover(0, 5, Instant::now()); // hover a different actionable row
    assert_eq!(
        view.selector,
        Some(2),
        "explicit selector is not moved by hover"
    );
    assert!(!view.sel_hover_armed, "explicit selector stays fully modal");
}

#[test]
fn open_create_is_modal_over_keyboard_overlays() {
    // codex peer review: create_keys routes AFTER selector/answers, so
    // opening the create overlay while one is open must clear it - else the
    // typed workspace name drives the selector instead.
    let mut view = two_pane_view();
    view.selector = Some(0);
    view.answers = Some(0);
    view.yard = Some(YardSel {
        sel: 0,
        opened_at: Instant::now(),
    });
    view.open_create();
    assert!(view.selector.is_none(), "create clears an open selector");
    assert!(
        view.answers.is_none(),
        "create clears an open answer overlay"
    );
    assert!(view.yard.is_none(), "create clears an open yard");
    assert!(view.search.is_none());
    assert_eq!(
        view.create.as_deref(),
        Some(""),
        "the create overlay opens with an empty buffer"
    );
}

#[test]
fn layout_push_clears_stale_hover_row() {
    // change #3 AC3-FR: a layout push that drops the hovered row must not
    // leave the highlight on a now-out-of-range index.
    let mut view = two_pane_view();
    // With one squad (auto-expanded: 2 tab rows), display_rows is
    // [squad, tab, tab, + new workspace] (len 4), so a hover on index 4
    // is now stale and must be cleared by the push.
    view.hover_row = Some(4);
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)], // second squad dropped
        active_squad: 1,
        panes: vec![(
            11,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 11,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert_eq!(view.hover_row, None);
}

#[test]
fn chrome_hit_card_opens_confirm_with_node() {
    // change #4: a work-queue card click resolves to a Confirm carrying the
    // node id and its display label (slug preferred), not a silent dispatch.
    let mut view = two_pane_view();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(
            11,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 11,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: vec![BacklogCard {
            id: "x-a496".into(),
            slug: "hover-cards".into(),
            priority: "p2".into(),
            state: CardState::Ready,
            pane_id: None,
            attach_id: None,
            where_hint: None,
            project: None,
            lane: None,
            plan_path: None,
            head: false,
        }],
        backlog_lanes: vec![(crate::backlog_view::UNLANED.into(), 1)],
        backlog_stale: false,
    });
    view.expand_pull_sections(); // (x-c5ee) ~ backlog now defaults Collapsed
                                 // display_rows (x-0090, no tab rows): [footnote squad, + new workspace,
                                 // Header, Card] -> the card is index 3, at outer row 3 (x-cd67 US1: the
                                 // sideline owns row 0, so outer row == display index).
    match view.chrome_hit(3, 5) {
        Some(ChromeHit::Confirm(a)) => {
            assert!(
                matches!(&a.action, ConfirmKind::Dispatch { node } if node == "x-a496"),
                "confirm dispatches the card's node"
            );
            assert_eq!(a.label, "hover-cards");
        }
        other => panic!("expected Confirm, got {}", chrome_hit_label(&other)),
    }
}

#[test]
fn chrome_hit_non_ready_card_is_notice_not_confirm() {
    // A blocked/in-flight card is NOT dispatchable (codex peer review): the
    // click is a local notice, never a Confirm that would start work prefix+g
    // would skip. Two cards under the work-queue header.
    let mut view = two_pane_view();
    let card = |id: &str, state| BacklogCard {
        id: id.into(),
        slug: String::new(),
        priority: "p2".into(),
        state,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(
            11,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 11,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: vec![
            card("x-blk", CardState::Blocked),
            card("x-fly", CardState::InFlight),
        ],
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view.expand_pull_sections(); // (x-c5ee) ~ backlog now defaults Collapsed
                                 // display_rows (x-0090, no tab rows): [squad, + new workspace, Header,
                                 // blocked, in-flight] -> the cards paint at outer rows 3, 4 (x-cd67 US1).
    assert!(
        matches!(view.chrome_hit(3, 5), Some(ChromeHit::Notice(_))),
        "blocked card -> notice, not confirm"
    );
    assert!(
        matches!(view.chrome_hit(4, 5), Some(ChromeHit::Notice(_))),
        "in-flight card -> notice, not confirm"
    );
}

#[test]
fn chrome_hit_inflight_card_routes_pane_then_attach_then_hint() {
    // x-54fa: an in-flight card is no longer a dead-end. Route priority
    // (plan Locked 5): a pane in this session focuses; a paneless bg
    // worker attaches (same command the agents-row click sends, so the
    // v14 server gates apply); nothing routable says WHERE the work is
    // (the server's where_hint), never a bare "already dispatching".
    let mut view = two_pane_view();
    let card =
        |id: &str, pane: Option<u64>, attach: Option<&str>, hint: Option<&str>| BacklogCard {
            id: id.into(),
            slug: String::new(),
            priority: "p2".into(),
            state: CardState::InFlight,
            pane_id: pane,
            attach_id: attach.map(str::to_owned),
            where_hint: hint.map(str::to_owned),
            project: None,
            lane: None,
            plan_path: None,
            head: false,
        };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(
            11,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 11,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: vec![
            // Pane beats attach when the server sent both (it never does,
            // but the client must not attach when a local pane exists).
            card("x-aaa", Some(11), Some("deadbee1"), None),
            card("x-bbb", None, Some("deadbee2"), None),
            card("x-ccc", None, None, Some("in flight - worked by t:abc")),
            card("x-ddd", None, None, None),
        ],
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view.expand_pull_sections(); // (x-c5ee) ~ backlog now defaults Collapsed
                                 // display_rows (x-0090, no tab rows): [squad, + new workspace, Header,
                                 // 4 cards] -> rows 3-6 (x-cd67 US1: outer row == display index).
    assert_eq!(cmds(view.chrome_hit(3, 5)), vec![Command::FocusPane(11)]);
    assert_eq!(
        cmds(view.chrome_hit(4, 5)),
        vec![Command::attach_agent("deadbee2")]
    );
    match view.chrome_hit(5, 5) {
        Some(ChromeHit::Notice(msg)) => assert_eq!(msg, "in flight - worked by t:abc"),
        other => panic!("expected hint notice, got {}", chrome_hit_label(&other)),
    }
    match view.chrome_hit(6, 5) {
        Some(ChromeHit::Notice(msg)) => {
            assert_eq!(msg, "card in flight - no session visible here")
        }
        other => panic!("expected default notice, got {}", chrome_hit_label(&other)),
    }
}

fn cmds(hit: Option<ChromeHit>) -> Vec<Command> {
    match hit {
        Some(ChromeHit::Cmds(c)) => c,
        other => panic!("expected Cmds, got {}", chrome_hit_label(&other)),
    }
}

fn chrome_hit_label(hit: &Option<ChromeHit>) -> &'static str {
    match hit {
        None => "None",
        Some(ChromeHit::Cmds(_)) => "Cmds",
        Some(ChromeHit::Notice(_)) => "Notice",
        Some(ChromeHit::Confirm(_)) => "Confirm",
        Some(ChromeHit::OpenCreate) => "OpenCreate",
        Some(ChromeHit::CycleSection(_)) => "CycleSection",
        Some(ChromeHit::SortColumn(_)) => "SortColumn",
        Some(ChromeHit::ToggleIdle(_)) => "ToggleIdle",
        Some(ChromeHit::OpenSidelineMenu { .. }) => "OpenSidelineMenu",
        Some(ChromeHit::CycleDensity) => "CycleDensity",
    }
}

// A left click on the tab bar switches to the clicked tab, opens a new one on
// the `+`, and does nothing on the inert squad-name label.
#[test]
fn chrome_hit_tab_bar_routes_tabs_and_new_tab() {
    let view = two_pane_view(); // active squad 1 "footnote", tabs 0 & 1, +.
                                // (x-cd67 US1) The strip is scoped to the content area (origin
                                // panel_w=28): " footnote "=28..37, " 1 "=38..40, "[2]"=41..43,
                                // " + "=44..46.
    assert_eq!(cmds(view.chrome_hit(0, 39)), vec![Command::SelectTab(0)]);
    assert_eq!(cmds(view.chrome_hit(0, 42)), vec![Command::SelectTab(1)]);
    assert_eq!(cmds(view.chrome_hit(0, 45)), vec![Command::NewTab]);
    // The squad-name label is inert.
    assert!(view.chrome_hit(0, 33).is_none());
}

// (x-cd67 US1, AC1-HP) The tab strip is scoped to the content columns: its
// first painted cell is at column panel_w, and terminal row 0 in the sideline
// columns belongs to the sideline (squad 1), not the strip.
#[test]
fn tab_strip_scoped_to_content_area_row0_is_sideline() {
    let view = two_pane_view();
    let panel_w = view.panel_w() as usize;
    assert_eq!(panel_w, 28);
    let frame = view.compose();
    let cols = frame.cols as usize;
    // Left of the divider on row 0 is the sideline's squad-1 caret, not chrome.
    assert_eq!(frame.cells[0].c, '▾', "row 0 col 0 is the squad-1 caret");
    // The divider column runs full height, including row 0.
    assert_eq!(frame.cells[panel_w - 1].c, '│', "divider at row 0");
    // The strip's first span (the active squad name) begins at panel_w.
    let strip: String = (panel_w..cols).map(|c| frame.cells[c].c).collect();
    assert!(
        strip.trim_start().starts_with("footnote"),
        "strip begins at panel_w: {strip:?}"
    );
    // A row-0 click left of the divider toggles squad 1 (the active squad row),
    // never a tab.
    assert!(matches!(
        view.chrome_hit(0, 2),
        Some(ChromeHit::CycleSection(SectionKey::Squad(_)))
    ));
}

// A left click on an inactive sideline squad row switches to it; the
// already-active squad row toggles its caret locally instead of the old
// silent SelectSquad no-op (x-2f99, AC3-HP/AC4-HP).
#[test]
fn chrome_hit_sideline_squad_rows() {
    // Rows (x-cd67 US1 sideline owns row 0; US3 adds a Blank spacer between
    // the two squad groups): [squad 1 (0), Blank (1), squad 2 (2), footer (3)].
    let view = two_pane_view();
    assert!(matches!(
        view.chrome_hit(0, 4),
        Some(ChromeHit::CycleSection(SectionKey::Squad(_)))
    ));
    assert_eq!(cmds(view.chrome_hit(2, 4)), vec![Command::SelectSquad(2)]);
    // The Blank spacer row is inert.
    assert!(view.chrome_hit(1, 4).is_none());
    // The divider column and the pane content beyond it are not chrome hits.
    assert!(view.chrome_hit(2, 27).is_none());
    assert!(view.chrome_hit(2, 40).is_none());
}

#[test]
fn chrome_hit_adds_sideline_offset_when_scrolled() {
    // Regression (codex P2): a click must invert draw_sideline's scroll
    // offset, so a click on a scrolled row activates the row painted there,
    // not the unscrolled row at the same terminal cell.
    // Rows (x-cd67 US1 owns row 0; US3 Blank spacer at 1): [squad1(0),
    // Blank(1), squad2(2), footer(3)]. display index == terminal row.
    let mut v = two_pane_view();
    // Unscrolled: terminal row 2 -> display index 2 -> squad2.
    assert_eq!(cmds(v.chrome_hit(2, 4)), vec![Command::SelectSquad(2)]);
    // Scrolled by 1: terminal row 1 -> display index 2 -> squad2 (without the
    // offset it would resolve to index 1, the Blank spacer).
    v.sideline_offset = 1;
    assert_eq!(
        cmds(v.chrome_hit(1, 4)),
        vec![Command::SelectSquad(2)],
        "click resolves through the scroll offset"
    );
}

// ---- x-2f99: active-squad visibility ----

/// two_pane_view's layout with a chosen active squad (LayoutView is not
/// Clone; squad 1 has 2 tabs, squad 2 has 1).
fn two_squad_layout(active_squad: u64) -> LayoutView {
    LayoutView {
        squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
        active_squad,
        panes: vec![],
        focus: 0,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    }
}

// AC2-HP: a fresh attach seeds the active squad expanded, so the first
// frame shows its tabs (and the `*` marker) without any keypress.
#[test]
fn view_new_seeds_expanded_with_active_squad() {
    let view = two_pane_view();
    assert!(view.squad_view(1) == SectionView::Expanded);
    assert!(view.squad_view(2) == SectionView::Collapsed);
}

// AC1-HP + Locked 3 (x-c5ee): the active squad defaults Expanded and an
// inactive squad with no explicit choice defaults Collapsed. The default is
// computed live in `section_view`, so switching the active squad re-points
// which one opens with no map write - the squad you leave folds back to its
// header (attention-first), it is not force-collapsed.
#[test]
fn active_squad_defaults_expanded_inactive_collapsed_on_activation_change() {
    let mut view = two_pane_view();
    view.set_layout(two_squad_layout(2));
    assert!(
        view.squad_view(2) == SectionView::Expanded,
        "the active squad defaults expanded"
    );
    assert!(
        view.squad_view(1) == SectionView::Collapsed,
        "an inactive squad with no explicit choice defaults collapsed"
    );
}

// AC1-EDGE + Locked 2 (x-c5ee): an explicit collapse of the active squad
// outranks the computed Expanded default - it survives both the ~250ms
// scrape-tick pushes AND a later re-activation. Only an explicit re-expand
// (another cycle) brings it back, never an activation - the old force-seed
// that re-opened on re-activation is gone.
#[test]
fn manual_collapse_survives_same_active_layout_push() {
    let mut view = two_pane_view();
    view.cycle_squad(1);
    assert!(view.squad_view(1) == SectionView::Collapsed);
    view.set_layout(two_squad_layout(1));
    assert!(
        view.squad_view(1) == SectionView::Collapsed,
        "a push with an unchanged active_squad must not re-expand"
    );
    // Re-activation must NOT re-expand: the explicit choice wins.
    view.set_layout(two_squad_layout(2));
    view.set_layout(two_squad_layout(1));
    assert!(
        view.squad_view(1) == SectionView::Collapsed,
        "an explicit collapse outranks the active-squad default on re-activation"
    );
}

// AC3-EDGE: an expanded squad removed server-side leaves `expanded`.
#[test]
fn set_layout_prunes_dead_squad_ids_from_expanded() {
    let mut view = two_pane_view();
    let mut layout = two_squad_layout(2);
    layout.squads.remove(0); // squad 1 (expanded) vanishes
    view.set_layout(layout);
    assert!(
        view.squad_view(1) == SectionView::Collapsed,
        "dead id pruned"
    );
    assert!(view.squad_view(2) == SectionView::Expanded);
}

// A synthetic mission squad has no `active_squad` moment to ride in on (it
// is never selectable server-side), so `section_view` gives every mission
// the Expanded-tier default directly (x-c5ee) - else its grouped workers
// stay invisible with no way to reveal them (codex review of x-1a47 change
// 2/3, P1-a). No seed: the default is computed live.
#[test]
fn new_mission_squad_defaults_expanded() {
    let mut view = two_pane_view();
    let mut layout = two_squad_layout(1);
    let mid = mission_meta(1, "mux-squad  1/2").id;
    layout.squads.push(mission_meta(1, "mux-squad  1/2"));
    view.set_layout(layout);
    assert!(
        view.squad_view(mid) == SectionView::Expanded,
        "a mission defaults expanded via section_view"
    );
}

// (x-c5ee) A section-view agent: only the fields the majority check reads
// (squad, exited) matter; badge/seen round out a plausible row.
fn sv_agent(squad: u64, name: &str, badge: Option<AgentBadge>, exited: bool) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(squad),
        name: name.into(),
        pane_id: None,
        badge,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
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
        // (x-d401) A badgeless LIVE row here means "an idle worker"; the
        // reading is said explicitly, because absence now renders
        // Unmeasured (`?`), never Idle.
        pane_activity: if badge.is_none() && !exited {
            Some(ShellActivity::Idle)
        } else {
            None
        },
    }
}

// (x-c5ee) Point the view store at a fresh temp dir so a computed-default
// test never reads or writes the real `~/.fno/mux-view.json` (hermeticity).
// Caller clears with `clear_test_path` + removes the returned dir.
fn isolate_view_store(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fno-view-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::view_store::set_test_path(&dir);
    dir
}

// AC1-HP (x-c5ee): a majority-exited active squad defaults to LiveOnly, no
// persisted choice needed - the dead rows fold behind the header's ✗N.
#[test]
fn majority_exited_active_squad_defaults_live_only() {
    let dir = isolate_view_store("majexit");
    let view = view_with_agents(vec![
        sv_agent(1, "a", Some(AgentBadge::Done), true),
        sv_agent(1, "b", Some(AgentBadge::Done), true),
        sv_agent(1, "c", Some(AgentBadge::Done), true),
        sv_agent(1, "d", Some(AgentBadge::Working), false),
    ]);
    assert_eq!(
        view.squad_view(1),
        SectionView::LiveOnly,
        "3 of 4 exited is a majority -> LiveOnly"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC2-EDGE (x-c5ee): a 50/50 split is not a strict majority, so the active
// squad keeps its Expanded default.
#[test]
fn even_split_active_squad_stays_expanded() {
    let dir = isolate_view_store("even");
    let view = view_with_agents(vec![
        sv_agent(1, "a", None, true),
        sv_agent(1, "b", None, true),
        sv_agent(1, "c", None, false),
        sv_agent(1, "d", None, false),
    ]);
    assert_eq!(
        view.squad_view(1),
        SectionView::Expanded,
        "exited * 2 > total is false for 2 of 4"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC1-EDGE (x-c5ee): an empty active squad keeps Expanded (0 is never a
// majority).
#[test]
fn empty_active_squad_stays_expanded() {
    let dir = isolate_view_store("empty");
    let view = view_with_agents(vec![]);
    assert_eq!(view.squad_view(1), SectionView::Expanded);
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC3-FR (x-c5ee): the majority default recomputes as agents exit
// mid-session, with no operator gesture and no persisted write - the whole
// reason the default is live in `section_view` rather than a one-time seed.
#[test]
fn majority_default_recomputes_as_agents_exit() {
    let dir = isolate_view_store("recompute");
    let mut view = view_with_agents(vec![
        sv_agent(1, "a", Some(AgentBadge::Working), false),
        sv_agent(1, "b", Some(AgentBadge::Working), false),
        sv_agent(1, "c", Some(AgentBadge::Working), false),
        sv_agent(1, "d", Some(AgentBadge::Done), true),
    ]);
    assert_eq!(
        view.squad_view(1),
        SectionView::Expanded,
        "1 of 4 exited is not a majority"
    );
    // Two more finish and exit; no operator gesture touches the section.
    view.layout.agents = vec![
        sv_agent(1, "a", Some(AgentBadge::Done), true),
        sv_agent(1, "b", Some(AgentBadge::Done), true),
        sv_agent(1, "c", Some(AgentBadge::Working), false),
        sv_agent(1, "d", Some(AgentBadge::Done), true),
    ];
    assert_eq!(
        view.squad_view(1),
        SectionView::LiveOnly,
        "3 of 4 exited now -> the default tracks the exits, no seed to go stale"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC2-HP (x-c5ee): the two pull-sections default Collapsed - the top of the
// panel is the operator's own agents, the pull-sections one click away.
#[test]
fn pull_sections_default_collapsed() {
    let dir = isolate_view_store("pull");
    let view = two_pane_view();
    assert_eq!(
        view.section_view(&SectionKey::Elsewhere),
        SectionView::Collapsed
    );
    assert_eq!(
        view.section_view(&SectionKey::WorkQueue),
        SectionView::Collapsed
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC1-FR (x-c5ee): an explicit persisted choice outranks the new Collapsed
// pull-section default. Inserted straight into the map to mirror a value
// loaded from disk, without touching the real store.
#[test]
fn persisted_choice_outranks_pull_section_default() {
    let dir = isolate_view_store("pull-persist");
    let mut view = two_pane_view();
    view.section_view
        .insert(SectionKey::Elsewhere, SectionView::Expanded);
    assert_eq!(
        view.section_view(&SectionKey::Elsewhere),
        SectionView::Expanded,
        "an operator's saved expand of ~ elsewhere survives its Collapsed default"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- x-c5ee US2: the top-K idle cap ----

/// The fold row's `(hidden, expanded)` if one is emitted, else None.
fn idle_fold(v: &View) -> Option<(usize, bool)> {
    v.display_rows().iter().find_map(|r| match r {
        DisplayRow::IdleFold {
            hidden, expanded, ..
        } => Some((*hidden, *expanded)),
        _ => None,
    })
}

/// Count rendered agent rows whose name starts with `prefix`.
fn rendered(v: &View, prefix: &str) -> usize {
    v.display_rows()
        .iter()
        .filter(|r| matches!(r, DisplayRow::Agent(a) if a.name.starts_with(prefix)))
        .count()
}

// The active squad's canonical key in the two_pane_view fixture.
fn footnote_key() -> SectionKey {
    SectionKey::Squad("/code/footnote".into())
}

// AC3-HP (x-c5ee): 2 Working + 12 Idle, cap 8 -> both Working render, 6 idle
// fill to the cap, and a `+6 idle` fold row follows.
#[test]
fn idle_cap_folds_the_overflow_into_plus_n_idle() {
    let dir = isolate_view_store("cap-hp");
    let mut agents = vec![
        sv_agent(1, "w1", Some(AgentBadge::Working), false),
        sv_agent(1, "w2", Some(AgentBadge::Working), false),
    ];
    for i in 0..12 {
        agents.push(sv_agent(1, &format!("idle{i}"), None, false));
    }
    let view = view_with_agents(agents);
    assert_eq!(rendered(&view, "w"), 2, "attention rows always render");
    assert_eq!(
        rendered(&view, "idle"),
        6,
        "idle fills to the cap of 8 live"
    );
    assert_eq!(idle_fold(&view), Some((6, false)), "a folded +6 idle row");
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC2-UI (x-c5ee): attention rows are never folded - 10 Blocked, 0 idle, cap
// 8 -> all 10 render, no fold row.
#[test]
fn attention_rows_are_never_folded_by_the_cap() {
    let dir = isolate_view_store("cap-att");
    let agents: Vec<AgentRow> = (0..10)
        .map(|i| sv_agent(1, &format!("b{i}"), Some(AgentBadge::Blocked), false))
        .collect();
    let view = view_with_agents(agents);
    assert_eq!(
        rendered(&view, "b"),
        10,
        "all 10 attention rows render past the cap"
    );
    assert_eq!(idle_fold(&view), None, "no idle -> no fold row");
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC (x-c5ee): exactly SQUAD_ROW_CAP idle rows emit no fold (never a `+0`).
#[test]
fn exactly_cap_idle_rows_emit_no_fold() {
    let dir = isolate_view_store("cap-exact");
    let agents: Vec<AgentRow> = (0..SQUAD_ROW_CAP)
        .map(|i| sv_agent(1, &format!("idle{i}"), None, false))
        .collect();
    let view = view_with_agents(agents);
    assert_eq!(
        rendered(&view, "idle"),
        SQUAD_ROW_CAP,
        "the whole idle set fits"
    );
    assert_eq!(
        idle_fold(&view),
        None,
        "no +0 idle row when it fits exactly"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC3-EDGE (x-c5ee): an Expanded squad with 2 exited + 13 idle renders both
// dead rows (Expanded shows them), 8 live idle (dead never consume the cap),
// and a `+5 idle` fold - not `+7` (the dead rows are not counted as idle).
#[test]
fn dead_rows_stay_in_the_dead_bucket_not_the_idle_fold() {
    let dir = isolate_view_store("cap-dead");
    let mut agents = vec![
        sv_agent(1, "dead1", None, true),
        sv_agent(1, "dead2", None, true),
    ];
    for i in 0..13 {
        agents.push(sv_agent(1, &format!("idle{i}"), None, false));
    }
    let mut view = view_with_agents(agents);
    view.set_squad_view(1, SectionView::Expanded);
    assert_eq!(rendered(&view, "dead"), 2, "Expanded shows every dead row");
    assert_eq!(rendered(&view, "idle"), 8, "the cap budgets live rows only");
    assert_eq!(
        idle_fold(&view),
        Some((5, false)),
        "+5 idle, dead not counted"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC4-EDGE (x-c5ee): the same squad in LiveOnly hides the dead rows but the
// idle fold is unchanged - the cap never counted the dead in the first place.
#[test]
fn live_only_hides_dead_and_keeps_the_same_idle_fold() {
    let dir = isolate_view_store("cap-liveonly");
    let mut agents = vec![
        sv_agent(1, "dead1", None, true),
        sv_agent(1, "dead2", None, true),
    ];
    for i in 0..13 {
        agents.push(sv_agent(1, &format!("idle{i}"), None, false));
    }
    let mut view = view_with_agents(agents);
    view.set_squad_view(1, SectionView::LiveOnly);
    assert_eq!(rendered(&view, "dead"), 0, "LiveOnly hides the dead rows");
    assert_eq!(rendered(&view, "idle"), 8, "the live idle cap is unchanged");
    assert_eq!(idle_fold(&view), Some((5, false)), "still +5 idle");
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC12-HP: Space on a workspace row opens a LOCAL peek (tabs + members
// from the layout), no wire round trip; a late agent PeekBody cannot land
// in it; it closes when the squad goes and holds while it stays.
#[tokio::test]
async fn space_opens_a_local_workspace_peek() {
    let mut v = unified_rows_view();
    let hdr = squad_header_at(&v, 1);
    v.selector = Some(hdr);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b" ", &mut buf).await.unwrap();
    let peek = v.peek.as_ref().expect("peek opens on a workspace row");
    assert_eq!(peek.squad, Some(1), "marked as a workspace peek");
    let body = peek.body.as_ref().expect("the body renders locally");
    assert!(
        body.iter().any(|l| l.contains("worker")),
        "the member row is listed: {body:?}"
    );
    assert!(
        body.iter()
            .any(|l| l.contains("tab") || l.contains("origin")),
        "the summary carries tabs/origin"
    );
    assert!(buf.is_empty(), "no wire command for a workspace peek");
    // The seq open_peek consumed was never attached to a request, so no
    // PeekBody can arrive carrying it: a superseded agent request (the
    // prior seq) is dropped by the guard instead of landing here.
    assert!(peek.seq >= 1);
}

#[tokio::test]
async fn workspace_peek_holds_on_layout_and_closes_when_the_squad_goes() {
    let mut v = unified_rows_view();
    let hdr = squad_header_at(&v, 1);
    v.selector = Some(hdr);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b" ", &mut buf).await.unwrap();
    assert!(v.peek.is_some());

    // Same squad at the cursor: holds (and never refetches an agent).
    assert_eq!(v.peek_reanchor(), None, "holds, no agent refetch");
    assert!(
        v.peek.is_some(),
        "the workspace peek survives a layout push"
    );
    // The auto-refresh path never arms for a workspace peek.
    assert!(v.peek_refresh_due().is_none(), "no PeekAgent is ever sent");

    // The squad gone: the peek closes rather than re-anchoring onto an
    // agent row.
    v.layout.squads.retain(|s| s.id != 1);
    assert_eq!(v.peek_reanchor(), None);
    assert!(v.peek.is_none(), "the peek closes with its workspace");
}

// AC1-UI (x-c5ee): the fold toggles visibly and reversibly - folded `+N more`
// -> all idle shown with a `- fewer` affordance -> folded again.
#[test]
fn idle_fold_toggles_visibly_and_reversibly() {
    let dir = isolate_view_store("cap-toggle");
    let mut agents = vec![sv_agent(1, "w", Some(AgentBadge::Working), false)];
    for i in 0..12 {
        agents.push(sv_agent(1, &format!("idle{i}"), None, false));
    }
    let mut view = view_with_agents(agents);
    assert_eq!(
        rendered(&view, "idle"),
        7,
        "1 attention + 7 idle fills the cap"
    );
    assert_eq!(idle_fold(&view), Some((5, false)), "folded: +5 idle");

    view.toggle_idle(footnote_key());
    assert_eq!(rendered(&view, "idle"), 12, "expanded shows every idle row");
    assert_eq!(idle_fold(&view), Some((5, true)), "the - fewer affordance");

    view.toggle_idle(footnote_key());
    assert_eq!(rendered(&view, "idle"), 7, "toggling again re-folds");
    assert_eq!(idle_fold(&view), Some((5, false)));
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC11-HP: Right on a hovered or selected workspace row toggles its caret;
// `l` stays the explicit expand and never toggles.
#[tokio::test]
async fn right_arrow_toggles_a_workspace_caret() {
    let dir = isolate_view_store("right-caret");
    let mut agents = vec![sv_agent(1, "w", Some(AgentBadge::Working), false)];
    for i in 0..12 {
        agents.push(sv_agent(1, &format!("idle{i}"), None, false));
    }
    let mut view = view_with_agents(agents);
    view.selector = Some(0); // the workspace header row
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut view, &[0x1b, b'[', b'C'], &mut buf)
        .await
        .unwrap();
    assert_eq!(rendered(&view, "idle"), 12, "Right opens the caret");
    selector_keys(&mut view, &[0x1b, b'[', b'C'], &mut buf)
        .await
        .unwrap();
    assert_eq!(rendered(&view, "idle"), 7, "Right again re-folds it");

    // `l` remains the EXPLICIT section expand (x-975a) and never touches
    // the idle caret: two presses leave the fold alone, state Expanded.
    selector_keys(&mut view, b"l", &mut buf).await.unwrap();
    selector_keys(&mut view, b"l", &mut buf).await.unwrap();
    assert_eq!(
        rendered(&view, "idle"),
        7,
        "`l` does not fold or unfold idle rows"
    );
    assert_eq!(
        view.section_view.get(&footnote_key()),
        Some(&SectionView::Expanded),
        "`l` sets the explicit Expanded view"
    );

    // An agent row REACHES, it does not toggle (x-9fd0): Right takes the same
    // row_action path Enter takes, so a live paneless row goes through portal
    // 0 instead of the old "only a workspace row has a caret" refusal - that
    // refusal was the dead-keybind shape, a losing case that only beeped.
    view.selector = Some(1); // an agent row
    buf.clear();
    selector_keys(&mut view, &[0x1b, b'[', b'C'], &mut buf)
        .await
        .unwrap();
    assert_eq!(view.selector, None, "Right on an agent row reaches");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert_eq!(placement.portal_target(), Some(0));
        }
        other => panic!("expected the portal reach, got {other:?}"),
    }
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC2-FR (x-c5ee): the selected idle agent is never folded. The selector
// only ever rests on a RENDERED row (folded rows are absent from the list
// navigation walks), so a selected idle agent is within budget and unfolded
// by construction - no per-frame force-expand needed.
#[test]
fn selected_idle_agent_is_never_folded() {
    let dir = isolate_view_store("cap-sel");
    let mut agents = vec![sv_agent(1, "w", Some(AgentBadge::Working), false)];
    for i in 0..12 {
        agents.push(sv_agent(1, &format!("idle{i}"), None, false));
    }
    let mut view = view_with_agents(agents);
    // Capped order (budget 8 - 1 = 7): Sel(0), w(1), idle0(2)..idle6(8),
    // IdleFold(9). The selector can only land on a rendered row, so idle0 at
    // index 2 is within budget and therefore never folded.
    view.selector = Some(2);
    assert!(
        matches!(view.display_rows().get(2), Some(DisplayRow::Agent(a)) if a.name == "idle0"),
        "the selector rests on a rendered idle row (idle0), never a folded one"
    );
    assert_eq!(
        rendered(&view, "idle"),
        7,
        "the within-budget idle rows render"
    );
    assert_eq!(
        idle_fold(&view),
        Some((5, false)),
        "the overflow still folds; reaching it is the fold row's toggle"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression (x-c5ee, codex P1/P2 on #566/#568): the render is a pure
// function of state, so resting the selector ON the `+N more` fold row never
// perturbs it. The earlier per-frame force-expand resolved the selector index
// against a rebuilt enumeration, which mis-identified the row (fold moved /
// Enter reported no action) and could collapse a walked-into overflow row.
#[test]
fn selector_on_fold_row_leaves_it_actionable_and_in_place() {
    let dir = isolate_view_store("cap-foldsel");
    let agents: Vec<AgentRow> = (0..12)
        .map(|i| sv_agent(1, &format!("idle{i}"), None, false))
        .collect();
    let mut view = view_with_agents(agents);
    // Budget 8, 12 idle -> 8 shown, IdleFold at index 9 (Sel(0), idle0..7 at
    // 1..8, fold at 9).
    let fold_i = view
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::IdleFold { .. }))
        .expect("a fold row");
    view.selector = Some(fold_i);
    // The fold row stays put (not replaced by a mis-protected agent)...
    assert!(
        matches!(
            view.display_rows().get(fold_i),
            Some(DisplayRow::IdleFold { .. })
        ),
        "the fold row is still at the selector index"
    );
    // ...still shows +4 idle (protection did not force-expand the squad)...
    assert_eq!(
        idle_fold(&view),
        Some((4, false)),
        "fold unchanged, still folded"
    );
    // ...and Enter on it toggles idle rather than reporting no action.
    assert!(matches!(
        view.row_action(fold_i),
        Some(ChromeHit::ToggleIdle(SectionKey::Squad(_)))
    ));
    // Toggling it open reveals the whole idle roster (persisted), and it is
    // stable across re-render - the intended way to walk the overflow.
    view.toggle_idle(SectionKey::Squad("/code/footnote".into()));
    assert_eq!(rendered(&view, "idle"), 12, "toggling reveals all idle");
    assert_eq!(
        rendered(&view, "idle"),
        12,
        "and it is stable across re-render"
    );
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// AC1-UI (x-c5ee): a click / selector Enter on the fold row toggles idle -
// it is actionable, not inert.
#[test]
fn idle_fold_row_action_toggles_idle() {
    let dir = isolate_view_store("cap-action");
    let agents: Vec<AgentRow> = (0..12)
        .map(|i| sv_agent(1, &format!("idle{i}"), None, false))
        .collect();
    let view = view_with_agents(agents);
    let i = view
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::IdleFold { .. }))
        .expect("a fold row");
    assert!(matches!(
        view.row_action(i),
        Some(ChromeHit::ToggleIdle(SectionKey::Squad(_)))
    ));
    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// A manual collapse of a mission squad must survive later ticks the same
// way a real squad's does - insert-only, not force-reopened.
#[test]
fn manual_collapse_of_mission_squad_persists_across_ticks() {
    let mission_layout = |active| {
        let mut l = two_squad_layout(active);
        l.squads.push(mission_meta(7, "mux-squad  0/3"));
        l
    };
    let mid = mission_meta(7, "mux-squad  0/3").id;
    let mut view = two_pane_view();
    view.set_layout(mission_layout(1));
    view.cycle_squad(mid);
    assert!(view.squad_view(mid) == SectionView::Collapsed);
    view.set_layout(mission_layout(1));
    assert!(
        view.squad_view(mid) == SectionView::Collapsed,
        "an already-known mission must not re-seed on every tick"
    );
}

// A mission squad can never hold an agent (its id is a high-bit sentinel
// nothing is assigned), so it renders as a progress line under the
// `~ missions` band rather than a workspace section an operator would expect
// to hold sessions. The band header cycles locally; no per-mission row ever
// reaches SelectSquad (a mission has no server-side squad to select).
#[test]
fn mission_renders_under_the_missions_band_not_as_a_workspace() {
    let mut view = two_pane_view();
    let mut layout = two_squad_layout(1);
    layout.squads.push(mission_meta(3, "mux-squad  2/2"));
    view.set_layout(layout);
    let rows = view.display_rows();
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, DisplayRow::Sel(row) if is_mission_squad(row.squad))),
        "a mission must not render as a workspace section"
    );
    let band = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { label, .. } if *label == "~ missions"))
        .expect("a `~ missions` band");
    assert!(
        rows.iter()
            .any(|r| matches!(r, DisplayRow::Sub(s) if s == "mux-squad  2/2")),
        "the mission's name renders as a band line"
    );
    assert!(
        matches!(
            view.row_action(band),
            Some(ChromeHit::CycleSection(SectionKey::Missions))
        ),
        "the missions band cycles locally, never SelectSquad"
    );
}

#[test]
fn section_toggles_hide_the_missions_and_backlog_bands() {
    // config.mux.show_missions / show_backlog (default on) drop the bands
    // entirely - an operator who runs no epics hides the empty missions band
    // rather than collapsing it each session.
    let mut view = two_pane_view();
    let mut layout = two_squad_layout(1);
    layout.squads.push(mission_meta(3, "epic  0/4"));
    layout.backlog = vec![bcard("x-rdy", CardState::Ready)];
    layout.backlog_lanes = vec![("ready".into(), 1)];
    view.set_layout(layout);
    view.section_view
        .insert(SectionKey::WorkQueue, SectionView::Expanded);
    let has = |v: &View, lbl: &str| {
        v.display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Header { label, .. } if *label == lbl))
    };
    assert!(has(&view, "~ missions"), "missions band shown by default");
    assert!(has(&view, "~ backlog"), "backlog band shown by default");
    view.show_missions = false;
    view.show_backlog = false;
    assert!(
        !has(&view, "~ missions"),
        "show_missions=false hides the band"
    );
    assert!(
        !has(&view, "~ backlog"),
        "show_backlog=false hides the band"
    );
}

// AC3-HP: acting on the active squad row cycles locally - with no dead
// rows the cycle is binary, so two clicks round-trip - and apply_hit's
// CycleSection arm does no I/O (AC1-FR is structural: cycle_section never
// touches the socket).
#[test]
fn cycle_section_round_trips_without_dead_rows() {
    let mut view = two_pane_view();
    assert!(matches!(
        view.row_action(0),
        Some(ChromeHit::CycleSection(SectionKey::Squad(_)))
    ));
    view.cycle_squad(1);
    assert!(
        view.squad_view(1) == SectionView::Collapsed,
        "first toggle collapses"
    );
    // Collapsed, the active row still resolves to the toggle (rows are
    // now [sq1, sq2, footer]).
    assert!(matches!(
        view.row_action(0),
        Some(ChromeHit::CycleSection(SectionKey::Squad(_)))
    ));
    view.cycle_squad(1);
    assert!(
        view.squad_view(1) == SectionView::Expanded,
        "second toggle re-expands"
    );
}

// AC1-UI: exactly one squad row carries the `*` glyph - the active one -
// in both its expanded and collapsed states.
#[test]
fn client_compose_active_squad_glyph_in_both_caret_states() {
    let mut view = two_pane_view();
    let text = frame_text(&view.compose());
    assert!(text.contains("▾*footnote"), "expanded active carries *");
    assert!(text.contains("▸ notes"), "inactive carries no *");
    view.cycle_squad(1);
    let text = frame_text(&view.compose());
    assert!(text.contains("▸*footnote"), "collapsed active keeps *");
}

// (x-975a) A squad row with interleaved live/exited agents, for the
// tri-state filtering tests below.
fn view_with_dead_interleaved() -> View {
    let row = |name: &str, exited: bool| AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: name.into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
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
    view_with_agents(vec![
        row("live-a", false),
        row("dead-a", true),
        row("live-b", false),
        row("dead-b", true),
    ])
}

fn agent_names(view: &View) -> Vec<String> {
    view.display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.clone()),
            _ => None,
        })
        .collect()
}

// AC4-UI: with exited agents interleaved, click 1 hides exactly the exited
// rows in place (live order preserved), click 2 collapses all, click 3
// restores Expanded.
#[test]
fn cycle_section_tri_state_filters_then_collapses_then_restores() {
    let mut view = view_with_dead_interleaved();
    assert_eq!(
        agent_names(&view),
        vec!["live-a", "dead-a", "live-b", "dead-b"],
        "expanded shows every row"
    );

    view.cycle_squad(1);
    assert_eq!(view.squad_view(1), SectionView::LiveOnly);
    assert_eq!(
        agent_names(&view),
        vec!["live-a", "live-b"],
        "live-only hides exactly the exited rows, order preserved"
    );

    view.cycle_squad(1);
    assert_eq!(view.squad_view(1), SectionView::Collapsed);
    assert!(agent_names(&view).is_empty(), "collapsed hides every row");

    view.cycle_squad(1);
    assert_eq!(view.squad_view(1), SectionView::Expanded);
    assert_eq!(
        agent_names(&view).len(),
        4,
        "cycle restores the full section"
    );
}

// AC5-EDGE: a squad with no exited rows skips LiveOnly entirely - the
// middle state would hide nothing and read as a dead click.
#[test]
fn cycle_section_skips_live_only_when_no_row_is_dead() {
    let mut view = two_pane_view();
    assert_eq!(view.squad_view(1), SectionView::Expanded);
    view.cycle_squad(1);
    assert_eq!(
        view.squad_view(1),
        SectionView::Collapsed,
        "straight to collapsed"
    );
}

// AC12-FR: a section left in LiveOnly whose last exited agent is reaped
// elsewhere paints no `✗` count and advances to Collapsed on the next
// click - it can never wedge in a state that now hides nothing.
#[test]
fn live_only_advances_after_dead_rows_disappear() {
    let mut view = view_with_dead_interleaved();
    view.cycle_squad(1);
    assert_eq!(view.squad_view(1), SectionView::LiveOnly);

    // The reap lands as a plain layout push with the exited rows gone.
    view.layout.agents.retain(|a| !a.exited);
    assert!(
        !frame_text(&view.compose()).contains('✗'),
        "no dead rows left, so no ✗ count"
    );
    view.cycle_squad(1);
    assert_eq!(
        view.squad_view(1),
        SectionView::Collapsed,
        "no stuck LiveOnly"
    );
}

// The caret discriminates all three states - hollow `▿` for live-only
// against filled `▾` for expanded, so the middle state is never
// indistinguishable from the full one.
#[test]
fn caret_glyph_distinguishes_all_three_view_states() {
    let mut view = view_with_dead_interleaved();
    assert!(frame_text(&view.compose()).contains("▾*footnote"));
    view.cycle_squad(1);
    assert!(
        frame_text(&view.compose()).contains("▿*footnote"),
        "live-only carries the hollow caret"
    );
    view.cycle_squad(1);
    assert!(frame_text(&view.compose()).contains("▸*footnote"));
}

// The Backlog section is binary in both directions: a card has no exited state,
// so LiveOnly would be meaningless there.
#[test]
fn work_queue_section_is_binary_and_hides_cards_when_collapsed() {
    let mut view = two_pane_view();
    view.layout.backlog = vec![BacklogCard {
        id: "x-0001".into(),
        slug: "a-card".into(),
        state: CardState::Ready,
        priority: "p2".into(),
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    }];
    let cards = |v: &View| {
        v.display_rows()
            .iter()
            .filter(|r| matches!(r, DisplayRow::Card(_)))
            .count()
    };
    // (x-c5ee) The queue now defaults Collapsed (AC2-HP): no cards until the
    // operator expands it.
    assert_eq!(
        view.section_view(&SectionKey::WorkQueue),
        SectionView::Collapsed,
        "queue defaults collapsed"
    );
    assert_eq!(cards(&view), 0, "collapsed by default hides the cards");

    // Binary cycle: Collapsed -> Expanded, never through LiveOnly (a card
    // has no exited state).
    view.cycle_section(SectionKey::WorkQueue);
    assert_eq!(
        view.section_view(&SectionKey::WorkQueue),
        SectionView::Expanded
    );
    assert_eq!(cards(&view), 1, "expanded shows the card");

    view.cycle_section(SectionKey::WorkQueue);
    assert_eq!(
        view.section_view(&SectionKey::WorkQueue),
        SectionView::Collapsed,
        "binary: straight back to collapsed, never LiveOnly"
    );
    assert_eq!(cards(&view), 0, "collapsed hides the cards");
}

// A `~` section header is CLICKABLE (it cycles its own view) but stays
// inert to the selector cursor - the x-260a "cursor never rests on a
// label" invariant is preserved, not widened.
#[test]
fn section_header_is_clickable_but_never_selector_selectable() {
    let view = view_with_agents(vec![AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(99), // no such squad -> orphan -> `~ elsewhere`
        name: "stray".into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    }]);
    let hdr = view
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { key, .. } if *key == SectionKey::Elsewhere))
        .expect("elsewhere header present");
    assert!(matches!(
        view.row_action(hdr),
        Some(ChromeHit::CycleSection(SectionKey::Elsewhere))
    ));
    assert!(
        row_is_inert(&view.display_rows()[hdr]),
        "still inert: the selector cursor skips it"
    );
    assert_ne!(view.selector_anchor(hdr), Some(hdr), "cursor steps off it");
}

// A persisted state wins over the active-squad seed on a fresh attach, and
// an operator cycle writes back - the restart-survival contract.
#[test]
fn persisted_section_state_survives_a_fresh_view() {
    let dir = std::env::temp_dir().join(format!("fno-view-client-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::view_store::set_test_path(&dir);

    // A cycle on the active squad persists.
    let mut view = view_with_dead_interleaved();
    view.cycle_squad(1);
    assert_eq!(view.squad_view(1), SectionView::LiveOnly);

    // A fresh attach restores it INSTEAD of re-seeding the active squad
    // expanded, and a squad absent from this layout is pruned away.
    let restored = two_pane_view();
    assert_eq!(
        restored.squad_view(1),
        SectionView::LiveOnly,
        "persisted state beats the active-squad seed"
    );
    assert!(
        crate::view_store::load().keys().all(|k| matches!(
            k,
            SectionKey::Squad(cwd) if cwd == "/code/footnote" || cwd == "/code/notes"
        )),
        "only live squads persist, keyed by canonical cwd"
    );

    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// The production attach path: `View::new` against an EMPTY placeholder
// layout, then the server's first push. Persisted state has to survive
// BOTH - the earlier version pruned in `View::new` (deleting everything
// against the placeholder) and then re-seeded the active squad expanded,
// so persistence never worked in production while a test that built the
// View with a populated layout still passed.
#[test]
fn persisted_state_survives_the_real_attach_path() {
    let dir = std::env::temp_dir().join(format!("fno-view-attach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::view_store::set_test_path(&dir);

    let mid = crate::proto::MISSION_SQUAD_BASE | 3;
    let mut saved = HashMap::new();
    saved.insert(
        SectionKey::Squad("/code/footnote".into()),
        SectionView::Collapsed,
    );
    saved.insert(SectionKey::Mission(mid), SectionView::Collapsed);
    crate::view_store::save(&saved);

    // Exactly what `attach_and_run` builds: no squads, active_squad 0.
    let mut view = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    // The server's first real push.
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1), mission_meta(3, "epic  0/4")],
        active_squad: 1,
        panes: Vec::new(),
        focus: 0,
        area: (28, 72),
        agents: Vec::new(),
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });

    assert_eq!(
        view.squad_view(1),
        SectionView::Collapsed,
        "a persisted collapse must survive attach, not be re-seeded expanded"
    );
    assert_eq!(
        view.squad_view(mid),
        SectionView::Collapsed,
        "the same for a mission header, which seeds on first appearance"
    );

    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// Only an explicit operator choice is persisted. A seeded default is
// recomputed on every attach, and writing it would let this build re-seed
// over a value a NEWER build wrote and this one could not parse.
#[test]
fn seeded_defaults_are_not_persisted_only_operator_choices() {
    let dir = std::env::temp_dir().join(format!("fno-view-chosen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::view_store::set_test_path(&dir);

    // two_pane_view seeds squad 1 expanded; nothing was chosen.
    let mut view = two_pane_view();
    assert_eq!(view.squad_view(1), SectionView::Expanded);
    assert!(
        crate::view_store::load().is_empty(),
        "a seed alone must not reach disk"
    );

    // An operator gesture does persist, and ONLY the key it touched.
    view.cycle_squad(2);
    let saved = crate::view_store::load();
    assert_eq!(saved.len(), 1, "only the chosen key persists: {saved:?}");
    assert!(saved.contains_key(&SectionKey::Squad("/code/notes".into())));

    crate::view_store::clear_test_path();
    let _ = std::fs::remove_dir_all(&dir);
}

// A genuine mid-session activation still expands (x-2f99 preserved under
// x-c5ee): squad 2 sits at its inactive-default Collapsed with no explicit
// choice, and activating it flips the computed default to Expanded. No
// `set_squad_view` here - that would write an EXPLICIT choice, which now
// outranks activation (Locked 2), a different case covered elsewhere.
#[test]
fn later_activation_still_expands_a_collapsed_squad() {
    let mut view = two_pane_view();
    assert_eq!(
        view.squad_view(2),
        SectionView::Collapsed,
        "squad 2 starts at the inactive default"
    );
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
        active_squad: 2,
        panes: view.layout.panes.clone(),
        focus: view.layout.focus,
        area: (28, 72),
        agents: Vec::new(),
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert_eq!(
        view.squad_view(2),
        SectionView::Expanded,
        "activating a squad mid-session expands it"
    );
}

// `squad_matches` is the allocation-free twin of `section_key`; if they
// ever disagree, pruning would silently drop live sections.
#[test]
fn section_key_matches_resolver() {
    let mut plain = meta(1, "footnote", 1, 0);
    let mission = mission_meta(9, "epic  1/2");
    let mut cwdless = meta(2, "nameonly", 1, 0);
    cwdless.canonical_cwd = String::new();
    plain.canonical_cwd = "/code/footnote".into();

    for s in [&plain, &mission, &cwdless] {
        assert!(
            squad_matches(s, &section_key(s)),
            "squad_matches must accept its own section_key: {:?}",
            s.name
        );
    }
    // ...and reject a foreign one.
    assert!(!squad_matches(&plain, &section_key(&mission)));
    assert!(!squad_matches(&mission, &section_key(&plain)));
    assert!(!squad_matches(&plain, &SectionKey::Elsewhere));
}

// The regression the name key would have caused: a mission header's NAME
// carries its live done/total counters, so keying on it meant an expanded
// mission silently collapsed the moment one of its nodes finished.
#[test]
fn mission_section_state_survives_a_progress_tick() {
    let mut view = two_pane_view();
    let mid = crate::proto::MISSION_SQUAD_BASE | 7;
    let panes = view.layout.panes.clone();
    let layout = |name: &str| LayoutView {
        squads: vec![meta(1, "footnote", 2, 1), mission_meta(7, name)],
        active_squad: 1,
        panes: panes.clone(),
        focus: 10,
        area: (28, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    };
    view.set_layout(layout("epic  1/5"));
    assert_eq!(
        view.squad_view(mid),
        SectionView::Expanded,
        "a new mission seeds expanded"
    );

    // A worker finishes: same mission, same stable id, brand-new NAME.
    view.set_layout(layout("epic  2/5"));
    assert_eq!(
        view.squad_view(mid),
        SectionView::Expanded,
        "progress must not collapse the mission out from under the operator"
    );

    // And a deliberate collapse still survives the next tick.
    view.cycle_squad(mid);
    assert_eq!(view.squad_view(mid), SectionView::Collapsed);
    view.set_layout(layout("epic  3/5"));
    assert_eq!(
        view.squad_view(mid),
        SectionView::Collapsed,
        "the operator's choice outlives the rename"
    );
}

// Two squads whose DERIVED labels collide (display_names disambiguates only
// one level, so /a/x/foo and /b/x/foo both render as `x/foo`) must not
// share one view state.
#[test]
fn same_named_squads_keep_separate_view_state() {
    let mut view = two_pane_view();
    let mut a = meta(1, "x/foo", 1, 0);
    a.canonical_cwd = "/a/x/foo".into();
    let mut b = meta(2, "x/foo", 1, 0);
    b.canonical_cwd = "/b/x/foo".into();
    let panes = view.layout.panes.clone();
    view.set_layout(LayoutView {
        squads: vec![a, b],
        active_squad: 1,
        panes,
        focus: 10,
        area: (28, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view.set_squad_view(1, SectionView::Expanded);
    view.set_squad_view(2, SectionView::Collapsed);
    assert_eq!(view.squad_view(1), SectionView::Expanded);
    assert_eq!(
        view.squad_view(2),
        SectionView::Collapsed,
        "a shared rendered name must not conflate two workspaces"
    );
}

// The `~ elsewhere` filter is a second copy of the squad filter, so it
// needs its own coverage - drift between the two would be silent.
#[test]
fn elsewhere_section_live_only_hides_exited_orphans() {
    let orphan = |name: &str, exited: bool| AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(99), // no such squad -> orphan
        name: name.into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
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
    let mut view = view_with_agents(vec![
        orphan("stray-live", false),
        orphan("stray-dead", true),
    ]);
    view.expand_pull_sections(); // (x-c5ee) ~ elsewhere now defaults Collapsed
    assert_eq!(agent_names(&view), vec!["stray-live", "stray-dead"]);

    view.cycle_section(SectionKey::Elsewhere);
    assert_eq!(
        view.section_view(&SectionKey::Elsewhere),
        SectionView::LiveOnly
    );
    assert_eq!(
        agent_names(&view),
        vec!["stray-live"],
        "live-only hides the exited orphan"
    );
    assert!(
        frame_text(&view.compose()).contains('✗'),
        "the header keeps the ✗ count so the hidden row stays discoverable"
    );
}

// A `~` header's caret is a SEPARATE render path from the squad row's, so
// it needs its own frame assertion.
#[test]
fn section_header_caret_tracks_all_three_states() {
    let orphan = |name: &str, exited: bool| AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(99),
        name: name.into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
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
    let mut view = view_with_agents(vec![orphan("a", false), orphan("b", true)]);
    view.expand_pull_sections(); // (x-c5ee) ~ elsewhere now defaults Collapsed
    assert!(frame_text(&view.compose()).contains("▾~ elsewhere"));
    view.cycle_section(SectionKey::Elsewhere);
    assert!(frame_text(&view.compose()).contains("▿~ elsewhere"));
    view.cycle_section(SectionKey::Elsewhere);
    assert!(frame_text(&view.compose()).contains("▸~ elsewhere"));
}

// The selector's explicit `l`/`h` pair was rewritten onto the new state
// enum; `l` must OPEN a live-only section all the way, not just one step.
#[tokio::test]
async fn selector_l_and_h_set_explicit_view_states() {
    let mut v = view_with_dead_interleaved();
    let mut buf: Vec<u8> = Vec::new();
    v.selector = Some(0); // the active squad's name row
    v.set_squad_view(1, SectionView::LiveOnly);

    selector_keys(&mut v, b"l", &mut buf).await.unwrap();
    assert_eq!(
        v.squad_view(1),
        SectionView::Expanded,
        "l opens fully from live-only, never one step of the cycle"
    );
    v.selector = Some(0);
    selector_keys(&mut v, b"h", &mut buf).await.unwrap();
    assert_eq!(v.squad_view(1), SectionView::Collapsed);
    v.selector = Some(0);
    selector_keys(&mut v, b"l", &mut buf).await.unwrap();
    assert_eq!(v.squad_view(1), SectionView::Expanded);
}

// AC2-EDGE: a zero-tab active squad expands to a bare `▾` caret - no tab
// rows, no panic.
#[test]
fn client_compose_zero_tab_active_squad() {
    let view = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: vec![meta(1, "empty", 0, 0), meta(2, "notes", 1, 0)],
            active_squad: 1,
            panes: vec![],
            focus: 0,
            area: (28, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    let text = frame_text(&view.compose());
    let lines: Vec<&str> = text.lines().collect();
    // (x-cd67 US1 owns row 0; US3 Blank spacer at line 1): squad 1 leads
    // line 0, the spacer is line 1, squad 2 follows on line 2.
    assert!(lines[0].contains("▾*empty"), "{:?}", lines[0]);
    assert!(lines[2].contains("▸ notes"), "no tab rows in between");
}

// AC2-UI: the status row names the active squad iff more than one squad
// exists (the sideline-hidden answer to "which squad?").
#[test]
fn client_compose_status_row_squad_cell_multi_squad_only() {
    let view = two_pane_view();
    let text = frame_text(&view.compose());
    let bottom = text.lines().last().unwrap();
    assert!(
        bottom.starts_with(" main │ footnote │ /code/footnote"),
        "{bottom:?}"
    );
    // A single squad has nothing to disambiguate: the cell is absent.
    let mut view = two_pane_view();
    let mut layout = two_squad_layout(1);
    layout.squads.remove(1);
    view.set_layout(layout);
    let text = frame_text(&view.compose());
    let bottom = text.lines().last().unwrap();
    assert!(bottom.starts_with(" main │ /code/footnote"), "{bottom:?}");
}

// A pane-hosted agent row focuses its pane; a watch-only row (no pane in this
// session) can only surface a hint.
#[test]
fn chrome_hit_agent_rows_focus_or_hint() {
    let hosted = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "worker".into(),
        pane_id: Some(10),
        portal: None,
        badge: Some(AgentBadge::Working),
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    // A watch-only bg row with a claude jobId: a click reaches the
    // dedicated thread pane (x-07c2); a row with no attach id reaches
    // BY NAME (Follow/Locate tiers).
    let bg_attach = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Drive,
        spawned_by_session: None,
        harness_session_id: None,
        squad: None,
        name: "bg-claude".into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        answerable: None,
        attach_id: Some("c19cd2c3".into()),
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
    // A watch-only row with no attach target: its reach opens the
    // dedicated pane by name (Follow tails it, Locate explains it).
    let bg_plain = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Follow,
        spawned_by_session: None,
        harness_session_id: None,
        squad: None,
        name: "bg-other".into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    let mut view = view_with_agents(vec![hosted, bg_attach, bg_plain]);
    view.expand_pull_sections(); // (x-c5ee) ~ elsewhere now defaults Collapsed
                                 // Agents-first display order (x-0090; no tab rows) with x-cd67 US1
                                 // (sideline owns row 0, terminal row == display index) + Blank spacers:
                                 // squad 1 (0), "worker" (1), Blank (2), squad 2 (3), Blank footer spacer
                                 // (4), "+ new workspace" footer (5), Blank (6), "~ elsewhere" header (7),
                                 // orphan "bg-claude" (8), orphan "bg-other" (9).
    assert_eq!(cmds(view.chrome_hit(1, 4)), vec![Command::FocusPane(10)]);
    // (x-07c2) Both watch-only rows now REACH the dedicated thread pane:
    // the attachable one by attach id, the other by name.
    for (row, want_id) in [(8usize, "c19cd2c3"), (9, "bg-other")] {
        let row = row.try_into().unwrap();
        match view.chrome_hit(row, 4) {
            Some(ChromeHit::Cmds(c)) => assert!(
                matches!(
                    c.as_slice(),
                    [Command::AttachAgent { id, placement }]
                        if id == want_id && placement.portal_target() == Some(0)
                ),
                "row {row} must reach portal 0, got {c:?}"
            ),
            other => panic!(
                "row {row}: expected a thread reach, got {}",
                chrome_hit_label(&other)
            ),
        }
    }
    // (x-975a) The "~ elsewhere" header cycles its own section view. It
    // stays `row_is_inert` (the selector cursor still skips it) - clickable
    // is not selectable.
    assert!(matches!(
        view.chrome_hit(7, 4),
        Some(ChromeHit::CycleSection(SectionKey::Elsewhere))
    ));
    // The "+ new workspace" footer opens the create overlay.
    assert!(matches!(view.chrome_hit(5, 4), Some(ChromeHit::OpenCreate)));
}

// A click on the bottom row belongs to the status/which-key/search chrome
// painted over it, never the sideline row drawn underneath (codex P2).
#[test]
fn chrome_hit_bottom_chrome_row_is_swallowed() {
    // Enough agents that display_rows() reaches the last terminal row.
    // (x-c5ee) Working, not idle: attention rows are never folded by the
    // top-K cap, so all 40 render and the list still reaches the bottom.
    let agents: Vec<AgentRow> = (0..40)
        .map(|i| AgentRow {
            harness: None,
            model: None,
            route: None,
            reach: Reach::Locate,
            spawned_by_session: None,
            harness_session_id: None,
            squad: Some(1),
            name: format!("a{i}"),
            pane_id: Some(100 + i),
            portal: None,
            badge: Some(AgentBadge::Working),
            reason: None,
            exited: false,
            dnd: false,
            unmeasured: false,
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
        })
        .collect();
    let view = view_with_agents(agents);
    let bottom = view.term.0 - 1; // last terminal row
    assert!(view.bottom_row_is_chrome(), "status row on by default");
    assert!(
        view.display_rows().len() > (bottom - TAB_BAR_ROWS) as usize,
        "sideline is long enough to underlie the bottom row"
    );
    // The row under the cursor maps to a real display row, yet the click is
    // swallowed because the bottom row is chrome.
    assert!(view.chrome_hit(bottom, 4).is_none());
    // With the status row toggled off, that same row is a live sideline hit.
    let mut view = view;
    view.status_on = false;
    assert!(!view.bottom_row_is_chrome());
    assert!(view.chrome_hit(bottom, 4).is_some());
}

#[test]
fn client_compose_draws_scroll_indicator_when_pane_scrolled() {
    // AC1-UI: a `[+N]` indicator appears at a scrolled pane's top-right;
    // absent entirely when the pane is live (offset 0).
    let mut view = two_pane_view();
    assert!(!frame_text(&view.compose())
        .lines()
        .nth(1)
        .unwrap()
        .contains("[+"));
    let mut f = text_frame(29, 35, 'a');
    f.scroll_offset = 7;
    view.frames.insert(10, f);
    let frame = view.compose();
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    let row1: Vec<char> = lines[1].chars().collect();
    // start_c = origin_c(28) + rect.x(0) + rect.cols(35) - width("[+7]"=4).
    let seg: String = row1[59..63].iter().collect();
    assert_eq!(seg, "[+7]");
}

#[test]
fn client_compose_status_row_shows_session_cwd_and_help() {
    // US4 AC4-UI: bottom row carries session name, active squad cwd, and
    // the `? for keys` affordance; the focused pane's scroll offset joins
    // it when non-zero (the canonical `[+N]` home).
    let mut view = two_pane_view();
    let text = frame_text(&view.compose());
    let bottom = text.lines().last().unwrap().to_string();
    assert!(bottom.contains("main"), "{bottom:?}");
    assert!(bottom.contains("/code/footnote"), "{bottom:?}");
    assert!(bottom.contains("? for keys"), "{bottom:?}");
    assert!(!bottom.contains("[+"), "no stale indicator: {bottom:?}");
    // The row is blanked first, so no divider glyphs bleed through the
    // gaps between segments.
    assert!(!bottom.contains('\u{2500}'), "no '─' bleed: {bottom:?}");
    assert!(!bottom.contains('\u{253c}'), "no '┼' bleed: {bottom:?}");
    // Focused pane (11) scrolled -> [+N] in the status row.
    let mut f = text_frame(29, 36, 'b');
    f.scroll_offset = 3;
    view.frames.insert(11, f);
    let text = frame_text(&view.compose());
    assert!(text.lines().last().unwrap().contains("[+3]"));
}

#[test]
fn client_status_row_shows_focus_node_provenance() {
    // x-66e8 AC (happy): a node-driven focused pane -> `⚑ <node>` cell.
    let mut view = two_pane_view();
    view.layout.focus_node = Some("x-66e8".into());
    let bottom = frame_text(&view.compose())
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert!(bottom.contains("⚑ x-66e8"), "provenance cell: {bottom:?}");
    // AC (edge): an ad-hoc pane (no node) shows no provenance cell.
    view.layout.focus_node = None;
    let bottom = frame_text(&view.compose())
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert!(!bottom.contains('⚑'), "no cell for ad-hoc pane: {bottom:?}");
    // AC (edge): the which-key hint still fully overrides the row.
    view.layout.focus_node = Some("x-66e8".into());
    view.hint = true;
    let bottom = frame_text(&view.compose())
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert!(bottom.contains("hjkl focus"), "hint takeover: {bottom:?}");
    assert!(!bottom.contains('⚑'), "hint hides the cell: {bottom:?}");
}

#[test]
fn client_status_row_accounting_and_auto_hide() {
    // AC4-ERR + the Domain Pitfall: the content area the server sees
    // shrinks by exactly the status row, and a too-short terminal
    // recovers the line (geometry beats the toggle).
    let mut view = two_pane_view();
    assert_eq!(view.content_dims(), (28, 72), "tab bar + status row");
    view.status_on = false;
    assert_eq!(view.content_dims(), (29, 72), "toggled off");
    view.status_on = true;
    view.term = (MIN_ROWS_FOR_STATUS - 1, 100);
    assert!(!view.status_visible(), "auto-hidden below min height");
    assert_eq!(view.content_dims(), (MIN_ROWS_FOR_STATUS - 2, 72));
    // And the bottom row is NOT painted over content when hidden.
    let text = frame_text(&view.compose());
    assert!(!text.lines().last().unwrap().contains("? for keys"));
}

#[test]
fn client_status_off_leaves_bottom_row_as_content() {
    // codex P2: with the status row toggled off and no hint pending, the
    // bottom row belongs to content (content_dims gave the server the full
    // height) - draw_bottom_row must NOT blank it. The fixture's panes are
    // 29 rows tall from y=0, so pane content reaches the last terminal row.
    let mut view = two_pane_view();
    view.status_on = false;
    let text = frame_text(&view.compose());
    let bottom = text.lines().last().unwrap().to_string();
    assert!(
        bottom.contains('a') || bottom.contains('b'),
        "bottom row must keep pane content when status is off: {bottom:?}"
    );
    // A pending hint still transiently paints over that content row.
    view.hint = true;
    let text = frame_text(&view.compose());
    assert!(text.lines().last().unwrap().contains("hjkl focus"));
}

#[test]
fn client_compose_hint_paints_over_bottom_row() {
    // AC4-HP: the which-key hint lists live chords on the bottom row,
    // replacing the status content while a chord is pending - even with
    // the status row toggled off (discoverability survives the toggle).
    let mut view = two_pane_view();
    view.hint = true;
    let text = frame_text(&view.compose());
    let bottom = text.lines().last().unwrap().to_string();
    assert!(bottom.contains("hjkl focus"), "{bottom:?}");
    assert!(!bottom.contains("? for keys"), "{bottom:?}");
    view.status_on = false;
    let text = frame_text(&view.compose());
    assert!(text.lines().last().unwrap().contains("hjkl focus"));
}

#[test]
fn client_compose_keys_modal_renders_the_which_key_reference() {
    // x-8ccf US3: prefix+? opens the centered which-key modal (replacing the
    // top-left poster) built from the single-source binding table.
    let mut view = two_pane_view();
    view.term = (40, 80);
    view.open_keys_modal();
    let text = frame_text(&view.compose());
    assert!(text.contains("keybinds"), "modal title present");
    assert!(text.contains("esc close"), "dismiss affordance present");
    // Section headers + a sampling of bindings the table advertises.
    assert!(text.contains("panes"), "section header");
    assert!(text.contains("detach"), "the d binding's action");
    assert!(
        text.contains("find: goto squad/tab/pane/agent"),
        "the f binding's action names every row class nav_rows emits"
    );
    // (x-cf97) The digit row names the gesture, its resolve doors, and the
    // Alt form. The number jump is no longer capped at nine, so the old
    // "first 9; f goes past" ceiling would now be the lie; what must stay
    // is the honest description of an input path the scanner really runs.
    assert!(
        text.contains("jump to tab by number")
            && text.contains("Enter")
            && text.contains("Alt works too"),
        "the digit row names the gesture and its resolve doors"
    );
}

#[test]
fn client_keys_modal_execute_selected_maps_selected_row_to_its_chord() {
    // The default selection is the first binding; row_events[selected] must
    // be exactly the Event a direct chord of that key would produce (Locked
    // 3 parity, at the modal boundary).
    let m = super::build_keys_modal();
    let (ri, _) = m.popup.selected().expect("a selectable row");
    let ev = m.row_events[ri].clone().expect("first row is executable");
    // The first section is Global; its first binding is `w` -> OpenSelector.
    assert_eq!(ev, crate::keys::resolve_chord(b'w'));
}

#[tokio::test]
async fn keys_modal_which_key_executes_a_bound_key_to_the_wire() {
    // AC2-HP: tapping a bound key in the modal runs it immediately through
    // the SAME dispatch a direct chord uses, and the modal closes.
    let mut v = two_pane_view();
    v.term = (40, 80);
    v.open_keys_modal();
    let mut buf: Vec<u8> = Vec::new();
    keys_modal_keys(&mut v, &mut Scanner::default(), b"%", &mut buf)
        .await
        .unwrap();
    assert!(v.keys_modal.is_none(), "executing a chord closes the modal");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::SplitH) => {}
        other => panic!("expected SplitH from `%`, got {other:?}"),
    }
}

#[tokio::test]
async fn keys_modal_unbound_key_and_esc_dismiss_without_acting() {
    // AC2-EDGE: an unbound key dismisses and NO action fires. Esc is FOLDED
    // (carried across reads like every overlay, codex P2) so it resolves once
    // a following byte disambiguates it from a split arrow.
    let mut v = two_pane_view();
    v.term = (40, 80);
    let mut buf: Vec<u8> = Vec::new();
    v.open_keys_modal();
    keys_modal_keys(&mut v, &mut Scanner::default(), b"Z", &mut buf)
        .await
        .unwrap();
    assert!(v.keys_modal.is_none(), "unbound key dismisses");
    assert!(buf.is_empty(), "unbound key sends nothing");
    // A lone Esc is carried (no leak); the next byte flushes it as a dismiss.
    v.open_keys_modal();
    keys_modal_keys(&mut v, &mut Scanner::default(), b"\x1b", &mut buf)
        .await
        .unwrap();
    assert!(
        v.keys_modal.is_some(),
        "a lone Esc is carried, not acted on"
    );
    keys_modal_keys(&mut v, &mut Scanner::default(), b"z", &mut buf)
        .await
        .unwrap();
    assert!(
        v.keys_modal.is_none(),
        "the carried Esc dismisses on the next key"
    );
    assert!(buf.is_empty(), "Esc sends nothing to a pane");
}

#[tokio::test]
async fn keys_modal_esc_click_and_escape_both_close() {
    use crate::mouse::MouseReport;
    let mut v = two_pane_view();
    v.term = (30, 100);
    v.open_keys_modal();
    let (row, col) = {
        let rendered = v.keys_modal.as_ref().unwrap().popup.render(v.term);
        let (line, hits) = rendered
            .lines
            .iter()
            .enumerate()
            .find(|(_, line)| {
                line.hits
                    .iter()
                    .any(|(tag, _, _)| *tag == crate::chrome::ESC_CLOSE_HIT)
            })
            .expect("which-key modal exposes a clickable esc target");
        let (offset, len) = hits
            .hits
            .iter()
            .find(|(tag, _, _)| *tag == crate::chrome::ESC_CLOSE_HIT)
            .map(|(_, offset, len)| (*offset, *len))
            .unwrap();
        (
            rendered.origin.0 + line,
            rendered.origin.1 + offset + len / 2,
        )
    };
    let click = MouseReport {
        row: row as u16,
        col: col as u16,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    let mut buf = Vec::new();
    keys_modal_mouse(&mut v, &mut Scanner::default(), click, &mut buf)
        .await
        .unwrap();
    assert!(v.keys_modal.is_none());
    assert!(buf.is_empty());

    v.open_keys_modal();
    keys_modal_keys(&mut v, &mut Scanner::default(), b"\x1b", &mut buf)
        .await
        .unwrap();
    keys_modal_keys(&mut v, &mut Scanner::default(), b"z", &mut buf)
        .await
        .unwrap();
    assert!(v.keys_modal.is_none());
    assert!(buf.is_empty());
}

#[tokio::test]
async fn keys_modal_wheel_scrolls_and_click_off_dismisses() {
    use crate::mouse::MouseReport;
    let mut v = two_pane_view();
    v.term = (8, 80); // short: the binding list overflows and scrolls
    v.open_keys_modal();
    let mut buf: Vec<u8> = Vec::new();
    let wheel = MouseReport {
        row: 4,
        col: 40,
        kind: MouseKind::WheelDown,
        shift: false,
    };
    keys_modal_mouse(&mut v, &mut Scanner::default(), wheel, &mut buf)
        .await
        .unwrap();
    assert_eq!(
        v.keys_modal.as_ref().unwrap().popup.scroll,
        3,
        "wheel scrolls"
    );
    // A left click off the popup (top-left corner) dismisses.
    let click = MouseReport {
        row: 0,
        col: 0,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    keys_modal_mouse(&mut v, &mut Scanner::default(), click, &mut buf)
        .await
        .unwrap();
    assert!(v.keys_modal.is_none(), "click off the popup dismisses");
}

#[tokio::test]
async fn clicking_the_footer_esc_close_dismisses_the_modal() {
    // AC10-HP: the chrome footer's `esc close` words are a mouse target
    // stamped by chrome::frame, so clicking them closes the modal without
    // touching a key. Verified THROUGH the mouse router (chrome_close_hit
    // feeding aux_mouse) on the settings modal, whose footer reads
    // `tab switches section · esc close`, on the real rendered geometry.
    use crate::mouse::MouseReport;
    let mut v = two_pane_view();
    v.term = (30, 100);
    v.aux = Some(v.build_settings_modal());
    // Where do the words sit on screen? Render exactly as the router does.
    // (x-020d) The title bar's chip now ALSO carries an ESC_CLOSE_HIT (a
    // 3-char span); pick the footer's specifically by its longer span so
    // this stays a test of the footer words, not whichever comes first.
    let (fr, fc) = {
        let r = v.aux.as_ref().unwrap().popup.render(v.term);
        let (li, row) = r
            .lines
            .iter()
            .enumerate()
            .find(|(_, l)| {
                l.hits
                    .iter()
                    .any(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len > 3)
            })
            .expect("the modal footer carries the close target");
        let (off, len) = row
            .hits
            .iter()
            .find(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len > 3)
            .map(|(_, o, l)| (*o, *l))
            .unwrap();
        (r.origin.0 + li, r.origin.1 + off + len / 2)
    };
    let mut buf: Vec<u8> = Vec::new();
    let click = MouseReport {
        row: fr as u16,
        col: fc as u16,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    aux_mouse(&mut v, click, &mut buf).await.unwrap();
    assert!(v.aux.is_none(), "clicking `esc close` closes");
    assert!(buf.is_empty(), "the close sends nothing on the wire");

    // Esc still closes: the click added a target, it did not move the key.
    v.aux = Some(v.build_settings_modal());
    v.aux_esc = vec![0x1b];
    aux_keys(&mut v, b"z", &mut buf).await.unwrap();
    assert!(v.aux.is_none(), "Esc still closes the modal");
}

#[tokio::test]
async fn clicking_the_title_bar_esc_chip_dismisses_the_modal() {
    // (x-020d) The title bar's ` esc ` chip was decorative chrome; it is
    // now the same kind of mouse target the footer's `esc close` words
    // already were. Verified through the real mouse router, same as the
    // footer's own test above.
    use crate::mouse::MouseReport;
    let mut v = two_pane_view();
    v.term = (30, 100);
    v.aux = Some(v.build_settings_modal());
    let (fr, fc) = {
        let r = v.aux.as_ref().unwrap().popup.render(v.term);
        let (li, row) = r
            .lines
            .iter()
            .enumerate()
            .find(|(_, l)| {
                l.hits
                    .iter()
                    .any(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len == 3)
            })
            .expect("the title bar carries the close target");
        let (off, len) = row
            .hits
            .iter()
            .find(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len == 3)
            .map(|(_, o, l)| (*o, *l))
            .unwrap();
        (r.origin.0 + li, r.origin.1 + off + len / 2)
    };
    let mut buf: Vec<u8> = Vec::new();
    let click = MouseReport {
        row: fr as u16,
        col: fc as u16,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    aux_mouse(&mut v, click, &mut buf).await.unwrap();
    assert!(v.aux.is_none(), "clicking the title bar's esc chip closes");
    assert!(buf.is_empty(), "the close sends nothing on the wire");
}

#[tokio::test]
async fn clicking_a_row_menus_bare_bottom_border_chip_dismisses_it() {
    // (x-020d) A row/tab menu wears Bare chrome (Anchor::At), whose esc
    // chip rides the inline bottom border rather than a title bar. Same
    // click target, different chrome level - verified through the real
    // row_menu_mouse router, same as the Full title-bar chip above.
    use crate::mouse::MouseReport;
    let mut v = view_with_agents(vec![agent_row("a", 10, Some(AgentBadge::Working), false)]);
    assert!(v.open_row_menu(1, Anchor::At { row: 1, col: 1 }));
    let (fr, fc) = {
        let r = v.row_menu.as_ref().unwrap().popup.render(v.term);
        let (li, row) = r
            .lines
            .iter()
            .enumerate()
            .find(|(_, l)| {
                l.hits
                    .iter()
                    .any(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len == 3)
            })
            .expect("the Bare bottom border carries the close target");
        let (off, len) = row
            .hits
            .iter()
            .find(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len == 3)
            .map(|(_, o, l)| (*o, *l))
            .unwrap();
        (r.origin.0 + li, r.origin.1 + off + len / 2)
    };
    let mut buf: Vec<u8> = Vec::new();
    let click = MouseReport {
        row: fr as u16,
        col: fc as u16,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    row_menu_mouse(&mut v, click, &mut buf).await.unwrap();
    assert!(
        v.row_menu.is_none(),
        "clicking the Bare menu's bottom-border chip closes it"
    );
}

#[tokio::test]
async fn hovering_the_footer_esc_close_keeps_the_selection() {
    // The footer's close target must never surface through `aux_hit` as a
    // row index: ESC_CLOSE_HIT clamps in `Popup::select` to the LAST entry,
    // so a hover sweep over the words would silently re-target Enter.
    use crate::mouse::MouseReport;
    let mut v = two_pane_view();
    v.term = (30, 100);
    v.aux = Some(v.build_settings_modal());
    let n = v.aux.as_ref().unwrap().popup.targets().len();
    v.aux.as_mut().unwrap().popup.select(0);
    let sel_before = v.aux.as_ref().unwrap().popup.sel;
    // (x-020d) Same disambiguation as the click test above: pick the
    // footer's close span specifically, not the title bar chip's.
    let (fr, fc) = {
        let r = v.aux.as_ref().unwrap().popup.render(v.term);
        let (li, row) = r
            .lines
            .iter()
            .enumerate()
            .find(|(_, l)| {
                l.hits
                    .iter()
                    .any(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len > 3)
            })
            .expect("the modal footer carries the close target");
        let (off, len) = row
            .hits
            .iter()
            .find(|(t, _, len)| *t == crate::chrome::ESC_CLOSE_HIT && *len > 3)
            .map(|(_, o, l)| (*o, *l))
            .unwrap();
        (r.origin.0 + li, r.origin.1 + off + len / 2)
    };
    let hover = MouseReport {
        row: fr as u16,
        col: fc as u16,
        kind: MouseKind::Move,
        shift: false,
    };
    let mut buf: Vec<u8> = Vec::new();
    aux_mouse(&mut v, hover, &mut buf).await.unwrap();
    assert!(
        n < 2 || v.aux.as_ref().unwrap().popup.sel == sel_before,
        "hover over the close words keeps the selection put ({} targets)",
        n
    );
}

#[test]
fn row_menu_entries_gate_by_agent_state() {
    // US2: no dead item - a bg row gets new-tab + the 2x2 split grid; a pane
    // row gets focus and NO splits (already placed); an exited row gets
    // remove and no stop.
    let mk = |name: &str, pane_id: Option<u64>, attach: Option<&str>, exited: bool| AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: None,
        name: name.into(),
        pane_id,
        badge: None,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
        answerable: None,
        attach_id: attach.map(Into::into),
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
    let bg = super::build_row_menu(&mk("bg", None, Some("id"), false), Anchor::Center);
    assert!(bg.actions.contains(&super::MenuAction::NewTab));
    assert!(bg.actions.contains(&super::MenuAction::Split(Dir::Right)));
    assert!(bg.actions.contains(&super::MenuAction::Split(Dir::Up)));
    assert!(bg.actions.contains(&super::MenuAction::Stop));
    assert!(!bg.actions.contains(&super::MenuAction::Focus));
    // AC1-UI (x-9f75): Open Here is present and leads above New Tab.
    let open_here = bg
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::OpenHere);
    let new_tab = bg
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::NewTab);
    assert!(
        matches!((open_here, new_tab), (Some(o), Some(n)) if o < n),
        "Open Here sits above New Tab"
    );
    let pane = super::build_row_menu(&mk("p", Some(9), None, false), Anchor::Center);
    assert!(pane.actions.contains(&super::MenuAction::Focus));
    assert!(
        !pane.actions.contains(&super::MenuAction::OpenHere),
        "a placed pane row offers no open-here"
    );
    assert!(
        !pane
            .actions
            .iter()
            .any(|a| matches!(a, super::MenuAction::Split(_))),
        "a placed pane row offers no splits"
    );
    assert!(
        !pane.actions.contains(&super::MenuAction::NewTab),
        "a placed pane row breaks its pane out, it never re-attaches"
    );
    // Re-placement IS offered on a pane row - as a move of the live pane.
    for d in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
        assert!(
            pane.actions.contains(&super::MenuAction::MoveDir(d)),
            "pane row offers Move {d:?}"
        );
    }
    assert!(
        menu_labels(&pane)
            .iter()
            .any(|label| label == "Detach pane"),
        "a live pane row offers row-scoped detach"
    );
    assert!(pane.actions.contains(&super::MenuAction::BreakOut));
    let dead = super::build_row_menu(&mk("d", None, None, true), Anchor::Center);
    assert!(dead.actions.contains(&super::MenuAction::Remove));
    assert!(!dead.actions.contains(&super::MenuAction::Stop));
}

#[tokio::test]
async fn row_menu_bg_split_right_attaches_to_current_route() {
    // AC1-HP: "Split Right" on a bg row sends AttachAgent placing it as a
    // right split of the current tab - an existing command, zero proto bump.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    assert!(v.open_row_menu(idx, Anchor::Center));
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::Split(Dir::Right))
        .unwrap();
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "executing closes the menu");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert_eq!(placement.split, Some(Dir::Right));
            assert!(matches!(
                placement.target,
                crate::proto::PaneTarget::CurrentRoute
            ));
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn row_menu_open_here_sends_here_placement() {
    // AC1-UI (x-9f75): "Open Here" on a bg row sends AttachAgent with here:true and the default
    // (CurrentRoute, no split) placement; the menu closes.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    assert!(v.open_row_menu(idx, Anchor::Center));
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::OpenHere)
        .unwrap();
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "executing closes the menu");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert!(placement.here, "open-here sets here:true");
            assert!(placement.split.is_none());
            assert!(matches!(
                placement.target,
                crate::proto::PaneTarget::CurrentRoute
            ));
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn row_menu_stale_target_notices_without_acting() {
    // AC1-ERR: the target racing out between open and execute becomes a
    // Notice, and nothing goes on the wire.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    v.open_row_menu(idx, Anchor::Center);
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::Split(Dir::Right))
        .unwrap();
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    v.layout.agents.retain(|a| a.name != "bg-claude"); // it vanishes
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a stale target sends nothing");
    assert!(v.notice.is_some(), "and surfaces a notice");
}

#[test]
fn row_menu_opens_only_on_menu_bearing_rows() {
    // A workspace header is always menu-bearing now (US3: it offers Rename),
    // even in `unified_rows_view`, which has no dead rows to clear.
    let mut v = unified_rows_view();
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    assert_eq!(
        v.row_menu.as_ref().unwrap().actions,
        vec![
            super::MenuAction::Rename,
            super::MenuAction::MoveSquad(-1),
            super::MenuAction::MoveSquad(1),
            super::MenuAction::RemoveSquad,
        ]
    );
    v.row_menu = None;
    // A truly menu-less row (the dim subline) refuses with no notice at all.
    // A FOREIGN cwd is what makes display_rows emit the Sub line, so the
    // fixture has to opt in - `.expect` rather than `if let`, so a fixture
    // that stops producing one fails loudly instead of skipping the check.
    v.layout.agents[0].cwd_base = Some("elsewhere".into());
    let sub = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Sub(_)))
        .expect("a foreign-cwd agent renders a Sub row");
    v.notice = None;
    assert!(!v.open_row_menu(sub, Anchor::Center));
    assert!(v.notice.is_none(), "an inert row says nothing");
}

#[test]
fn row_menu_opens_on_pane_hosted_agent_row() {
    // Operator report: "right-click does nothing on most rows; it works only
    // on a row not on a pane yet." A pane-hosted session renders as a
    // DisplayRow::Agent with pane_id: Some (x-0090 moved these off the old
    // Sel-with-tab rows), so the one path a right-click reaches is
    // open_row_menu -> build_row_menu. This goes THROUGH open_row_menu on that
    // exact row - not build_row_menu directly - so it pins the pane-row
    // affordance (Focus/BreakOut/Move/Stop) on the path a user has, the case
    // the direct-builder tests never covered.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "worker" && a.pane_id.is_some());
    assert!(
        v.open_row_menu(idx, Anchor::Center),
        "pane-hosted row opens a menu"
    );
    let actions = &v.row_menu.as_ref().unwrap().actions;
    assert!(
        actions.contains(&MenuAction::Focus),
        "pane row offers Focus"
    );
    assert!(
        actions.contains(&MenuAction::BreakOut),
        "pane row offers BreakOut"
    );
    assert!(actions.contains(&MenuAction::Stop), "pane row offers Stop");
    assert!(
        actions.iter().any(|a| matches!(a, MenuAction::MoveDir(_))),
        "pane row offers the Move grid"
    );
    // The paneless-only attach verbs must not appear on a pane-hosted row.
    assert!(!actions.contains(&MenuAction::OpenHere));
    assert!(!actions.contains(&MenuAction::NewTab));
}

/// The display index of the squad-name header row for `squad`.
fn squad_header_at(view: &View, squad: u64) -> usize {
    view.display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Sel(s) if s.squad == squad && s.tab.is_none()))
        .expect("squad header row")
}

/// Every command one Enter put on the wire, in order.
fn decode_cmds(buf: Vec<u8>) -> Vec<Command> {
    let len = buf.len() as u64;
    let mut cur = std::io::Cursor::new(buf);
    let mut out = Vec::new();
    while cur.position() < len {
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(c) => out.push(c),
            other => panic!("expected a Command, got {other:?}"),
        }
    }
    out
}

/// Open the section menu on a squad header and run its only entry.
async fn arm_clear_dead(v: &mut View, squad: u64) {
    let hdr = squad_header_at(v, squad);
    assert!(v.open_row_menu(hdr, Anchor::Center), "section menu opens");
    // Rename now leads the workspace menu, so explicitly select Clear dead.
    let m = v.row_menu.as_mut().unwrap();
    m.popup.sel = m
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::ClearDead)
        .expect("clear-dead entry present");
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "the menu entry only arms the confirm");
}

#[tokio::test]
async fn clear_dead_removes_every_dead_row_in_the_section() {
    // (x-f300) The header menu's clear-dead sends one Remove per exited row
    // and leaves every live row alone.
    let mut v = view_with_dead_interleaved();
    arm_clear_dead(&mut v, 1).await;
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::ClearDead { dead, .. }) => assert_eq!(*dead, 2),
        _ => panic!("expected a ClearDead confirm"),
    }
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![
            Command::RemoveAgent {
                name: "dead-a".into()
            },
            Command::RemoveAgent {
                name: "dead-b".into()
            },
        ],
        "only the exited rows are removed"
    );
}

#[test]
fn nonworkspace_section_with_no_dead_rows_gets_a_notice() {
    // The menu never renders a no-op entry: a NON-workspace band (Elsewhere)
    // with nothing to clear and nothing to rename gets a notice, not a
    // one-entry menu (AC-EDGE). A workspace section always opens (Rename).
    let orphan_live = {
        let mut r = lifecycle_row("stray-live", false, false);
        r.squad = Some(99); // no such squad -> Elsewhere band
        r
    };
    let mut v = view_with_agents(vec![orphan_live]);
    let hdr = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { key, .. } if *key == SectionKey::Elsewhere))
        .expect("elsewhere band");
    assert!(!v.open_row_menu(hdr, Anchor::Center));
    assert!(v.row_menu.is_none());
    assert!(v.notice.is_some(), "and says why");
}

#[tokio::test]
async fn workspace_section_menu_offers_rename() {
    // US3: a workspace section header offers Rename (menu parity with
    // selector `r`), even with no dead rows to clear.
    let mut v = view_with_agents(vec![]);
    v.layout.agents = vec![lifecycle_row("live-a", false, false)];
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center), "workspace menu opens");
    assert_eq!(
        v.row_menu.as_ref().unwrap().actions,
        vec![
            super::MenuAction::Rename,
            super::MenuAction::MoveSquad(-1),
            super::MenuAction::MoveSquad(1),
            super::MenuAction::RemoveSquad
        ],
        "no dead rows -> the five standing workspace verbs"
    );
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "opening the overlay sends nothing");
    assert_eq!(
        v.rename.map(|(t, _)| t),
        Some(RenameTarget::Squad(1)),
        "opens the rename overlay for this workspace"
    );
}

#[test]
fn workspace_section_menu_offers_rename_then_clear_dead() {
    // With dead rows present the workspace menu offers BOTH, Rename first.
    let mut v = view_with_dead_interleaved();
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    assert_eq!(
        v.row_menu.as_ref().unwrap().actions,
        vec![
            super::MenuAction::Rename,
            super::MenuAction::MoveSquad(-1),
            super::MenuAction::MoveSquad(1),
            super::MenuAction::RemoveSquad,
            super::MenuAction::ClearDead
        ]
    );
}

#[tokio::test]
async fn workspace_section_menu_move_sends_the_reorder_command() {
    // AC8-HP: Move up/down ride the same Command::MoveSquad the keyboard
    // J/K path sends; the server's silent clamp covers the at-edge case
    // (AC9-EDGE), so the client sends unconditionally and never bells.
    let mut v = view_with_agents(vec![]);
    v.layout.agents = vec![lifecycle_row("live-a", false, false)];
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    for (delta, label) in [(-1, "up"), (1, "down")] {
        let m = v.row_menu.as_mut().unwrap();
        m.popup.sel = m
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::MoveSquad(delta))
            .unwrap_or_else(|| panic!("move-{label} entry present"));
        let mut buf: Vec<u8> = Vec::new();
        row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
        assert_eq!(
            decode_cmds(buf),
            vec![Command::MoveSquad { squad: 1, delta }],
            "move {label} sends the reorder command"
        );
        assert!(v.open_row_menu(hdr, Anchor::Center), "re-open for the next");
    }
}

#[tokio::test]
async fn workspace_section_menu_remove_opens_the_confirm_not_the_command() {
    // AC8-HP: Remove workspace routes through the SAME
    // ConfirmKind::RemoveSquad confirm the keyboard path builds - a mouse
    // click must not skip the destructive-action gate.
    let mut v = view_with_agents(vec![]);
    v.layout.agents = vec![lifecycle_row("live-a", false, false)];
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    let m = v.row_menu.as_mut().unwrap();
    m.popup.sel = m
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::RemoveSquad)
        .unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "the entry arms the confirm, sends nothing");
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::RemoveSquad { squad, .. }) => assert_eq!(*squad, 1),
        _ => panic!("expected a RemoveSquad confirm"),
    }
}

#[test]
fn name_entry_prompt_renders_centered_naming_its_target() {
    // The create/rename/recruit name inputs used to paint the bottom-left
    // chrome row (plain BOLD, outside the operator's field of view). They now
    // render as a centered modal that names the target.
    //
    // (x-b465) The target moved into the chrome TITLE and the name is the
    // body, so this scans the whole block rather than one line - the modal
    // wears the shared frame now and its parts sit on different rows.
    let mut v = two_pane_view();
    v.rename = Some((RenameTarget::Squad(1), "renamed".into()));
    let (rows, cols) = (v.term.0 as usize, v.term.1 as usize);
    let mut cells = vec![Cell::default(); rows * cols];
    v.draw_bottom_row(&mut cells, rows, cols);
    let screen: String = (0..rows)
        .map(|r| {
            cells[r * cols..(r + 1) * cols]
                .iter()
                .map(|c| c.c)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("rename workspace"),
        "the modal names its target: {screen}"
    );
    assert!(
        screen.contains("renamed_"),
        "shows the in-progress name + cursor: {screen}"
    );
    assert!(
        cells.iter().any(|c| c.flags & cell_flags::INVERSE != 0),
        "the modal inverts the theme pair, not the old plain bottom row"
    );
    let bottom: String = cells[(rows - 1) * cols..rows * cols]
        .iter()
        .map(|c| c.c)
        .collect();
    assert!(
        !bottom.contains("rename"),
        "the prompt left the bottom row it used to share"
    );
}

#[test]
fn every_confirm_variant_renders_shared_chrome_and_controls() {
    let variants = vec![
        (ConfirmKind::Dispatch { node: "x-1".into() }, "dispatch"),
        (
            ConfirmKind::RemoveSquad {
                squad: 1,
                panes: 2,
                last: false,
            },
            "remove squad",
        ),
        (
            ConfirmKind::StopAgent {
                name: "agent".into(),
            },
            "stop agent",
        ),
        (
            ConfirmKind::RemoveAgent {
                name: "agent".into(),
            },
            "remove agent",
        ),
        (ConfirmKind::ReapAgents, "reap"),
        (
            ConfirmKind::StopExternal {
                attach_id: "a-1".into(),
                name: "external".into(),
            },
            "stop external",
        ),
        (
            ConfirmKind::RemoveExternal {
                attach_id: "a-1".into(),
                name: "external".into(),
            },
            "remove external",
        ),
        (
            ConfirmKind::DismissMember {
                squad: 1,
                attach_id: "a-1".into(),
            },
            "dismiss member",
        ),
        (
            ConfirmKind::ClearDead {
                key: crate::view_store::SectionKey::Missions,
                squad: None,
                dead: 3,
            },
            "clear dead",
        ),
        (ConfirmKind::CloseTab { tab: 1 }, "close tab"),
    ];

    for (action, label) in variants {
        let mut view = two_pane_view();
        view.confirm = Some(ConfirmAction {
            action,
            label: label.into(),
        });
        let (rows, cols) = (view.term.0 as usize, view.term.1 as usize);
        let mut cells = vec![Cell::default(); rows * cols];
        view.draw_bottom_row(&mut cells, rows, cols);
        let screen: String = (0..rows)
            .map(|r| {
                cells[r * cols..(r + 1) * cols]
                    .iter()
                    .map(|cell| cell.c)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains('┌'),
            "{label} has no shared top border: {screen}"
        );
        assert!(
            screen.contains("enter confirm · esc cancel"),
            "{label} has no actual controls footer: {screen}"
        );
    }
}

#[test]
fn modal_esc_chips_cancel_name_and_confirm_states_without_outside_dismissal() {
    let close_cell = |view: &View| {
        let layout = view
            .active_overlay_layout()
            .expect("an active modal has a family-B layout");
        let (line, offset, len) = layout
            .framed
            .lines
            .iter()
            .enumerate()
            .find_map(|(line, row)| {
                row.hits
                    .iter()
                    .find(|(target, _, _)| *target == crate::chrome::ESC_CLOSE_HIT)
                    .map(|(_, offset, len)| (line, *offset, *len))
            })
            .expect("the modal exposes a shared esc chip");
        (
            (layout.origin.0 + line) as u16,
            (layout.origin.1 + offset + len / 2) as u16,
        )
    };
    let click = |view: &mut View| {
        let (row, col) = close_cell(view);
        let press = crate::mouse::MouseReport {
            row,
            col,
            kind: MouseKind::Press(MouseButton::Left),
            shift: false,
        };
        assert!(modal_mouse(view, press));
        assert!(modal_mouse(
            view,
            crate::mouse::MouseReport {
                kind: MouseKind::Release(MouseButton::Left),
                ..press
            },
        ));
    };

    let mut view = two_pane_view();
    view.open_create();
    click(&mut view);
    assert!(
        view.create.is_none(),
        "create closes from the shared esc chip"
    );

    view.open_rename(RenameTarget::Tab(1));
    click(&mut view);
    assert!(
        view.rename.is_none(),
        "rename closes from the shared esc chip"
    );

    view.marks.insert("a-1".into());
    view.open_recruit();
    click(&mut view);
    assert!(
        view.recruit.is_none(),
        "recruit closes from the shared esc chip"
    );
    assert!(view.marks.contains("a-1"), "recruit cancel keeps its marks");

    view.confirm = Some(ConfirmAction {
        action: ConfirmKind::Dispatch { node: "x-1".into() },
        label: "dispatch".into(),
    });
    assert!(modal_mouse(
        &mut view,
        crate::mouse::MouseReport {
            row: 0,
            col: 0,
            kind: MouseKind::Press(MouseButton::Left),
            shift: false,
        },
    ));
    assert!(
        view.confirm.is_some(),
        "an outside click is swallowed without dismissing the confirm"
    );
    click(&mut view);
    assert!(
        view.confirm.is_none(),
        "confirm closes from the shared esc chip"
    );
}

#[test]
fn esc_chip_close_swallows_its_matching_left_release() {
    let mut view = two_pane_view();
    view.open_create();
    let layout = view.active_overlay_layout().expect("create layout");
    let (line, offset, len) = layout
        .framed
        .lines
        .iter()
        .enumerate()
        .find_map(|(line, row)| {
            row.hits
                .iter()
                .find(|(target, _, _)| *target == crate::chrome::ESC_CLOSE_HIT)
                .map(|(_, offset, len)| (line, *offset, *len))
        })
        .expect("create exposes an esc chip");
    let click = crate::mouse::MouseReport {
        row: (layout.origin.0 + line) as u16,
        col: (layout.origin.1 + offset + len / 2) as u16,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    assert!(modal_mouse(&mut view, click));
    assert!(view.create.is_none(), "the chip press closes the modal");
    assert!(
        modal_mouse(
            &mut view,
            crate::mouse::MouseReport {
                kind: MouseKind::Release(MouseButton::Left),
                ..click
            },
        ),
        "the release paired with the closing click stays swallowed"
    );
    assert!(
        !modal_mouse(
            &mut view,
            crate::mouse::MouseReport {
                kind: MouseKind::Release(MouseButton::Left),
                ..click
            },
        ),
        "only the matching release is consumed"
    );
}

#[test]
fn name_modal_clears_the_reserved_bottom_row() {
    let mut view = two_pane_view();
    view.rename = Some((RenameTarget::Tab(1), "typed".into()));
    let (rows, cols) = (view.term.0 as usize, view.term.1 as usize);
    let mut cells = vec![Cell::default(); rows * cols];
    for cell in &mut cells[(rows - 1) * cols..rows * cols] {
        *cell = Cell {
            c: 'x',
            ..Cell::default()
        };
    }

    view.draw_bottom_row(&mut cells, rows, cols);

    assert!(
        cells[(rows - 1) * cols..rows * cols]
            .iter()
            .all(|cell| *cell == Cell::default()),
        "the reserved bottom row is blank beneath a name modal"
    );
}

#[tokio::test]
async fn shifted_release_after_esc_chip_close_is_consumed_before_prefilter() {
    let mut view = two_pane_view();
    view.open_create();
    let layout = view.active_overlay_layout().expect("create layout");
    let (line, offset, len) = layout
        .framed
        .lines
        .iter()
        .enumerate()
        .find_map(|(line, row)| {
            row.hits
                .iter()
                .find(|(target, _, _)| *target == crate::chrome::ESC_CLOSE_HIT)
                .map(|(_, offset, len)| (line, *offset, *len))
        })
        .expect("create exposes an esc chip");
    let row = (layout.origin.0 + line) as u16;
    let col = (layout.origin.1 + offset + len / 2) as u16;
    assert!(modal_mouse(
        &mut view,
        crate::mouse::MouseReport {
            row,
            col,
            kind: MouseKind::Press(MouseButton::Left),
            shift: false,
        },
    ));

    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf = Vec::new();
    let shifted_release = format!("\x1b[<4;{};{}m", col + 1, row + 1);
    handle_stdin(
        &mut view,
        &mut scanner,
        &mut carry,
        shifted_release.as_bytes(),
        &mut buf,
    )
    .await
    .unwrap();

    assert!(
        !view.modal_release_swallow,
        "the shifted release clears the latch"
    );
    assert!(buf.is_empty(), "the shifted release never reaches the pane");
}

#[test]
fn esc_chip_close_swallows_drag_until_left_release() {
    let mut view = two_pane_view();
    view.open_create();
    let layout = view.active_overlay_layout().expect("create layout");
    let (line, offset, len) = layout
        .framed
        .lines
        .iter()
        .enumerate()
        .find_map(|(line, row)| {
            row.hits
                .iter()
                .find(|(target, _, _)| *target == crate::chrome::ESC_CLOSE_HIT)
                .map(|(_, offset, len)| (line, *offset, *len))
        })
        .expect("create exposes an esc chip");
    let click = crate::mouse::MouseReport {
        row: (layout.origin.0 + line) as u16,
        col: (layout.origin.1 + offset + len / 2) as u16,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    assert!(modal_mouse(&mut view, click));
    assert!(modal_mouse(
        &mut view,
        crate::mouse::MouseReport {
            kind: MouseKind::Drag(MouseButton::Left),
            ..click
        },
    ));
    assert!(
        view.modal_release_swallow,
        "drag keeps the closing gesture armed"
    );
    assert!(modal_mouse(
        &mut view,
        crate::mouse::MouseReport {
            kind: MouseKind::Release(MouseButton::Left),
            ..click
        },
    ));
    assert!(
        !view.modal_release_swallow,
        "left release ends the closing gesture"
    );
}

#[tokio::test]
async fn close_latch_consumes_release_before_an_intervening_modal_router() {
    let mut view = two_pane_view();
    view.open_create();
    let layout = view.active_overlay_layout().expect("create layout");
    let (line, offset, len) = layout
        .framed
        .lines
        .iter()
        .enumerate()
        .find_map(|(line, row)| {
            row.hits
                .iter()
                .find(|(target, _, _)| *target == crate::chrome::ESC_CLOSE_HIT)
                .map(|(_, offset, len)| (line, *offset, *len))
        })
        .expect("create exposes an esc chip");
    let row = (layout.origin.0 + line) as u16;
    let col = (layout.origin.1 + offset + len / 2) as u16;
    assert!(modal_mouse(
        &mut view,
        crate::mouse::MouseReport {
            row,
            col,
            kind: MouseKind::Press(MouseButton::Left),
            shift: false,
        },
    ));
    view.open_keys_modal();
    assert!(
        view.modal_release_swallow,
        "the close gesture remains armed"
    );

    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf = Vec::new();
    let release = format!("\x1b[<0;{};{}m", col + 1, row + 1);
    handle_stdin(
        &mut view,
        &mut scanner,
        &mut carry,
        release.as_bytes(),
        &mut buf,
    )
    .await
    .unwrap();

    assert!(
        !view.modal_release_swallow,
        "the release clears the latch first"
    );
    assert!(
        view.keys_modal.is_some(),
        "the release does not dismiss the new modal"
    );
    assert!(buf.is_empty(), "the release never reaches the pane");
}

#[test]
fn confirm_clears_the_reserved_bottom_row_on_fallback() {
    let mut view = two_pane_view();
    view.confirm = Some(ConfirmAction {
        action: ConfirmKind::ReapAgents,
        label: "all agents".into(),
    });
    let (rows, cols) = (view.term.0 as usize, view.term.1 as usize);
    let mut cells = vec![Cell::default(); rows * cols];
    for cell in &mut cells[(rows - 1) * cols..rows * cols] {
        *cell = Cell {
            c: 'x',
            ..Cell::default()
        };
    }

    view.draw_bottom_row(&mut cells, rows, cols);

    assert!(
        cells[(rows - 1) * cols..rows * cols]
            .iter()
            .all(|cell| *cell == Cell::default()),
        "the reserved bottom row is blank beneath a fallback confirm"
    );
}

#[tokio::test]
async fn clear_dead_refolds_the_set_at_commit_not_at_open() {
    // Concurrency: the confirm pins the SECTION, not the row list. A row
    // reaped while the prompt sat open drops out of the commit instead of
    // sending a Remove for something already gone.
    let mut v = view_with_dead_interleaved();
    arm_clear_dead(&mut v, 1).await;
    v.layout.agents.retain(|a| a.name != "dead-a");
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::RemoveAgent {
            name: "dead-b".into()
        }],
        "the vanished row is not re-removed"
    );
}

#[tokio::test]
async fn clear_dead_routes_external_rows_by_attach_id() {
    // An external tombstone removes by its stable attach_id (x-7561), the
    // same split the single-row `x` verb makes - clear-dead must not flatten
    // every row to a by-name RemoveAgent.
    let mut ext = lifecycle_row("ext-dead", true, true);
    ext.attach_id = Some("deadbeef".into());
    let mut v = view_with_agents(vec![lifecycle_row("plain-dead", true, false), ext]);
    arm_clear_dead(&mut v, 1).await;
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![
            Command::RemoveAgent {
                name: "plain-dead".into()
            },
            Command::RemoveExternal {
                attach_id: "deadbeef".into(),
                name: "ext-dead".into()
            },
        ]
    );
}

#[tokio::test]
async fn clear_dead_on_an_emptied_section_sends_nothing() {
    // AC-ERR: every dead row vanishing between arm and Enter is a notice,
    // not an empty-but-silent commit.
    let mut v = view_with_dead_interleaved();
    arm_clear_dead(&mut v, 1).await;
    v.layout.agents.retain(|a| !a.exited);
    v.notice = None;
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "nothing goes on the wire");
    assert!(v.notice.is_some(), "and the operator is told");
}

#[test]
fn clear_dead_resolves_by_squad_id_when_canonical_paths_collide() {
    // SectionKey::Squad carries the persisted cwd, and two squads may share
    // an origin - so the key alone is ambiguous while the squad id never is.
    // A dead row in EACH squad is what makes this bite: a by-key resolve
    // would hand squad 2's header squad 1's row.
    let mut v = view_with_agents(vec![]);
    v.set_layout(two_squad_layout(1));
    for s in v.layout.squads.iter_mut() {
        s.canonical_cwd = "/shared".into();
    }
    let in_squad = |name: &str, squad: u64| {
        let mut r = lifecycle_row(name, true, false);
        r.squad = Some(squad);
        r
    };
    v.layout.agents = vec![in_squad("dead-in-1", 1), in_squad("dead-in-2", 2)];
    let key = squad_key(&v.layout, 2).expect("squad 2 has a key");
    let names =
        |rows: Vec<&AgentRow>| -> Vec<String> { rows.iter().map(|a| a.name.clone()).collect() };
    assert_eq!(names(v.section_dead_rows(&key, Some(2))), ["dead-in-2"]);
    assert_eq!(names(v.section_dead_rows(&key, Some(1))), ["dead-in-1"]);
    // The display-only caller (cycle_section's has_dead) keeps the by-key
    // lookup: a collision must not cost the section its LiveOnly state.
    assert!(
        !v.section_dead_rows(&key, None).is_empty(),
        "LiveOnly is still offered on a collided section"
    );
}

#[tokio::test]
async fn clear_dead_dismisses_member_tombstones() {
    // A tombstone lives in the squad's member list, not the agent registry,
    // so RemoveAgent would answer "no such agent" and leave the gray row on
    // screen - the exact symptom clear-dead exists to remove.
    let mut tomb = lifecycle_row("cc-member", true, false);
    tomb.tombstone = true;
    tomb.attach_id = Some("deadbeef".into());
    let mut v = view_with_agents(vec![tomb, lifecycle_row("plain-dead", true, false)]);
    arm_clear_dead(&mut v, 1).await;
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![
            Command::DismissMember {
                squad: 1,
                attach_id: "deadbeef".into()
            },
            Command::RemoveAgent {
                name: "plain-dead".into()
            },
        ]
    );
}

#[tokio::test]
async fn row_menu_remove_dismisses_a_member_tombstone() {
    // The single-row path shares `remove_dead`, so it must route a tombstone
    // the same way the bulk clear does.
    let mut tomb = lifecycle_row("cc-member", true, false);
    tomb.tombstone = true;
    tomb.attach_id = Some("deadbeef".into());
    let mut v = view_with_agents(vec![tomb]);
    // (x-c5ee) The squad's only agent is exited, so it would default LiveOnly
    // and hide the dead row; force Expanded to exercise the Remove path.
    v.set_squad_view(1, SectionView::Expanded);
    let idx = agent_row_at(&v, |a| a.name == "cc-member");
    assert!(v.open_row_menu(idx, Anchor::Center));
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::Remove)
        .expect("an exited row offers Remove");
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::DismissMember {
            squad: 1,
            attach_id: "deadbeef".into()
        }]
    );
}

#[tokio::test]
async fn clear_dead_caps_the_fan_out_and_says_what_is_left() {
    // Each row costs the server one `fno agents rm` subprocess, so the fan-out
    // is capped - and the leftover is announced, never silently truncated.
    let over = CLEAR_DEAD_MAX + 3;
    let rows: Vec<AgentRow> = (0..over)
        .map(|i| lifecycle_row(&format!("dead-{i}"), true, false))
        .collect();
    let mut v = view_with_agents(rows);
    arm_clear_dead(&mut v, 1).await;
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(decode_cmds(buf).len(), CLEAR_DEAD_MAX, "the cap holds");
    let notice = v
        .notice
        .as_ref()
        .map(|(t, _)| t.clone())
        .unwrap_or_default();
    assert!(
        notice.contains("3 left"),
        "the remainder is surfaced: {notice}"
    );
}

#[test]
fn backlog_header_has_no_menu_and_stays_silent() {
    // Cards have no exited state, so a notice there would imply "none right
    // now" about a section that can never have any. (SectionKey::WorkQueue
    // is the pre-rename identifier for the Backlog section.)
    let mut v = view_with_agents(vec![]);
    // The band only renders over a non-empty backlog, so the card is what
    // makes this test non-vacuous - `.expect` keeps it that way.
    v.layout.backlog = vec![BacklogCard {
        id: "x-f300".into(),
        slug: "a-card".into(),
        priority: "p2".into(),
        state: CardState::Ready,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    }];
    let hdr = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { key, .. } if *key == SectionKey::WorkQueue))
        .expect("a backlog card renders the Backlog band");
    v.notice = None;
    assert!(!v.open_row_menu(hdr, Anchor::Center));
    assert!(v.notice.is_none(), "the Backlog section says nothing");
}

#[tokio::test]
async fn clear_dead_is_scoped_to_its_own_section() {
    // The load-bearing guarantee of a SECTION clear: a sibling workspace's
    // dead rows are none of its business. Without this, "clear dead" on one
    // squad silently reaps the whole session's tombstones.
    let in_squad = |name: &str, squad: u64| {
        let mut r = lifecycle_row(name, true, false);
        r.squad = Some(squad);
        r
    };
    let mut v = view_with_agents(vec![]);
    v.set_layout(two_squad_layout(1));
    v.layout.agents = vec![in_squad("dead-in-1", 1), in_squad("dead-in-2", 2)];
    arm_clear_dead(&mut v, 1).await;
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::RemoveAgent {
            name: "dead-in-1".into()
        }],
        "the sibling squad's dead row is untouched"
    );
}

#[tokio::test]
async fn clear_dead_works_on_the_elsewhere_band_too() {
    // A `~` band is a DisplayRow::Header, a different branch from a squad's
    // Sel row - drift between the two would leave orphaned dead rows with no
    // bulk path, the exact gap this closes.
    let orphan = |name: &str, exited: bool| {
        let mut r = lifecycle_row(name, exited, false);
        r.squad = Some(99); // no such squad -> orphan
        r
    };
    let mut v = view_with_agents(vec![
        orphan("stray-live", false),
        orphan("stray-dead", true),
    ]);
    let hdr = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { key, .. } if *key == SectionKey::Elsewhere))
        .expect("elsewhere band");
    assert!(v.open_row_menu(hdr, Anchor::Center));
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::RemoveAgent {
            name: "stray-dead".into()
        }]
    );
}

#[test]
fn which_key_lists_the_dead_row_removal_verbs() {
    // (x-f300) The gap this node closed was discoverability: if the modal
    // stops naming these, removal is invisible again.
    let modal = build_keys_modal();
    let labels: Vec<String> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { glyph, label, .. } => Some(format!("{glyph} {label}")),
            PopupRow::Header(h) => Some(h.clone()),
            _ => None,
        })
        .collect();
    let joined = labels.join("\n");
    assert!(joined.contains("sideline rows"), "the section renders");
    assert!(joined.contains("x stop a live row · remove a dead one"));
    assert!(joined.contains("X reap all exited agents"));
    // (x-7683) The context-menu row names every trigger, not just the
    // right-click, and keeps the header-only clear-dead behavior named
    // too - the only in-app documentation that a header's menu offers it.
    assert!(joined.contains("context menu · or m · or hold L 500ms · on a header: clear dead"));
    // Display-only: Enter on them must BEL, never dispatch a bogus chord.
    for (i, r) in modal.popup.rows.iter().enumerate() {
        if matches!(r, PopupRow::Entry { glyph, .. } if glyph == "X") {
            assert!(
                modal.row_events[i].is_none(),
                "a bare sideline key is not a prefix chord"
            );
        }
    }
}

#[tokio::test]
async fn row_menu_unbound_key_dismisses() {
    // codex P2: the shared popup contract says an unbound key dismisses; the
    // row menu must not just ring BEL and stay open. (x-91a1) The byte is
    // chosen against the OPEN menu's accelerator set, so this fixture keeps
    // testing dismissal even after more actions gain menu keys.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    v.open_row_menu(idx, Anchor::Center);
    let bound: Vec<u8> = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .filter_map(|a| a.accelerator_id())
        .filter_map(crate::keys::menu_byte_for)
        .collect();
    let unbound = (b'a'..=b'z')
        .find(|b| !bound.contains(b))
        .expect("the menu scope leaves some letter unbound");
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[unbound], &mut buf).await.unwrap();
    assert!(
        v.row_menu.is_none(),
        "a byte no entry of this menu answers still dismisses it"
    );
}

#[tokio::test]
async fn row_menu_disambiguates_same_named_agents() {
    // codex P1: two rows share a name; the menu is pinned by the full
    // identity (pane_id/attach_id) so Focus acts on the row it was opened on,
    // never the other same-named row.
    let mk = |name: &str, pane_id: Option<u64>| AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: name.into(),
        pane_id,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    let mut v = view_with_agents(vec![mk("dup", Some(5)), mk("dup", Some(9))]);
    // Open the menu on the SECOND "dup" (pane 9) and pick Focus.
    let second = mk("dup", Some(9));
    v.row_menu = Some(build_row_menu(&second, Anchor::Center));
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::Focus)
        .unwrap();
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::FocusPane(pid)) => {
            assert_eq!(
                pid, 9,
                "focused the row the menu was opened on, not its twin"
            );
        }
        other => panic!("expected FocusPane(9), got {other:?}"),
    }
}

// ---- (x-92d3) wave 5: the tab menu --------------------------------------

/// The strip cell coordinates of the first tab and of the `+` NewTab cell,
/// off the same spans `tab_cell_at` walks.
fn tab_and_new_tab_cells(v: &View) -> ((u16, u16), (u16, u16)) {
    let pw = v.panel_w();
    let mut c = pw as usize;
    let mut tab = None;
    let mut plus = None;
    for span in v.tab_bar_window() {
        let w = span.text.chars().count();
        if tab.is_none() && matches!(span.hit, Some(super::TabHit::Tab(_))) {
            tab = Some((0u16, c as u16));
        }
        if plus.is_none() && matches!(span.hit, Some(super::TabHit::NewTab)) {
            plus = Some((0u16, c as u16));
        }
        c += w;
    }
    (tab.expect("a tab cell"), plus.expect("a + cell"))
}

#[test]
fn tab_menu_opens_off_a_tab_cell_with_destructive_last() {
    // AC3-HP: right-pressing a tab cell opens the menu pinned to that tab's
    // stable id, with close tab last after a rule.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let m = v.row_menu.as_ref().unwrap();
    assert_eq!(m.target, super::MenuTarget::Tab(0));
    assert_eq!(
        m.actions,
        vec![
            super::MenuAction::TabNew,
            super::MenuAction::TabRename,
            super::MenuAction::TabReorder(-1),
            super::MenuAction::TabReorder(1),
            super::MenuAction::TabMoveTo,
            super::MenuAction::TabJoin(Dir::Left),
            super::MenuAction::TabJoin(Dir::Right),
            super::MenuAction::TabJoin(Dir::Up),
            super::MenuAction::TabJoin(Dir::Down),
            super::MenuAction::TabClose,
        ]
    );
    // The destructive item sits last, after a Rule (menu grammar).
    let labels = menu_labels(m);
    // `Close`, not `Close tab`: one shape with the row menu's `✕ Remove`.
    assert_eq!(labels.last().map(String::as_str), Some("Close"));
    assert!(
        m.popup
            .rows
            .iter()
            .rposition(|r| matches!(r, PopupRow::Rule))
            .is_some_and(|rule_at| m.popup.rows.len() - 1 > rule_at),
        "a rule separates close tab from the rest"
    );
}

#[test]
fn tab_menu_falls_through_on_the_new_tab_cell() {
    // AC4-EDGE: the `+` cell is not a tab, so no menu opens and the press
    // is left to fall through - never swallowed by a silent no-op.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), (pr, pc)) = tab_and_new_tab_cells(&v);
    // Sanity: the cells are on the strip but resolve differently.
    assert_eq!(v.tab_cell_at(tr, tc), Some(0));
    assert_eq!(v.tab_cell_at(pr, pc), None);
    assert!(!v.open_tab_menu(pr, pc, Anchor::Center));
    assert!(v.row_menu.is_none(), "no menu, no swallow");
}

#[tokio::test]
async fn a_right_press_on_a_tab_cell_re_anchors_an_open_menu() {
    // (x-92d3 5.1) One press, same as a sideline row: with a menu open, a
    // right-press on a tab cell swaps in that tab's menu instead of
    // dismissing and making the user press again.
    let mut v = view_with_agents(vec![]);
    let tabs = squad_tabs(&v, 1);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    // Start from a menu pinned to the OTHER tab (index 1).
    v.row_menu = Some(super::build_tab_menu(1, &tabs[1], Anchor::Center));
    let mut buf: Vec<u8> = Vec::new();
    super::row_menu_mouse(
        &mut v,
        crate::mouse::MouseReport {
            row: tr,
            col: tc,
            kind: MouseKind::Press(MouseButton::Right),
            shift: false,
        },
        &mut buf,
    )
    .await
    .unwrap();
    assert_eq!(
        v.row_menu.as_ref().map(|m| m.target.clone()),
        Some(super::MenuTarget::Tab(tabs[0].id)),
        "the open menu re-anchored onto the pressed tab in one press"
    );
}

#[tokio::test]
async fn tab_menu_close_arms_the_confirm_and_enter_selects_then_closes() {
    // AC9-EDGE: close tab arms the confirm line, not a modal; Enter sends
    // SelectTab for the CAPTURED id then CloseTab (which closes the
    // sender's viewed tab server-side).
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap(); // sel starts on New tab
                                                                // Re-open and pick Close tab.
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    menu_select(&mut v, super::MenuAction::TabClose).await;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "arming sends nothing");
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(super::ConfirmKind::CloseTab { tab: 0 }) => {}
        _ => panic!("expected CloseTab{{tab:0}} confirm"),
    }
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::SelectTab(0), Command::CloseTab],
        "the captured tab is selected then closed"
    );
}

// ---- (x-d545) the row menu teaches its own keyboard ----------------------

/// A live paneless bg row: the menu shape the operator's complaint runs
/// through (Open Here / New Tab / splits / Peek / Mail / Stop / inert
/// Remove / Diff).
fn paneless_bg_row(name: &str) -> AgentRow {
    let mut r = agent_row(name, 10, None, false);
    r.pane_id = None;
    r.attach_id = Some("job1".into());
    r
}

fn entry_hint(v: &View, label: &str) -> Option<String> {
    let m = v.row_menu.as_ref()?;
    m.popup.rows.iter().find_map(|row| match row {
        PopupRow::Entry { label: l, hint, .. } if l == label => Some(hint.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn row_menu_verbs_show_their_menu_keys() {
    // AC9-HP: Stop, Peek, Mail, Open Here and Diff each show the
    // menu-scope key that runs them - the glyph read live from
    // keys::menu_key_for, never a literal.
    let mut v = view_with_agents(vec![paneless_bg_row("w1")]);
    assert!(v.open_row_menu(1, Anchor::Center));
    for (label, id) in [
        ("Open Here", "open-here"),
        ("Peek", "peek-row"),
        ("Mail", "mail-row"),
        ("Stop", "stop-row"),
        ("Diff", "diff-row"),
    ] {
        let want = crate::keys::menu_key_for(id).expect("the verb is bound in menu scope");
        assert_eq!(
            entry_hint(&v, label),
            Some(want),
            "{label} shows its menu-scope key"
        );
    }
}

#[tokio::test]
async fn row_menu_keys_run_the_same_execute_path_as_enter() {
    // AC9-HP: typing the drawn byte runs the entry through the SAME
    // execute path a click and Enter use - positive markers on the
    // command sent, the confirm armed, or the composer opened.
    let mut v = view_with_agents(vec![paneless_bg_row("w1")]);
    assert!(v.open_row_menu(1, Anchor::Center));

    // o runs Open Here: the attach-here command.
    let o = crate::keys::menu_byte_for("open-here").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[o], &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::attach_agent_here("job1".to_string())],
        "the open-here key attached here"
    );

    // s arms the SAME stop confirm the click arms.
    assert!(v.open_row_menu(1, Anchor::Center));
    let s = crate::keys::menu_byte_for("stop-row").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[s], &mut buf).await.unwrap();
    assert!(buf.is_empty(), "arming sends nothing");
    assert!(
        matches!(
            v.confirm.as_ref().map(|c| &c.action),
            Some(super::ConfirmKind::StopAgent { name }) if name == "w1"
        ),
        "the stop key armed the stop confirm"
    );

    // d sends the diff toggle for this row.
    assert!(v.open_row_menu(1, Anchor::Center));
    let d = crate::keys::menu_byte_for("diff-row").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[d], &mut buf).await.unwrap();
    assert!(
        decode_cmds(buf)
            .iter()
            .any(|c| matches!(c, Command::ToggleDiffPane { .. })),
        "the diff key toggled the diff pane"
    );

    // p opens the peek (the fetch rides the wire).
    assert!(v.open_row_menu(1, Anchor::Center));
    let p = crate::keys::menu_byte_for("peek-row").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[p], &mut buf).await.unwrap();
    assert!(
        decode_cmds(buf)
            .iter()
            .any(|c| matches!(c, Command::PeekAgent { .. })),
        "the peek key opened the peek"
    );

    // m opens the SAME composer peek `m` opens.
    assert!(v.open_row_menu(1, Anchor::Center));
    let m = crate::keys::menu_byte_for("mail-row").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[m], &mut buf).await.unwrap();
    assert!(
        v.peek_input.as_ref().is_some_and(|(name, _)| name == "w1"),
        "the mail key armed the peek composer"
    );
}

#[tokio::test]
async fn a_bound_byte_no_entry_offers_dismisses_without_action() {
    // AC10-HP: the resume byte (`r`, bound for exited-row menus) is NOT
    // offered by a live row's menu, so typing it dismisses exactly as any
    // unbound byte does and sends nothing - and the inert Remove entry
    // contributes no action slot for `x` to hit either.
    let mut v = view_with_agents(vec![paneless_bg_row("w1")]);
    assert!(v.open_row_menu(1, Anchor::Center));
    let r = crate::keys::menu_byte_for("resume-row").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[r], &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "the menu dismissed");
    assert!(buf.is_empty(), "no action fired");

    // The remove byte on a LIVE row's menu: remove-row is bound, the row
    // offers no Remove action (the entry is inert), so it dismisses too.
    assert!(v.open_row_menu(1, Anchor::Center));
    let x = crate::keys::menu_byte_for("remove-row").unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[x], &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "the menu dismissed");
    assert!(buf.is_empty(), "no remove fired from a live row's menu");
}

#[test]
fn the_inert_remove_entry_shows_the_key_and_the_precondition() {
    // AC11-EDGE: the disabled Remove entry carries the byte that WILL
    // remove the row once the precondition clears, beside the
    // precondition - and it stays unselectable, contributing no action
    // slot, so the actions vector stays index-aligned with the rows.
    let mut v = view_with_agents(vec![paneless_bg_row("w1")]);
    assert!(v.open_row_menu(1, Anchor::Center));
    let m = v.row_menu.as_ref().unwrap();
    let remove_key = crate::keys::menu_key_for("remove-row").unwrap();
    let inert_row = m.popup.rows.iter().find_map(|row| match row {
        PopupRow::Entry {
            glyph,
            label,
            hint,
            enabled,
        } if label == "Remove" && !*enabled => Some((glyph.clone(), hint.clone())),
        _ => None,
    });
    assert_eq!(
        inert_row,
        Some(("✕".into(), format!("{remove_key} stop first"))),
        "the hint carries the key and the precondition"
    );
    assert!(
        !m.actions
            .iter()
            .any(|a| matches!(a, super::MenuAction::Remove)),
        "an inert entry contributes no action slot"
    );
}

#[test]
fn new_menu_bindings_share_no_byte_within_one_offered_set() {
    // The one-safety property every new binding relies on: within any ONE
    // menu, no two offered actions resolve to the same byte, so the
    // offer-scoped dispatch can never be ambiguous. The exited-row menu
    // (Remove/Peek/Resume/Diff) and the live paneless menu are the two
    // shapes that carry the new verbs.
    let exited = build_row_menu(
        &{
            let mut r = paneless_bg_row("w1");
            r.exited = true;
            r
        },
        Anchor::Center,
    );
    let live = build_row_menu(&paneless_bg_row("w1"), Anchor::Center);
    for (menu, shape) in [(&exited, "exited"), (&live, "live")] {
        let mut seen: Vec<(u8, &str)> = Vec::new();
        for a in &menu.actions {
            if let Some(b) = a.accelerator_id().and_then(crate::keys::menu_byte_for) {
                assert!(
                    seen.iter().all(|(sb, _)| *sb != b),
                    "{shape} menu: byte {b} answers two actions ({seen:?} + {a:?})"
                );
                seen.push((b, "taken"));
            }
        }
    }
}

/// Point the open row menu's selection at `action` (executes nothing).
async fn menu_select(v: &mut View, action: super::MenuAction) {
    let m = v.row_menu.as_mut().unwrap();
    m.popup.sel = m
        .actions
        .iter()
        .position(|a| *a == action)
        .unwrap_or_else(|| panic!("menu should offer {action:?}"));
}

// ---- (x-91a1) menu-scoped accelerators -----------------------------------

#[tokio::test]
async fn menu_accelerator_rename_opens_the_tab_rename_overlay() {
    // AC2-HP: the byte drawn beside Rename - read from the menu registry,
    // never a literal - opens the rename overlay for the menu's stable tab.
    // The OVERLAY is the marker: asserting the menu closed would pass even
    // if the advertised key merely dismissed it, which is the exact defect
    // this node closes.
    let mut v = view_with_agents(vec![]);
    let want = squad_tabs(&v, 1)[0].id;
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let key = crate::keys::menu_byte_for("rename-tab").expect("rename-tab registered");
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[key], &mut buf).await.unwrap();
    assert_eq!(
        v.rename.as_ref().map(|(t, _)| t.clone()),
        Some(super::RenameTarget::Tab(want)),
        "the advertised key opened the rename overlay for this tab"
    );
}

#[tokio::test]
async fn menu_accelerator_new_tab_and_reorder_send_their_commands() {
    // The tab menu's other advertised keys are executable too, bytes read
    // from the registry: n sends NewTab, `<` reorders the pinned tab one
    // slot left. Positive markers on the decoded commands.
    let mut v = view_with_agents(vec![]);
    let want = squad_tabs(&v, 1)[0].id;
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    let mut buf: Vec<u8> = Vec::new();
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let n = crate::keys::menu_byte_for("new-tab").expect("new-tab registered");
    row_menu_keys(&mut v, &[n], &mut buf).await.unwrap();
    assert_eq!(decode_cmds(buf), vec![Command::NewTab], "n opened a tab");

    let mut buf: Vec<u8> = Vec::new();
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let left = crate::keys::menu_byte_for("move-tab-left").expect("move-tab-left registered");
    row_menu_keys(&mut v, &[left], &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::ReorderTab {
            squad: 1,
            tab: want,
            delta: -1
        }],
        "the angle bracket moved the pinned tab left"
    );
}

#[tokio::test]
async fn menu_accelerator_close_arms_the_confirm_and_closes() {
    // AC2-EDGE: the registry byte for Close arms the SAME confirm the click
    // arms, and committing it selects then closes the captured tab.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let key = crate::keys::menu_byte_for("close-tab").expect("close-tab registered");
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[key], &mut buf).await.unwrap();
    assert!(buf.is_empty(), "arming sends nothing");
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(super::ConfirmKind::CloseTab { tab: 0 }) => {}
        _ => panic!("expected CloseTab{{tab:0}} confirm"),
    }
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::SelectTab(0), Command::CloseTab],
        "the captured tab is selected then closed"
    );
}

#[tokio::test]
async fn menu_accelerator_remove_arms_the_dead_row_confirm() {
    // AC2-EDGE: the scoped `x` on an EXITED row's menu arms the removal
    // confirm for that exact row, the same confirm Enter on Remove arms.
    let exited = {
        let mut r = pane_hosted_row("dead", 0);
        r.pane_id = None;
        r.exited = true;
        r
    };
    let mut v = view_with_agents(vec![exited]);
    // A lone exited row makes the squad majority-exited -> LiveOnly, which
    // hides the row under test; pin Expanded so it renders (same pin as
    // the resume test).
    v.section_view.insert(
        SectionKey::Squad("/code/footnote".into()),
        SectionView::Expanded,
    );
    let idx = agent_row_at(&v, |a| a.name == "dead");
    v.open_row_menu(idx, Anchor::Center);
    let key = crate::keys::menu_byte_for("remove-row").expect("remove-row registered");
    let mut buf: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, &[key], &mut buf).await.unwrap();
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(super::ConfirmKind::RemoveAgent { name }) => assert_eq!(name, "dead"),
        _ => panic!("expected RemoveAgent{{dead}} confirm"),
    }
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::RemoveAgent {
            name: "dead".into()
        }],
        "the confirm the key armed removes exactly this row"
    );
}

#[test]
fn menu_accelerators_never_collide_within_one_menu() {
    // Dispatch picks the FIRST action a byte answers, so a second entry in
    // the SAME menu sharing that byte would be unreachable. Cross-menu
    // reuse (tab close vs dead-row remove) is legal - the actions never
    // share a popup - so uniqueness is asserted per built menu.
    let exited = {
        let mut r = pane_hosted_row("dead", 0);
        r.pane_id = None;
        r.exited = true;
        r
    };
    let live = pane_hosted_row("p", 9);
    let tabs = squad_tabs(&view_with_agents(vec![]), 1);
    let menus: &[(&str, RowMenu)] = &[
        ("exited row", super::build_row_menu(&exited, Anchor::Center)),
        (
            "live pane row",
            super::build_row_menu(&live, Anchor::Center),
        ),
        ("tab", super::build_tab_menu(0, &tabs[0], Anchor::Center)),
    ];
    for (name, menu) in menus {
        let mut seen: Vec<u8> = Vec::new();
        for a in &menu.actions {
            if let Some(id) = a.accelerator_id() {
                let b = crate::keys::menu_byte_for(id)
                    .unwrap_or_else(|| panic!("{name}: {id} left the menu scope"));
                assert!(
                    !seen.contains(&b),
                    "{name}: two entries answer {} ({})",
                    b as char,
                    id
                );
                seen.push(b);
            }
        }
    }
}

// ---- (x-7683) wave 1: right-click coverage on pane cells -----------------

#[tokio::test]
async fn x7683_right_press_in_a_pane_opens_the_owning_agents_row_menu() {
    // A pane cell is the one menu-bearing surface right-click skipped: the
    // press forwarded to the inner app instead. Now it opens the menu of the
    // agent that OWNS the pane under the cursor - the same menu its sideline
    // row opens - and nothing reaches the pane (AC1-HP).
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    // SGR right-press at 0-based (5, 30): content col 2, inside pane 10's
    // rect (x 0..35, content origin col 28).
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;31;6M", &mut buf)
        .await
        .unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "w"
        ),
        "the pane's owning agent menu opened"
    );
    assert!(buf.is_empty(), "no press forwards to the pane");
}

#[tokio::test]
async fn x7683_right_press_in_an_agentless_pane_still_forwards() {
    // AC3-EDGE preserved: a pane no agent row owns (scratch panes, an exited
    // and reaped roster) has no menu to open, so the press forwards to the
    // inner app exactly as before the pane path existed.
    let mut v = view_with_agents(vec![]);
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;31;6M", &mut buf)
        .await
        .unwrap();
    assert!(v.row_menu.is_none(), "no owner, no menu, no swallow");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Mouse { pane, .. } => assert_eq!(pane, 10, "forwarded to pane 10"),
        other => panic!("expected a pane mouse forward, got {other:?}"),
    }
}

#[tokio::test]
async fn a_confirm_survives_the_release_of_the_click_that_armed_it() {
    // The mouse path, end to end: clicking a menu entry that arms a confirm
    // used to cancel it on the RELEASE of that same click, so the prompt
    // opened and vanished inside one gesture and the entry read as dead.
    // Driven through handle_stdin with real SGR bytes - the builder-level
    // tests never touched this path, which is how the gap survived them.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let row = agent_row_at(&v, |a| a.name == "w");
    assert!(v.open_row_menu(row, Anchor::Center));
    menu_select(&mut v, super::MenuAction::Stop).await;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v.confirm.is_some(), "the entry armed a confirm");

    // The release of that click. Left release at 0-based (5, 30): `m`
    // terminates an SGR release, `M` a press.
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<0;31;6m", &mut buf)
        .await
        .unwrap();
    assert!(
        v.confirm.is_some(),
        "the arming click's own release must not cancel the confirm"
    );
    assert!(
        buf.is_empty(),
        "and it is still swallowed - a release must never reach the pane \
             underneath with no press before it"
    );

    // Enter still commits it.
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::StopAgent { name: "w".into() }],
        "Enter commits the confirm the click armed"
    );
}

#[tokio::test]
async fn an_outside_press_does_not_dismiss_an_armed_confirm() {
    // The confirm owns every pointer event. An outside press is swallowed,
    // while only its rendered shared Chrome esc chip cancels it.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let row = agent_row_at(&v, |a| a.name == "w");
    assert!(v.open_row_menu(row, Anchor::Center));
    menu_select(&mut v, super::MenuAction::Stop).await;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v.confirm.is_some());

    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<0;1;1M", &mut buf)
        .await
        .unwrap();
    assert!(v.confirm.is_some(), "an outside press does not cancel");
    assert!(buf.is_empty(), "and is swallowed, never reaching the pane");
}

#[tokio::test]
async fn x7683_right_press_on_a_pane_re_anchors_an_open_menu() {
    // While a menu is open, a right-press on a DIFFERENT pane's cell (off
    // the menu's own block) swaps in that pane's agent menu in one press -
    // the same re-anchor contract a tab cell and a sideline row carry.
    let mut v = view_with_agents(vec![
        agent_row("a", 10, Some(AgentBadge::Working), false),
        agent_row("b", 11, Some(AgentBadge::Working), false),
    ]);
    // Anchor b's menu deep in pane 11's columns so a press on pane 10
    // (screen col 35) is off the menu block entirely.
    let b = agent_row_at(&v, |a| a.name == "b");
    assert!(v.open_row_menu(b, Anchor::At { row: 5, col: 80 }));
    let mut buf: Vec<u8> = Vec::new();
    super::row_menu_mouse(
        &mut v,
        crate::mouse::MouseReport {
            row: 20,
            // Screen col 35: content col 7, inside pane 10's rect
            // (x 0..35, origin col 28), off the anchored menu block.
            col: 35,
            kind: MouseKind::Press(MouseButton::Right),
            shift: false,
        },
        &mut buf,
    )
    .await
    .unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "a"
        ),
        "one press re-anchored onto pane 10's agent"
    );
}

#[tokio::test]
async fn x7683_right_press_on_the_menu_body_over_a_sideline_row_is_swallowed() {
    // The block-contains swallow wins over EVERY re-anchor arm, not just
    // the pane one: a menu anchored at a sideline row extends over the
    // rows below it, and a press on its visible body must not silently
    // re-anchor onto the row underneath.
    let mut v = view_with_agents(vec![
        agent_row("a", 10, Some(AgentBadge::Working), false),
        agent_row("b", 11, Some(AgentBadge::Working), false),
    ]);
    // a is display row 1; anchor its menu AT that row so the block covers
    // b's row (display row 2) below it.
    assert!(v.open_row_menu(1, Anchor::At { row: 1, col: 1 }));
    let cell = (0..v.term.0)
        .flat_map(|r| (0..v.term.1).map(move |c| (r, c)))
        .find(|&(r, c)| v.row_menu_block_contains(r, c) && v.sideline_row_at(r, c) == Some(2))
        .expect("a menu cell over the sideline row below");
    let mut buf: Vec<u8> = Vec::new();
    super::row_menu_mouse(
        &mut v,
        crate::mouse::MouseReport {
            row: cell.0,
            col: cell.1,
            kind: MouseKind::Press(MouseButton::Right),
            shift: false,
        },
        &mut buf,
    )
    .await
    .unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "a"
        ),
        "a press on the menu body neither re-anchors nor dismisses"
    );
}

#[tokio::test]
async fn x7683_right_press_on_a_row_under_peek_still_opens_the_menu() {
    // Pre-diff behavior preserved: peek never intercepted the mouse, and a
    // right-press on a sideline row opened its menu OVER the peek (the
    // open path clears peek itself). Only the PANE path treats peek as a
    // blocker; blocking rows too would kill right-click while peek is
    // open, a regression.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.peek = Some(PeekView {
        cursor: agent_row_at(&v, |a| a.name == "w"),
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    });
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    // Right-press on the agent row (screen row 1, sideline col 5).
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;6;2M", &mut buf)
        .await
        .unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "w"
        ),
        "the row menu opens over peek, as it did before the guard"
    );
    assert!(v.peek.is_none(), "and clears peek itself");
}

#[tokio::test]
async fn x7683_long_press_on_a_tab_under_rename_degrades_to_the_click() {
    // A hold under a text-input overlay must not open a menu (it would
    // steal the typing); it degrades to the plain click the strip always
    // had, so the gesture is never a dead press.
    let mut v = view_with_agents(vec![]);
    v.rename = Some((super::RenameTarget::Squad(1), "na".into()));
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    v.tab_drag = Some(super::TabDrag {
        src_tab: v.tab_cell_at(tr, tc).unwrap(),
        zone: None,
        last_at: Instant::now(),
        start_at: Instant::now() - Duration::from_millis(600),
        moved: false,
    });
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, tr, tc, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "no menu under the rename overlay");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::SelectTab(_)) => {}
        other => panic!("the hold degraded to the click, got {other:?}"),
    }
}

// ---- (x-7683) wave 2: Left long-press opens the same menu ---------------

/// Release Left at 0-based (row, col) through the full stdin path.
async fn release_left(
    v: &mut View,
    row: u16,
    col: u16,
    buf: &mut Vec<u8>,
) -> Result<StdinFlow, String> {
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let bytes = format!("\x1b[<0;{};{}m", col + 1, row + 1);
    super::handle_stdin(v, &mut scanner, &mut carry, bytes.as_bytes(), buf).await
}

#[tokio::test]
async fn x7683_long_press_on_a_tab_cell_opens_the_tab_menu_not_a_select() {
    // A 600ms hold with no movement opens the tab menu at release - the
    // no-config path for a terminal that swallows right-click - and the
    // plain-click SelectTab does not fire.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    v.tab_drag = Some(super::TabDrag {
        src_tab: v.tab_cell_at(tr, tc).unwrap(),
        zone: None,
        last_at: Instant::now(),
        start_at: Instant::now() - Duration::from_millis(600),
        moved: false,
    });
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, tr, tc, &mut buf).await.unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Tab(_))
        ),
        "the held tab's menu opened"
    );
    assert!(buf.is_empty(), "no SelectTab rode the long press");
}

#[tokio::test]
async fn x7683_long_press_on_an_agent_row_opens_its_menu_not_the_click() {
    // Same contract on a sideline row: a 600ms hold opens the agent's row
    // menu; the focus/attach click action does not fire.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    // Display row 1 = agent w (row 0 is the squad name row); sideline col.
    v.row_drag = Some(super::RowDrag {
        src: super::RowSource::Pane(10),
        zone: None,
        last_at: Instant::now(),
        start_at: Instant::now() - Duration::from_millis(600),
        moved: false,
    });
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, 1, 5, &mut buf).await.unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "w"
        ),
        "the held row's menu opened"
    );
    assert!(buf.is_empty(), "no focus or pane input rode the long press");
}

#[tokio::test]
async fn a_long_press_on_a_workspace_row_opens_its_menu() {
    // (x-b465) The reported gap: a hold on a workspace row did nothing at
    // all. `open_row_menu` has built that menu since x-10ec, but the
    // long-press arm lived inside the `row_drag` branch and
    // `row_drag_source_at` answers only for agent rows, so a workspace press
    // armed no state and the release had nothing to resolve.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let hdr = squad_header_at(&v, 1);
    let id = v
        .row_identity(hdr)
        .expect("a workspace row has an identity");
    v.press_hold = Some((hdr, id, Instant::now() - Duration::from_millis(600)));
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, hdr as u16, 5, &mut buf).await.unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Section { squad: Some(1), .. })
        ),
        "the held workspace row's menu opened: {:?}",
        v.row_menu.as_ref().map(|m| &m.target)
    );
    assert!(buf.is_empty(), "no workspace selection rode the long press");
}

#[tokio::test]
async fn a_short_press_on_a_workspace_row_still_selects_it() {
    // The deferral must be invisible below the hold threshold: the press no
    // longer applies the click action, so the RELEASE has to.
    // Squad 2's row, not the active squad's: clicking the ACTIVE one cycles
    // its section view client-side and puts nothing on the wire, which
    // cannot tell a deferred click from a dropped one.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let hdr = squad_header_at(&v, 2);
    let id = v
        .row_identity(hdr)
        .expect("a workspace row has an identity");
    v.press_hold = Some((hdr, id, Instant::now()));
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, hdr as u16, 4, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "too short to be a hold");
    assert_eq!(
        decode_cmds(buf),
        vec![Command::SelectSquad(2)],
        "the click the press deferred runs on the release"
    );
}

#[tokio::test]
async fn a_long_press_on_an_inert_row_says_so_instead_of_nothing() {
    // A mission row is a `DisplayRow::Sub` label with no menu by
    // construction (its synthetic squad id is absent from `session.squads`,
    // so every squad verb would answer `no such squad`). The hold must still
    // ANSWER - silence is the defect, and a menu of failing entries would be
    // worse than none.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    // Seed a real mission squad: the fixture's own squads are ids 1 and 2,
    // neither synthetic, so without this there is no `Sub` row and the test
    // asserts nothing. An earlier version guarded on the row's existence and
    // returned - a green test measuring nothing.
    v.layout.squads.push(mission_meta(7, "epic  1/2"));
    let sub = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Sub(_)))
        .expect("the mission band renders its squad as an inert Sub row");
    let id = v.row_identity(sub).expect("a Sub row has an identity");
    v.press_hold = Some((sub, id, Instant::now() - Duration::from_millis(600)));
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, sub as u16, 5, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "an inert row opens no menu");
    assert_eq!(
        v.notice.as_ref().map(|(t, _)| t.as_str()),
        Some("no menu on the held row"),
        "the hold answers rather than falling silent"
    );
}

#[tokio::test]
async fn a_hold_whose_row_moved_under_it_refuses_rather_than_acting() {
    // An INDEX is not a row. `display_rows()` is rebuilt on every layout
    // push, so a row dropped mid-hold slides its neighbour under the same
    // number. Acting on that number would open a lifecycle menu - Stop,
    // Remove - on a worker nobody pressed. The same trap `RowDrag` avoids
    // with `RowSource` and the selector avoids by re-anchoring on name.
    let mut v = view_with_agents(vec![
        agent_row("alpha", 10, Some(AgentBadge::Working), false),
        agent_row("beta", 11, Some(AgentBadge::Working), false),
    ]);
    let held = agent_row_at(&v, |a| a.name == "alpha");
    let id = v.row_identity(held).expect("an agent row has an identity");
    v.press_hold = Some((held, id, Instant::now() - Duration::from_millis(600)));
    // alpha leaves while the button is down; beta takes its index.
    v.layout.agents.retain(|a| a.name != "alpha");
    assert_eq!(
        v.row_identity(held),
        Some("agent:beta".into()),
        "fixture: beta really did slide under the held index"
    );
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, held as u16, 5, &mut buf)
        .await
        .unwrap();
    assert!(
        v.row_menu.is_none(),
        "no menu opens on the row that took the index"
    );
    assert!(buf.is_empty(), "and no command rides the released press");
}

#[test]
fn the_reaper_opens_a_qualifying_hold_and_refuses_a_moved_one() {
    // A motionless hold emits no events, so for a workspace row the dead-
    // gesture reaper is the only thing that can fire before the release.
    // Both of its outcomes are asserted here: the untested half of a branch
    // is how the vacuous mission-row test got through.
    let mut v = view_with_agents(vec![
        agent_row("alpha", 10, Some(AgentBadge::Working), false),
        agent_row("beta", 11, Some(AgentBadge::Working), false),
    ]);
    let held = agent_row_at(&v, |a| a.name == "alpha");
    let id = v.row_identity(held).expect("an agent row has an identity");

    // Still the same row: the reaper opens its menu rather than dropping it.
    v.press_hold = Some((
        held,
        id.clone(),
        Instant::now() - Duration::from_millis(600),
    ));
    assert!(
        v.open_drag_menu(),
        "a qualifying hold opens from the reaper"
    );
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "alpha"
        ),
        "and on the row that was actually held"
    );
    assert!(v.press_hold.is_none(), "the reaper consumed the latch");

    // Row replaced under the index: refuse, and say why.
    v.row_menu = None;
    v.notice = None;
    v.press_hold = Some((held, id, Instant::now() - Duration::from_millis(600)));
    v.layout.agents.retain(|a| a.name != "alpha");
    assert!(!v.open_drag_menu(), "a moved row opens nothing");
    assert!(
        v.row_menu.is_none(),
        "least of all the row that replaced it"
    );
    assert_eq!(
        v.notice.as_ref().map(|(t, _)| t.as_str()),
        Some("the held row moved"),
        "and the refusal is stated, not silent"
    );
}

#[tokio::test]
async fn a_press_on_a_sideline_row_defers_its_click_to_the_release() {
    // The press arm end to end, through handle_stdin: it now defers EVERY
    // sideline click to the release, so a regression that swallowed the
    // press without the release re-applying it would look green in a suite
    // that only ever sets `press_hold` by hand.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let hdr = squad_header_at(&v, 2);
    let mut buf: Vec<u8> = Vec::new();
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let press = format!("\x1b[<0;{};{}M", 4 + 1, hdr + 1);
    super::handle_stdin(&mut v, &mut scanner, &mut carry, press.as_bytes(), &mut buf)
        .await
        .unwrap();
    assert!(v.press_hold.is_some(), "the press armed the hold");
    assert!(
        buf.is_empty(),
        "and sent nothing yet - the click is deferred"
    );

    release_left(&mut v, hdr as u16, 4, &mut buf).await.unwrap();
    assert!(v.press_hold.is_none(), "the release consumed the hold");
    assert_eq!(
        decode_cmds(buf),
        vec![Command::SelectSquad(2)],
        "and the deferred click ran"
    );
}

#[tokio::test]
async fn a_press_on_a_row_with_no_identity_is_not_deferred() {
    // A spacer has nothing to re-check after a layout push, so it must not
    // latch at all - its press keeps the pre-hold behavior rather than
    // deferring a click that can never be validated.
    let v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    let blank = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Blank))
        .expect("two squads put a spacer between the groups");
    assert!(
        v.press_hold_row_at(blank as u16, 4).is_none(),
        "a spacer arms no hold"
    );
}

#[test]
fn only_a_multi_pane_tab_wears_the_group_marker() {
    // A tab holding four panes rendered identically to one holding a single
    // pane, so the operator could not tell what a tab-level close would
    // destroy before pressing it.
    assert_eq!(
        super::tab_group_label("3:build".into(), 4),
        "▤ 3:build·4",
        "a group names its size"
    );
    assert_eq!(
        super::tab_group_label("3:build".into(), 1),
        "3:build",
        "a single-pane tab is untouched"
    );
    assert_eq!(
        super::tab_group_label("3:build".into(), 0),
        "3:build",
        "and so is an empty one"
    );
}

#[test]
fn a_live_row_shows_remove_as_inert_rather_than_hiding_it() {
    // The server refuses RemoveAgent on a live row ("still live - stop it
    // first"). Hiding the entry said the action does not exist; showing it
    // greyed says it exists and names the precondition. Disabled contributes
    // zero targets, so it can never be selected and never shifts the actions
    // vector.
    let live = agent_row("w", 10, Some(AgentBadge::Working), false);
    let menu = super::build_row_menu(&live, Anchor::Center);
    let inert: Vec<&PopupRow> = menu
        .popup
        .rows
        .iter()
        .filter(|r| matches!(r, PopupRow::Entry { enabled: false, .. }))
        .collect();
    assert!(
        matches!(
            inert.as_slice(),
            [PopupRow::Entry { label, hint, .. }]
                if label == "Remove"
                    && hint == &format!(
                        "{} stop first",
                        crate::keys::menu_key_for("remove-row").unwrap_or_default()
                    )
        ),
        "exactly one greyed Remove naming its key and precondition: {inert:?}"
    );
    assert!(
        !menu.actions.contains(&super::MenuAction::Remove),
        "a live row's Remove carries no action to run"
    );
}

#[tokio::test]
async fn x7683_short_press_keeps_the_plain_click_behavior() {
    // The guard is elapsed time, not the drag state itself: a fast
    // press-release still selects the tab and focuses the row exactly as
    // before the long-press existed.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    v.tab_drag = Some(super::TabDrag {
        src_tab: v.tab_cell_at(tr, tc).unwrap(),
        zone: None,
        last_at: Instant::now(),
        start_at: Instant::now(),
        moved: false,
    });
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, tr, tc, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "a short press opens no menu");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::SelectTab(_)) => {}
        other => panic!("expected SelectTab on a short press, got {other:?}"),
    }
}

// ---- (x-7683) wave 3: m-key parity + anchor ------------------------------

#[tokio::test]
async fn x7683_m_on_a_band_header_opens_the_section_menu() {
    // `m` opened menus only on agent rows and cards; a band header
    // (`~ elsewhere`) fell through to the tab-move notice. A header is a
    // menu-bearing row for the mouse, so the keyboard path now matches. A
    // band with a dead row is what makes the menu exist (an all-live band
    // refuses with a notice, exactly like the right-press path).
    let mut v = unified_rows_view();
    v.layout
        .agents
        .iter_mut()
        .find(|a| a.name == "bg-other")
        .expect("the orphan fixture row")
        .exited = true;
    let hdr = v
        .display_rows()
        .iter()
        .position(|r| {
            matches!(
                r,
                DisplayRow::Header {
                    key: SectionKey::Elsewhere,
                    ..
                }
            )
        })
        .expect("an ~ elsewhere header");
    v.selector = Some(hdr);
    let mut buf: Vec<u8> = Vec::new();
    super::selector_keys(&mut v, b"m", &mut buf).await.unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Section {
                key: SectionKey::Elsewhere,
                ..
            })
        ),
        "m on the header opens its section menu"
    );
}

#[tokio::test]
async fn x7683_m_anchor_lands_on_the_rows_screen_row() {
    // The m-key anchor still added TAB_BAR_ROWS to a geometry that dropped
    // that offset in x-cd67 (the sideline owns row 0), so the menu opened
    // one row below its row. The anchor is the row's screen row
    // (index - sideline_offset).
    let mut v = unified_rows_view();
    let w = agent_row_at(&v, |a| a.name == "worker");
    v.selector = Some(w);
    let mut buf: Vec<u8> = Vec::new();
    super::selector_keys(&mut v, b"m", &mut buf).await.unwrap();
    assert_eq!(
        v.row_menu.as_ref().unwrap().popup.anchor,
        crate::popup::Anchor::At {
            row: w as u16,
            col: 1
        },
        "the menu anchors on the row itself, not one below"
    );
}

// ---- (x-7683) wave 4: the help note ---------------------------------------

#[test]
fn x7683_keys_modal_names_every_menu_trigger_and_the_terminal_caveat() {
    // The operator-facing diagnosis: right-click works when the terminal
    // forwards it, and Terminal.app / unconfigured iTerm2 / tmux-with-mouse
    // do not. The in-app help must name all three triggers and the caveat,
    // so a swallowed right-click never reads as a dead feature.
    let mut view = two_pane_view();
    // Tall enough that the centered modal shows its tail (the note lines
    // ride below the binding sections; a short window scrolls them).
    view.term = (64, 100);
    view.open_keys_modal();
    let text = frame_text(&view.compose());
    let modal_tail: String = text
        .lines()
        .filter(|l| l.contains("menu") || l.contains("hold") || l.contains("click"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("right-click") && text.contains("hold"),
        "the context-menu row names the mouse path and the long-press; matched lines:\n{modal_tail}"
    );
    assert!(
        text.contains("terminal forwards it") || text.contains("terminal that"),
        "the caveat names the forwarding condition"
    );
}

// ---- (x-7683) review fixes: findings from the code-review drain ---------

#[tokio::test]
async fn x7683_right_press_on_the_menu_body_over_a_pane_is_swallowed() {
    // B1: hit_test is overlay-blind, so a menu floating over pane content
    // must win its own cells BEFORE the pane re-anchor runs - else a
    // right-press on the menu's body re-anchors onto the pane beneath it,
    // breaking the in-block swallow rule the comment still promises.
    let mut v = view_with_agents(vec![
        agent_row("a", 10, Some(AgentBadge::Working), false),
        agent_row("b", 11, Some(AgentBadge::Working), false),
    ]);
    let b = agent_row_at(&v, |a| a.name == "b");
    assert!(v.open_row_menu(b, Anchor::At { row: 5, col: 30 }));
    // A cell inside the rendered menu block that also sits over a pane.
    let cell = (0..v.term.0)
        .flat_map(|r| (0..v.term.1).map(move |c| (r, c)))
        .find(|&(r, c)| v.row_menu_block_contains(r, c) && v.hit_test(r, c).is_some())
        .expect("a menu cell floating over pane content");
    let mut buf: Vec<u8> = Vec::new();
    super::row_menu_mouse(
        &mut v,
        crate::mouse::MouseReport {
            row: cell.0,
            col: cell.1,
            kind: MouseKind::Press(MouseButton::Right),
            shift: false,
        },
        &mut buf,
    )
    .await
    .unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Agent(ident)) if ident.name == "b"
        ),
        "a press on the menu body neither re-anchors nor dismisses"
    );
}

#[tokio::test]
async fn x7683_slow_drag_that_ends_zoneless_is_a_click_not_a_menu() {
    // B2: the long-press clock alone cannot tell a hold from a slow drag.
    // A drag that MOVED (drag reports arrived) and ends zone-less on its
    // own tab keeps the plain click behavior, however long it took.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    v.tab_drag = Some(super::TabDrag {
        src_tab: v.tab_cell_at(tr, tc).unwrap(),
        zone: None,
        last_at: Instant::now(),
        start_at: Instant::now() - Duration::from_millis(600),
        moved: true,
    });
    let mut buf: Vec<u8> = Vec::new();
    release_left(&mut v, tr, tc, &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "a moved drag never opens a menu");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::SelectTab(_)) => {}
        other => panic!("expected SelectTab on a slow cancelled drag, got {other:?}"),
    }
}

#[tokio::test]
async fn x7683_right_press_in_a_pane_under_peek_still_forwards() {
    // B3: peek/nav don't intercept the mouse, so a pane-cell press under
    // them always fell through to the pane. The pane menu must not change
    // modes under an open overlay, and must not clear peek.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.peek = Some(PeekView {
        cursor: agent_row_at(&v, |a| a.name == "w"),
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    });
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;31;6M", &mut buf)
        .await
        .unwrap();
    assert!(v.row_menu.is_none(), "no menu under an open overlay");
    assert!(v.peek.is_some(), "peek survives the press");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Mouse { pane, .. } => assert_eq!(pane, 10),
        other => panic!("expected the pane forward, got {other:?}"),
    }
}

// ---- (x-7683) second review pass: overlay hijack, reaper, feedback -------

#[tokio::test]
async fn x7683_right_press_in_a_pane_under_rename_opens_nothing() {
    // A right-press under ANY key-owning overlay must not open the pane
    // menu: open_row_menu clears only peek, and the key router checks
    // row_menu ahead of the overlay, so a menu opening there would steal
    // the overlay's typing (rename Enter would run a menu action).
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.rename = Some((super::RenameTarget::Squad(1), "na".into()));
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;31;6M", &mut buf)
        .await
        .unwrap();
    assert!(v.row_menu.is_none(), "no menu under an open overlay");
    assert!(v.rename.is_some(), "the overlay survives untouched");
    assert!(
        buf.is_empty(),
        "a name modal swallows outside pointer input instead of leaking to a pane"
    );
}

#[test]
fn x7683_a_motionless_hold_past_the_reaper_opens_its_menu() {
    // The dead-drag reaper fires at 5s from the LAST MOTION, and a
    // motionless hold emits no motion - so a hold past the drag timeout
    // is cancelled before its release ever arrives. A hold that already
    // qualifies opens its menu at the reaper instead of dying silently.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    let tid = v.tab_cell_at(tr, tc).unwrap();
    v.tab_drag = Some(super::TabDrag {
        src_tab: tid,
        zone: None,
        last_at: Instant::now() - Duration::from_secs(6),
        start_at: Instant::now() - Duration::from_secs(6),
        moved: false,
    });
    assert!(
        v.open_drag_menu(),
        "a qualified motionless hold opens its tab menu"
    );
    assert!(matches!(
        v.row_menu.as_ref().map(|m| &m.target),
        Some(super::MenuTarget::Tab(_))
    ));
    assert!(v.tab_drag.is_none(), "the drag was consumed, not reaped");
    // A drag that MOVED never opens a menu at the reaper - it is a stuck
    // drag, exactly what the reaper exists to clear.
    v.row_menu = None;
    v.tab_drag = Some(super::TabDrag {
        src_tab: tid,
        zone: None,
        last_at: Instant::now() - Duration::from_secs(6),
        start_at: Instant::now() - Duration::from_secs(6),
        moved: true,
    });
    assert!(!v.open_drag_menu(), "a moved stale drag refuses the menu");
    assert!(v.row_menu.is_none());
}

#[tokio::test]
async fn x7683_m_on_the_backlog_header_says_why_not_nothing() {
    // The WorkQueue header is the one menu-less header by design, and
    // open_row_menu refuses it without a notice; `m` used to fall through
    // to the move notice. The key must not vanish silently.
    let mut v = unified_rows_view();
    let hdr = v
        .display_rows()
        .iter()
        .position(|r| {
            matches!(
                r,
                DisplayRow::Header {
                    key: SectionKey::WorkQueue,
                    ..
                }
            )
        })
        .expect("the backlog band header");
    v.selector = Some(hdr);
    v.notice = None;
    let mut buf: Vec<u8> = Vec::new();
    super::selector_keys(&mut v, b"m", &mut buf).await.unwrap();
    assert!(v.row_menu.is_none(), "the backlog header has no menu");
    assert!(v.notice.is_some(), "but m says why, never silence");
}

#[tokio::test]
async fn x7683_right_press_on_a_sideline_row_under_rename_opens_nothing() {
    // The overlay guard covers every right-press path, not just panes: a
    // menu opening over the rename overlay would steal its keys (the key
    // router checks row_menu first), so rename Enter could run a menu
    // action instead of submitting the name.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.rename = Some((super::RenameTarget::Squad(1), "na".into()));
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    // Right-press on the agent row itself (screen row 1, sideline col).
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;6;2M", &mut buf)
        .await
        .unwrap();
    assert!(v.row_menu.is_none(), "no row menu under an open overlay");
    assert!(v.rename.is_some(), "rename survives untouched");
}

#[tokio::test]
async fn x7683_right_press_on_a_sideline_row_under_nav_opens_nothing() {
    // nav looks read-only like peek (cursor + filtered list) but carries
    // a typed query buffer like rename/create/search - a menu opening
    // over it would swallow every keystroke meant for the filter (the
    // key router checks row_menu before nav), so it must guard the
    // row/tab paths the same way rename does, unlike peek.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.nav = Some(super::NavView {
        query: "w".into(),
        state_filter: None,
        cursor: 0,
    });
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    // Right-press on the agent row itself (screen row 1, sideline col).
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;6;2M", &mut buf)
        .await
        .unwrap();
    assert!(v.row_menu.is_none(), "no row menu under an open nav filter");
    assert!(v.nav.is_some(), "nav survives untouched");
}

#[tokio::test]
async fn x7683_right_press_on_a_sideline_row_under_answers_yard_digest_opens_nothing() {
    // answers/yard/digest LOOK read-only like peek, but each owns live
    // keys routed AFTER row_menu: answers dispatches PaneAnswer digits and
    // goto/close on Enter, yard takes n/N/q, digest swallows its next key
    // dismissing itself. Unlike peek (which the menu-open path clears),
    // none is cleared on open, so a menu over one steals its keys - Enter
    // meant to accept an answer could run a menu action on a live agent.
    // Named so clippy's type_complexity stays quiet: the array is a table of
    // (label, opener), and spelling the boxed closure inline said neither.
    type OpenOverlay = Box<dyn FnOnce(&mut super::View)>;
    let overlays: [(&str, OpenOverlay); 3] = [
        ("answers", Box::new(|v| v.answers = Some(0))),
        (
            "yard",
            Box::new(|v| {
                v.yard = Some(super::YardSel {
                    sel: 0,
                    opened_at: std::time::Instant::now(),
                })
            }),
        ),
        (
            "digest",
            Box::new(|v| v.digest = Some(vec!["catch-up".into()])),
        ),
    ];
    for (name, open) in overlays {
        let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
        open(&mut v);
        let mut scanner = Scanner::default();
        let mut carry = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        // Right-press on the agent row itself (screen row 1, sideline col).
        handle_stdin(&mut v, &mut scanner, &mut carry, b"\x1b[<2;6;2M", &mut buf)
            .await
            .unwrap();
        assert!(v.row_menu.is_none(), "no row menu over an open {name}");
    }
}

#[tokio::test]
async fn x7683_long_press_release_on_a_drop_zone_never_joins() {
    // A motionless hold sends no drag report, so the release coordinates
    // are the one unchecked signal left - a terminal that drops drag
    // reports can land them on a drop zone. The hold consumes the release
    // BEFORE the commit: a menu opens, no JoinTab travels.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    v.tab_drag = Some(super::TabDrag {
        src_tab: v.tab_cell_at(tr, tc).unwrap(),
        zone: None,
        last_at: Instant::now(),
        start_at: Instant::now() - Duration::from_millis(600),
        moved: false,
    });
    let mut buf: Vec<u8> = Vec::new();
    // Release at screen (5, 28): content col 0, the left edge of pane 10 -
    // a valid drop zone for a real drag.
    release_left(&mut v, 5, 28, &mut buf).await.unwrap();
    assert!(
        matches!(
            v.row_menu.as_ref().map(|m| &m.target),
            Some(super::MenuTarget::Tab(_))
        ),
        "the held tab's menu opened"
    );
    assert!(buf.is_empty(), "no JoinTab rode the hold");
}

#[tokio::test]
async fn tab_menu_stale_close_confirm_sends_nothing_on_enter() {
    // The CloseTab commit is SelectTab-then-CloseTab; a tab that vanished
    // while the prompt sat open must not fall through to a bare CloseTab,
    // which would close whatever is viewed NOW. Re-resolved at Enter.
    let mut v = view_with_agents(vec![]);
    v.confirm = Some(super::ConfirmAction {
        action: super::ConfirmKind::CloseTab { tab: 42 },
        label: "gone".into(),
    });
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a stale close sends nothing at all");
    assert!(v.notice.is_some(), "and says why");
}

#[tokio::test]
async fn tab_menu_rename_opens_the_overlay_for_that_tab() {
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    menu_select(&mut v, super::MenuAction::TabRename).await;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "rename is client-local");
    assert!(
        v.row_menu.is_none(),
        "open_rename replaces the menu overlay"
    );
    assert_eq!(
        v.rename.as_ref().map(|(t, _)| t.clone()),
        Some(super::RenameTarget::Tab(0)),
        "the overlay edits the CLICKED tab, not the viewed one"
    );
}

#[tokio::test]
async fn tab_menu_reorder_names_the_clicked_tab_and_its_squad() {
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    match menu_command_for(&mut v, super::MenuAction::TabReorder(1)).await {
        Command::ReorderTab { squad, tab, delta } => {
            assert_eq!((squad, tab, delta), (1, 0, 1));
        }
        other => panic!("expected ReorderTab, got {other:?}"),
    }
}

#[tokio::test]
async fn tab_menu_join_targets_the_viewed_tab_and_refuses_itself() {
    // Join is the drag's verb through the menu: the clicked tab joins the
    // viewed tab as a split of the focused pane. The clicked tab being the
    // focus's own tab is the join-into-self the wire refuses - named as a
    // notice instead of sent.
    let mut v = view_with_agents(vec![]);
    let focus = v.layout.focus;
    // Put the focus inside tab 0 (the clicked tab) by hand: meta() ships
    // empty pane lists, so this is fixture surgery, not layout truth.
    let squad = v.layout.squads.iter_mut().find(|s| s.id == 1).unwrap();
    squad.tabs[0].panes.push(crate::proto::PaneMeta {
        id: focus,
        label: "focused".into(),
    });
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let mut buf: Vec<u8> = Vec::new();
    menu_select(&mut v, super::MenuAction::TabJoin(Dir::Right)).await;
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a self-join sends nothing");
    assert!(v.notice.is_some(), "and says why");

    // Tab 1 (squad 1's second tab) holds no pane: joining it is legal.
    v.notice = None;
    v.row_menu = Some(super::build_tab_menu(
        1,
        &squad_tabs(&v, 1)[1],
        Anchor::Center,
    ));
    match menu_command_for(&mut v, super::MenuAction::TabJoin(Dir::Left)).await {
        Command::JoinTab {
            src_tab,
            anchor_pane,
            dir,
        } => {
            assert_eq!(src_tab, 1);
            assert_eq!(anchor_pane, focus);
            assert_eq!(dir, Dir::Left);
        }
        other => panic!("expected JoinTab, got {other:?}"),
    }
}

/// One squad's `TabMeta` list, cloned out of the layout borrow.
fn squad_tabs(v: &View, squad: u64) -> Vec<TabMeta> {
    v.layout
        .squads
        .iter()
        .find(|s| s.id == squad)
        .map(|s| s.tabs.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn tab_menu_stale_tab_notices_without_acting() {
    // A tab that closed between open and pick is a notice, never a
    // redirected action - the same stale-target contract as agent rows.
    let mut v = view_with_agents(vec![]);
    v.row_menu = Some(super::RowMenu {
        popup: crate::popup::Popup::new(
            vec![PopupRow::Entry {
                glyph: "✕".into(),
                label: "Close tab".into(),
                hint: String::new(),
                enabled: true,
            }],
            Anchor::Center,
        ),
        target: super::MenuTarget::Tab(99),
        actions: vec![super::MenuAction::TabClose],
    });
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a stale tab sends nothing");
    assert!(v.notice.is_some(), "and surfaces a notice");
}

#[test]
fn menu_hints_resolve_the_live_keymap_and_never_invent_one() {
    // AC8-EDGE, x-91a1: hints resolve from the LIVE menu-scope registry
    // (`menu_key_for`), never a literal and never a prefix-only chord - the
    // open menu does not run prefix chords, so advertising one is the LD9
    // lie. (x-d545) Diff, Peek and Resume are bound in menu scope now, so
    // they mirror their live glyphs; the invariant that survives is that
    // every hint IS the menu-scope answer, never a hardcoded chord.
    let exited = {
        let mut r = pane_hosted_row("dead", 0);
        r.pane_id = None;
        r.exited = true;
        r
    };
    let row = super::build_row_menu(&exited, Anchor::Center);
    let hint_of = |menu: &RowMenu, label: &str| {
        menu.popup
            .rows
            .iter()
            .find_map(|r| match r {
                PopupRow::Entry { label: l, hint, .. } if l == label => Some(hint.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no entry labelled {label}"))
    };
    assert_eq!(
        hint_of(&row, "Remove"),
        crate::keys::menu_key_for("remove-row").unwrap_or_default(),
        "Remove mirrors the menu-scope glyph for remove-row"
    );
    for (label, id) in [
        ("Diff", "diff-row"),
        ("Peek", "peek-row"),
        ("Resume", "resume-row"),
    ] {
        assert_eq!(
            hint_of(&row, label),
            crate::keys::menu_key_for(id).unwrap_or_default(),
            "{label} mirrors its menu-scope glyph"
        );
    }
    // Tab menu: every tab verb mirrors its scoped glyph; the join grid
    // carries no hint slot at all, so nothing can hardcode a chord there
    // either.
    let tabs = squad_tabs(&view_with_agents(vec![]), 1);
    let tab = super::build_tab_menu(0, &tabs[0], Anchor::Center);
    for (label, id) in [
        ("New tab", "new-tab"),
        ("Rename", "rename-tab"),
        ("Move left", "move-tab-left"),
        ("Move right", "move-tab-right"),
        ("Close", "close-tab"),
    ] {
        assert_eq!(
            hint_of(&tab, label),
            crate::keys::menu_key_for(id).unwrap_or_default(),
            "{label} mirrors menu_key_for({id})"
        );
    }
    for r in tab.popup.rows.iter().chain(row.popup.rows.iter()) {
        if let PopupRow::Entry { hint, .. } = r {
            assert!(
                !hint.contains('^'),
                "a literal chord in a menu hint is the LD9 lie: {hint}"
            );
        }
    }
}

#[test]
fn row_menu_entries_gate_resume_and_mail_by_row_state() {
    // AC7-HP shape: resume appears on an EXITED row only, mail on LIVE
    // rows only, both above the rule that fronts the destructive tail.
    let mk = |name: &str, pane_id: Option<u64>, exited: bool| {
        let mut r = pane_hosted_row(name, pane_id.unwrap_or(0));
        r.pane_id = pane_id;
        r.exited = exited;
        r
    };
    let dead = super::build_row_menu(&mk("d", None, true), Anchor::Center);
    assert!(dead.actions.contains(&super::MenuAction::Resume));
    assert!(!dead.actions.contains(&super::MenuAction::Mail));
    let live = super::build_row_menu(&mk("p", Some(9), false), Anchor::Center);
    assert!(live.actions.contains(&super::MenuAction::Mail));
    assert!(!live.actions.contains(&super::MenuAction::Resume));
    // Resume sits above the common rule; Stop/Diff ordering untouched.
    let labels = menu_labels(&dead);
    let (resume, rule) = (
        labels.iter().position(|l| l == "Resume").unwrap(),
        dead.popup
            .rows
            .iter()
            .rposition(|r| matches!(r, PopupRow::Rule))
            .unwrap(),
    );
    let resume_row = dead
        .popup
        .rows
        .iter()
        .position(|r| matches!(r, PopupRow::Entry { label, .. } if label == "Resume"))
        .unwrap();
    assert!(
        resume_row < rule,
        "resume is above the rule ({resume} < {rule})"
    );
}

#[tokio::test]
async fn row_menu_resume_sends_respawn_and_refuses_a_live_row() {
    // The menu twin of peek `r`: RespawnAgent on an exited row, re-checked
    // LIVE at execute - a row that came back on its own is not respawned.
    let mut dead = pane_hosted_row("dead", 0);
    dead.pane_id = None;
    dead.exited = true;
    let mut v = view_with_agents(vec![dead.clone()]);
    // A lone exited row makes the squad majority-exited -> LiveOnly, which
    // hides the very row under test; pin Expanded so it renders.
    v.section_view.insert(
        SectionKey::Squad("/code/footnote".into()),
        SectionView::Expanded,
    );
    let idx = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == "dead"))
        .unwrap();
    assert!(v.open_row_menu(idx, Anchor::Center));
    match menu_command_for(&mut v, super::MenuAction::Resume).await {
        Command::RespawnAgent { name } => assert_eq!(name, "dead"),
        other => panic!("expected RespawnAgent, got {other:?}"),
    }
    // The menu stayed open from the DEAD row; the row is live NOW. The
    // pinned entry must refuse rather than respawn a live session.
    let mut v2 = view_with_agents(vec![dead.clone()]);
    v2.row_menu = Some(super::build_row_menu(&dead, Anchor::Center));
    v2.layout.agents[0].exited = false;
    let mut buf: Vec<u8> = Vec::new();
    menu_select(&mut v2, super::MenuAction::Resume).await;
    row_menu_execute_selected(&mut v2, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a live row is not respawned");
    assert!(v2.notice.is_some());
}

#[tokio::test]
async fn row_menu_mail_opens_the_same_peek_composer() {
    // AC7-HP: mail routes into the EXISTING peek overlay + free-text
    // composer that peek `m` opens - no second input surface is grown.
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    assert!(v.open_row_menu(idx, Anchor::Center));
    let mut buf: Vec<u8> = Vec::new();
    menu_select(&mut v, super::MenuAction::Mail).await;
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    // fetch_peek writes its request; the composer is the client-side state.
    assert!(v.peek.is_some(), "peek opened");
    assert_eq!(
        v.peek_input,
        Some(("bg-claude".into(), String::new())),
        "the SAME peek m composer is armed"
    );
}

/// A pane-hosted sideline row, the shape the move/break-out menu acts on.
fn pane_hosted_row(name: &str, pane_id: u64) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: name.into(),
        pane_id: Some(pane_id),
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    }
}

/// A paneless bg row that is still attachable here - the branch carrying the
/// Split grid, the Move grid's twin.
fn attachable_row(name: &str, attach_id: &str) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        pane_id: None,
        attach_id: Some(attach_id.into()),
        ..pane_hosted_row(name, 0)
    }
}

/// Pick `action` out of a live row menu and run it, returning what went on
/// the wire.
async fn menu_command_for(v: &mut View, action: super::MenuAction) -> Command {
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == action)
        .unwrap_or_else(|| panic!("menu should offer {action:?}"));
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(v, &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(c) => c,
        other => panic!("expected a Command, got {other:?}"),
    }
}

#[tokio::test]
async fn row_menu_move_relocates_the_live_pane() {
    // An OFF-SCREEN row names the viewed focus, so the server grafts the
    // pane into the current view the way the paneless branch's Split entries
    // place a new one. (Not AttachAgent: on an already-attached session that
    // reconciles to an idempotent focus and moves nothing.)
    let row = pane_hosted_row("p", 7);
    let mut v = view_with_agents(vec![row.clone()]);
    v.row_menu = Some(build_row_menu(&row, Anchor::Center));
    let focus = v.layout.focus;
    match menu_command_for(&mut v, super::MenuAction::MoveDir(Dir::Up)).await {
        Command::MovePane { mover, target, dir } => {
            assert_eq!(mover, Some(7), "moves the row's own pane");
            assert_eq!(
                target,
                Some(focus),
                "anchored on the viewed tab's focus, NOT left for the server \
                     to resolve inside the row's own (background) tab"
            );
            assert_eq!(dir, Dir::Up);
        }
        other => panic!("expected MovePane, got {other:?}"),
    }
}

/// Every ON-SCREEN row leaves `target` unset, whether or not it is the
/// focused one. Naming the focus for an on-screen pane sends an origin drop
/// whenever the pane already sits `dir`-ward of it, and `move_leaf` reports
/// that two ways: `mover == target` for the focused row, and a `same_shape`
/// result for any other on-screen row. The server discards BOTH in silence,
/// so the entry would do nothing and say nothing. Unset avoids the whole
/// class, because navigate never returns the mover itself.
#[tokio::test]
async fn row_menu_move_on_an_onscreen_row_leaves_the_target_unset() {
    let v0 = view_with_agents(vec![]);
    let focused = v0.layout.focus;
    let onscreen_unfocused = v0
        .layout
        .panes
        .iter()
        .map(|(id, _)| *id)
        .find(|id| *id != focused)
        .expect("fixture has a second on-screen pane");
    for pid in [focused, onscreen_unfocused] {
        let mut v = view_with_agents(vec![]);
        let row = pane_hosted_row("p", pid);
        v.layout.agents = vec![row.clone()];
        v.row_menu = Some(build_row_menu(&row, Anchor::Center));
        match menu_command_for(&mut v, super::MenuAction::MoveDir(Dir::Left)).await {
            Command::MovePane { mover, target, dir } => {
                assert_eq!(mover, Some(pid));
                assert_eq!(
                    target, None,
                    "pane {pid} is on screen; naming the focus risks an origin \
                         drop the server discards in silence"
                );
                assert_eq!(dir, Dir::Left);
            }
            other => panic!("expected MovePane, got {other:?}"),
        }
    }
}

/// The selectable labels of a built menu, in the order `popup.sel` indexes
/// them - the same order `add` extends `actions`, so the two zip.
fn menu_labels(menu: &RowMenu) -> Vec<String> {
    menu.popup
        .targets()
        .iter()
        .map(|(ri, ci)| match &menu.popup.rows[*ri] {
            PopupRow::Grid(cells) => cells[*ci].label.clone(),
            PopupRow::Entry { label, .. } => label.clone(),
            PopupRow::FullWidth(l) => l.clone(),
            PopupRow::Header(_) | PopupRow::Rule => unreachable!("not a target"),
        })
        .collect()
}

#[test]
fn menu_grid_cells_send_the_direction_on_their_label() {
    // Rows and actions are two parallel lists joined only by position, so a
    // transposed pair puts "Move Left" over Dir::Right and every other test
    // still passes - they all locate an entry BY action, never by what the
    // operator reads. Both grids are covered: the pane-hosted Move grid and
    // the paneless Split grid have the same construction and the same gap.
    let cases: Vec<(RowMenu, Vec<(&str, super::MenuAction)>)> = vec![
        (
            build_row_menu(&pane_hosted_row("p", 7), Anchor::Center),
            vec![
                ("Move Left", super::MenuAction::MoveDir(Dir::Left)),
                ("Move Right", super::MenuAction::MoveDir(Dir::Right)),
                ("Move Up", super::MenuAction::MoveDir(Dir::Up)),
                ("Move Down", super::MenuAction::MoveDir(Dir::Down)),
            ],
        ),
        (
            build_row_menu(&attachable_row("a", "att-1"), Anchor::Center),
            vec![
                ("Split Left", super::MenuAction::Split(Dir::Left)),
                ("Split Right", super::MenuAction::Split(Dir::Right)),
                ("Split Up", super::MenuAction::Split(Dir::Up)),
                ("Split Down", super::MenuAction::Split(Dir::Down)),
            ],
        ),
    ];
    for (menu, want) in cases {
        let labels = menu_labels(&menu);
        assert_eq!(
            labels.len(),
            menu.actions.len(),
            "one action per selectable cell"
        );
        for (want_label, want_action) in want {
            let i = labels
                .iter()
                .position(|l| l == want_label)
                .unwrap_or_else(|| panic!("menu has a {want_label} cell: {labels:?}"));
            assert_eq!(
                menu.actions[i], want_action,
                "the cell reading {want_label:?} must send {want_action:?}"
            );
        }
    }
}

#[test]
fn pane_menu_new_tab_cell_breaks_out() {
    let menu = build_row_menu(&pane_hosted_row("p", 7), Anchor::Center);
    let labels = menu_labels(&menu);
    let i = labels
        .iter()
        .position(|l| l.contains("New Tab"))
        .expect("menu has a New Tab cell");
    assert_eq!(menu.actions[i], super::MenuAction::BreakOut);
}

#[tokio::test]
async fn row_menu_break_out_breaks_the_pane_into_its_own_tab() {
    let row = pane_hosted_row("p", 7);
    let mut v = view_with_agents(vec![row.clone()]);
    v.row_menu = Some(build_row_menu(&row, Anchor::Center));
    assert_eq!(
        menu_command_for(&mut v, super::MenuAction::BreakOut).await,
        Command::BreakPane { pane: 7 }
    );
}

#[tokio::test]
async fn row_menu_detach_sends_the_pane_id_without_global_detach() {
    let row = pane_hosted_row("p", 7);
    let mut v = view_with_agents(vec![row.clone()]);
    v.row_menu = Some(build_row_menu(&row, Anchor::Center));
    assert_eq!(
        menu_command_for(&mut v, super::MenuAction::Detach).await,
        Command::DetachPane { pane: 7 }
    );
}

#[tokio::test]
async fn row_menu_reattach_sends_resume_for_a_live_paneless_row() {
    let mut row = pane_hosted_row("p", 0);
    row.pane_id = None;
    row.no_pane_reason = Some(AgentNoPaneReason::LivePaneless);
    let mut v = view_with_agents(vec![row.clone()]);
    v.row_menu = Some(build_row_menu(&row, Anchor::Center));
    assert_eq!(
        menu_command_for(&mut v, super::MenuAction::Reattach).await,
        Command::ResumeAgent { name: "p".into() }
    );
}

#[tokio::test]
async fn row_menu_move_is_the_same_operation_as_the_row_drag() {
    // Invariant (AGENTS.md "a guard on one of N reachable paths is
    // decorative"): re-placing a pane-hosted row is reachable by menu AND by
    // drag, and the two must stay ONE operation. Compare DESTINATION, not
    // just the command variant: an earlier cut of this agreed on MovePane
    // while the menu left `target` unset, which sent the pane wandering
    // inside its own background tab instead of into the current view. The
    // fixture row is OFF-SCREEN, which is the arm where the menu names a
    // target at all; an on-screen row leaves it unset and is pinned
    // separately. The exact target ids differ by construction (a drag names
    // the pane its drop zone touched, a menu the viewed focus), so what is pinned
    // here is that BOTH name a concrete in-view destination.
    let row = pane_hosted_row("p", 99);
    let mut v = view_with_agents(vec![row.clone()]);
    v.row_menu = Some(build_row_menu(&row, Anchor::Center));
    let in_view: Vec<u64> = v.layout.panes.iter().map(|(id, _)| *id).collect();
    let via_menu = menu_command_for(&mut v, super::MenuAction::MoveDir(Dir::Right)).await;

    let mut dragged = three_pane_view();
    dragged.begin_row_drag(RowSource::Pane(99), Instant::now());
    let (r, c) = seam_cell_between(&dragged, 11, 12);
    assert!(dragged.row_drag_to(r, c, Instant::now()));
    let via_drag = dragged.commit_row_drag().expect("a drop in a zone commits");

    match (via_menu, via_drag) {
        (
            Command::MovePane {
                mover: m,
                target: mt,
                ..
            },
            Command::MovePane {
                mover: d,
                target: dt,
                ..
            },
        ) => {
            assert_eq!(m, Some(99));
            assert_eq!(d, m, "both name the clicked/dragged pane as the mover");
            assert!(
                mt.is_some_and(|t| in_view.contains(&t)),
                "the menu names a destination in the VIEWED tab, got {mt:?}"
            );
            assert!(
                dt.is_some(),
                "the drag names its drop-zone pane, got {dt:?}"
            );
        }
        (m, d) => panic!("both paths must relocate via MovePane: menu={m:?} drag={d:?}"),
    }
}

#[test]
fn footer_menu_region_routes_a_click_to_the_sideline_menu() {
    // US4: a click on the footer's `☰ menu` region opens the MENU popup; the
    // rest of the `+ new workspace` row still opens create.
    let mut v = two_pane_view();
    v.term = (30, 100);
    let footer = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::NewSquad))
        .unwrap();
    let panel_w = v.panel_w() as usize;
    let range = v
        .footer_menu_range(panel_w)
        .expect("a wide panel shows the menu button");
    // (x-cd67 US1) The sideline owns row 0, so outer row == display index - offset.
    let trow = (footer - v.sideline_offset) as u16;
    assert!(matches!(
        v.chrome_hit(trow, range.start as u16),
        Some(ChromeHit::OpenSidelineMenu { .. })
    ));
    assert!(matches!(v.chrome_hit(trow, 2), Some(ChromeHit::OpenCreate)));
}

#[tokio::test]
async fn sideline_menu_settings_toggle_flips_session_state_and_stays_open() {
    // MENU -> settings chains, and a toggle flips session state and keeps
    // the modal open while its persistence result is reported.
    let mut v = two_pane_view();
    v.term = (30, 100);
    v.open_sideline_menu(Anchor::Center);
    let settings = v
        .aux
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == AuxAction::OpenSettings)
        .unwrap();
    v.aux.as_mut().unwrap().popup.sel = settings;
    let mut buf: Vec<u8> = Vec::new();
    aux_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v
        .aux
        .as_ref()
        .unwrap()
        .actions
        .contains(&AuxAction::ToggleHoverFocus));
    let before = v.hover_focus;
    let hf = v
        .aux
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == AuxAction::ToggleHoverFocus)
        .unwrap();
    v.aux.as_mut().unwrap().popup.sel = hf;
    aux_execute_selected(&mut v, &mut buf).await.unwrap();
    assert_eq!(v.hover_focus, !before, "toggle flips session state");
    assert!(v
        .notice
        .as_ref()
        .is_some_and(|(notice, _)| notice.contains("focus follows mouse")));
    assert!(v.aux.is_some(), "settings stays open for another toggle");
}

#[test]
fn settings_theme_tab_lists_the_shipped_palettes() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::Theme;
    let modal = v.build_settings_modal();
    // One ApplyTheme action per shipped theme, in display order.
    let names: Vec<String> = modal
        .actions
        .iter()
        .filter_map(|a| match a {
            AuxAction::ApplyTheme(n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let want: Vec<String> = crate::theme::THEME_NAMES
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(names, want);
    // The active theme (terminal by default) is marked with the filled dot.
    assert!(
        modal
            .popup
            .rows
            .iter()
            .any(|r| matches!(r, PopupRow::Entry { glyph, .. } if glyph == "●")),
        "active theme is marked"
    );
    // The chrome carries all section tabs (positive marker it framed).
    assert_eq!(modal.popup.chrome.tabs.len(), 4);
}

#[test]
fn settings_keys_tab_lists_prefix_picks_and_names_the_live_prefix() {
    let (rows, actions) = build_prefix_settings_rows("C-b");
    assert!(matches!(
        rows.first(),
        Some(PopupRow::Header(header)) if header == "prefix: C-b"
    ));
    let specs: Vec<String> = actions
        .iter()
        .filter_map(|action| match action {
            AuxAction::ApplyPrefix(spec) => Some(spec.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(specs, PREFIX_PICKS.map(String::from));
    assert!(rows.iter().any(|row| matches!(
        row,
        PopupRow::Entry { glyph, label, .. } if glyph == "●" && label == "C-b"
    )));

    let (custom_rows, _) = build_prefix_settings_rows("C-q");
    assert!(matches!(
        custom_rows.first(),
        Some(PopupRow::Header(header)) if header == "prefix: C-q"
    ));
    assert!(!custom_rows
        .iter()
        .any(|row| matches!(row, PopupRow::Entry { glyph, .. } if glyph == "●")));
}

#[tokio::test]
async fn settings_tabs_cycle_through_keys() {
    let mut v = two_pane_view();
    v.aux = Some(v.build_settings_modal());
    let mut buf: Vec<u8> = Vec::new();
    aux_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert_eq!(v.settings_tab, SettingsTab::Theme);
    aux_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert_eq!(v.settings_tab, SettingsTab::Keys);
    aux_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert_eq!(v.settings_tab, SettingsTab::Colors);
    aux_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert_eq!(v.settings_tab, SettingsTab::General);
}

#[tokio::test]
async fn lane_key_entry_enter_opens_the_picker_for_the_typed_key() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::Colors;
    v.aux = Some(v.build_settings_modal());
    v.lane.axis = Some("route".into());
    v.lane.key_entry = Some(("route".into(), String::new()));
    v.reopen_settings_keeping_sel();
    let mut buf: Vec<u8> = Vec::new();
    aux_keys(&mut v, b"zai\r", &mut buf).await.unwrap();
    assert!(v.lane.key_entry.is_none(), "entry closed on submit");
    assert_eq!(
        v.lane.pick,
        Some(("route".into(), "zai".into())),
        "the picker opens for the typed key"
    );
}

#[tokio::test]
async fn lane_custom_entry_enter_refuses_an_invalid_color_without_saving() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::Colors;
    v.aux = Some(v.build_settings_modal());
    v.lane.pick = Some(("route".into(), "zai".into()));
    v.lane.custom_entry = Some("#12a".into());
    v.reopen_settings_keeping_sel();
    let mut buf: Vec<u8> = Vec::new();
    aux_keys(&mut v, b"\r", &mut buf).await.unwrap();
    // The refusal is a notice; the drill returns to the picker.
    assert!(
        v.notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("invalid color")),
        "the refusal names the accepted shapes: {:?}",
        v.notice
    );
    assert!(v.lane.custom_entry.is_none(), "entry closed");
    assert_eq!(v.lane.pick, Some(("route".into(), "zai".into())));
}

#[tokio::test]
async fn lane_entry_esc_cancels_the_entry_and_keeps_the_drill() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::Colors;
    v.aux = Some(v.build_settings_modal());
    v.lane.pick = Some(("route".into(), "zai".into()));
    v.lane.custom_entry = Some("#12".into());
    v.reopen_settings_keeping_sel();
    let mut buf: Vec<u8> = Vec::new();
    // A lone ESC press is buffered by fold_search_input (split-arrow
    // safety); a following non-'[' byte is what surfaces it, exactly as a
    // real terminal's next chunk would.
    aux_keys(&mut v, b"\x1bx", &mut buf).await.unwrap();
    assert!(v.lane.custom_entry.is_none(), "entry cancelled");
    assert_eq!(v.lane.pick, Some(("route".into(), "zai".into())));
}

#[tokio::test]
async fn lane_entry_buffer_dies_with_a_mouse_dismiss() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::Colors;
    v.aux = Some(v.build_settings_modal());
    v.lane.pick = Some(("route".into(), "zai".into()));
    v.lane.custom_entry = Some("#12".into());
    v.reopen_settings_keeping_sel();
    let mut buf: Vec<u8> = Vec::new();
    // A click OFF the block dismisses the modal AND drops the buffer, so
    // a stale entry can never capture keys in a reopened modal. (Row
    // clicks are additionally guarded inert while an entry is armed; the
    // entry views render no selectable targets of their own, so that
    // guard has no deterministic click surface against an ambient
    // palette and is covered by review, not by this test.)
    aux_mouse(&mut v, left_click(1, 1), &mut buf).await.unwrap();
    assert!(v.aux.is_none(), "off-block click dismisses");
    assert!(
        v.lane.custom_entry.is_none(),
        "the buffer died with the modal"
    );
    assert_eq!(v.lane.pick, Some(("route".into(), "zai".into())));
}

#[tokio::test]
async fn refused_prefix_pick_changes_nothing_and_shows_the_validator_reason() {
    let before = crate::keys::prefix();
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::Keys;
    v.aux = Some(v.build_settings_modal());
    let mut buf: Vec<u8> = Vec::new();
    execute_aux_action(&mut v, AuxAction::ApplyPrefix("3".into()), &mut buf)
        .await
        .unwrap();
    assert_eq!(crate::keys::prefix(), before);
    assert!(v
        .notice
        .as_ref()
        .is_some_and(|(notice, _)| notice.contains("1-9 select tabs")));
}

#[test]
fn settings_general_tab_keeps_the_session_toggles() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::General;
    let modal = v.build_settings_modal();
    assert!(modal.actions.contains(&AuxAction::ToggleHoverFocus));
    assert!(modal.actions.contains(&AuxAction::ToggleStatus));
    assert!(modal.actions.contains(&AuxAction::ToggleResourceMeter));
    assert!(modal.popup.rows.iter().all(|row| !matches!(
        row,
        PopupRow::Entry { hint, .. } if hint == "session only"
    )));
}

#[tokio::test]
async fn resource_meter_toggle_flips_persists_and_arms_the_sampler() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::General;
    let mut buf: Vec<u8> = Vec::new();
    execute_aux_action(&mut v, AuxAction::ToggleResourceMeter, &mut buf)
        .await
        .unwrap();
    assert!(v.resource_meter_on);
    assert!(
        v.resource_meter_sampling,
        "the run loop must be told to spawn the sampler"
    );
    assert!(v.resource_meter_text.is_none());
    assert!(v.notice.as_ref().is_some_and(|(notice, _)| {
        notice == "resource meter applied this session; save failed"
            || notice.starts_with("resource meter: ")
    }));
    // Off again: the gate flips and the sampler exits on its next wake.
    execute_aux_action(&mut v, AuxAction::ToggleResourceMeter, &mut buf)
        .await
        .unwrap();
    assert!(!v.resource_meter_on);
    assert!(!v.resource_meter_sampling);
    assert!(!v
        .resource_meter_gate
        .load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn a_dark_macmon_sample_never_renders_a_number() {
    // An empty or unparseable pipe parses to None; `sample_macmon_line`
    // renders that as the unavailable line, never a zero.
    assert_eq!(parse_macmon_sample(b""), None);
    assert_eq!(parse_macmon_sample(b"not json\n"), None);
    let good = parse_macmon_sample(
        br#"{"cpu_usage_pct":0.45,"sys_power":53.5,"memory":{"ram_total":103079215104,"ram_usage":30702266368}}"#,
    )
    .expect("a healthy sample parses");
    assert!(good.contains("cpu 45%"), "{good}");
    // Decimal GB (bytes / 1e9), matching the Python arm's convention.
    assert!(good.contains("mem 31G/103G"), "{good}");
    assert!(good.contains("54W"), "{good}");
    // A missing memory block parses to None, which renders as the
    // unavailable line - never a zero.
    assert_eq!(parse_macmon_sample(br#"{"cpu_usage_pct":0.45}"#), None);
}

#[tokio::test]
async fn settings_status_toggle_stays_live_when_the_save_fails() {
    let mut v = two_pane_view();
    let before = v.status_on;
    let mut buf: Vec<u8> = Vec::new();
    execute_aux_action(&mut v, AuxAction::ToggleStatus, &mut buf)
        .await
        .unwrap();
    assert_eq!(v.status_on, !before);
    assert!(!buf.is_empty(), "status toggle still sends the resize");
    assert!(v.notice.as_ref().is_some_and(|(notice, _)| {
        notice == "status row applied this session; save failed"
            || notice.starts_with("status row: ")
    }));
}

#[test]
fn every_overlay_constructor_wears_chrome_matching_its_anchor() {
    // (x-f75e) Chrome is mandatory by construction: Popup.chrome is
    // non-optional and draw_lines_overlay takes a &Chrome, so a new overlay
    // cannot skip it - it is a compile error, not a review catch. This
    // enumerates the family-A constructors reachable with light fixtures and
    // asserts each renders a border (positive marker) at the level its
    // anchor dictates (Centered -> Full, anchored -> Bare).
    //
    // The full set of fourteen: family A (7) = keys modal, row menu, card
    // menu, section menu, sideline MENU, mini-kanban, settings; family B
    // (7) = the seven draw_lines_overlay callers (catch-up, needs-me,
    // move-pick, attach-place, connections, peek, navigator), verified by
    // draw_lines_overlay_centers_within_viewport and the chrome::frame tests.
    let assert_chrome = |p: &Popup, expected: chrome::Level| {
        assert_eq!(p.chrome.level(), expected, "level matches the anchor");
        let r = p.render((40, 100));
        assert!(
            r.lines
                .iter()
                .any(|l| l.text.starts_with('┌') || l.text.starts_with('└')),
            "a border corner was drawn"
        );
    };
    // Centered (Full): keys modal, sideline MENU, settings.
    assert_chrome(&build_keys_modal().popup, chrome::Level::Full);
    assert_chrome(
        &build_sideline_menu(Anchor::Center, None).popup,
        chrome::Level::Full,
    );
    let v = two_pane_view();
    assert_chrome(&v.build_settings_modal().popup, chrome::Level::Full);
    // Anchored (Bare): the row context menu, opened next to the pointer.
    let agent = tab_agent(None, None, false);
    assert_chrome(
        &build_row_menu(&agent, Anchor::At { row: 5, col: 5 }).popup,
        chrome::Level::Bare,
    );
}

/// AC5-HP: a ready outcome puts the update row above keybinds.
#[test]
fn sideline_menu_shows_update_row_above_keybinds_when_ready() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: true,
        installed_rev: Some("aaa1111".into()),
        source_rev: Some("bbb2222".into()),
        changelog: vec!["fix(x): thing".into()],
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome));
    let labels: Vec<&str> = menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "update ready");
    assert_eq!(labels[1], "sweep threads");
    assert_eq!(labels[2], "keybinds");
    assert_eq!(menu.actions[0], AuxAction::OpenUpdate);
}

#[test]
fn sideline_menu_names_the_sweep_entry_off_dead() {
    let menu = build_sideline_menu(Anchor::Center, None);
    let i = menu
        .popup
        .rows
        .iter()
        .position(|row| {
            matches!(
                row,
                PopupRow::Entry { glyph, label, .. }
                    if glyph == "♺" && label == "sweep threads"
            )
        })
        .expect("sweep threads entry");
    let action_i = menu
        .popup
        .rows
        .iter()
        .take(i + 1)
        .filter(|row| matches!(row, PopupRow::Entry { .. }))
        .count()
        - 1;
    assert_eq!(menu.actions[action_i], AuxAction::OpenSweep);
    assert!(crate::popup::menu_glyph_is_bmp("♺"));
    assert!(!crate::popup::menu_glyph_is_bmp("📄"));
    assert_eq!(
        menu.actions
            .iter()
            .filter(|action| **action == AuxAction::Detach)
            .count(),
        1,
        "the global detach slot remains distinct"
    );
}

/// The choice modal is centered (not anchored to the menu cell), offers
/// the choices with live counts, and a zero count greys its entry
/// out rather than offering a lie. The used-shell half (x-cf97) is its
/// own row with its own count - never a rider on the tabs half.
#[test]
fn sweep_modal_is_centered_with_live_counts_and_inert_zeroes() {
    let modal = build_sweep_modal(3, 19, 7);
    assert_eq!(modal.popup.anchor, Anchor::Center);
    assert_eq!(modal.popup.targets().len(), 4, "{:?}", modal.popup.rows);
    assert_eq!(
        modal.actions,
        vec![
            AuxAction::SweepTabs,
            AuxAction::SweepUsedShells,
            AuxAction::SweepDeadAgents,
            AuxAction::SweepBoth
        ]
    );
    let labels: Vec<(String, bool)> = modal
        .popup
        .rows
        .iter()
        .filter_map(|row| match row {
            PopupRow::Entry { label, enabled, .. } => Some((label.clone(), *enabled)),
            _ => None,
        })
        .collect();
    assert!(labels.contains(&("tabs (3)".into(), true)), "{labels:?}");
    assert!(
        labels.contains(&("+ used shells (19)".into(), true)),
        "{labels:?}"
    );
    assert!(
        labels.contains(&("dead agents (7)".into(), true)),
        "{labels:?}"
    );
    assert!(labels.contains(&("both".into(), true)), "{labels:?}");

    let half = build_sweep_modal(0, 0, 2);
    assert_eq!(
        half.popup.targets().len(),
        2,
        "a zero tab count greys its entry out"
    );
    assert_eq!(
        half.actions,
        vec![AuxAction::SweepDeadAgents, AuxAction::SweepBoth]
    );

    let empty = build_sweep_modal(0, 0, 0);
    assert_eq!(empty.popup.targets().len(), 0, "{:?}", empty.popup.rows);
    assert!(empty
        .popup
        .rows
        .iter()
        .any(|row| matches!(row, PopupRow::Header(text) if text == "nothing to sweep")));
}

#[tokio::test]
async fn sweep_open_queues_one_counts_probe_and_apply_queues_scope() {
    let mut v = view_with_agents(vec![lifecycle_row("dead", true, false)]);
    v.term = (30, 100);
    let mut buf = Vec::new();
    execute_aux_action(&mut v, AuxAction::OpenSweep, &mut buf)
        .await
        .unwrap();
    assert_eq!(v.sweep_action, Some(SweepAction::Counts));
    assert!(
        v.aux.is_none(),
        "the menu closes; the modal opens on landing"
    );

    // A second tap while a verb runs says so instead of queueing: the
    // pending counts probe stays exactly as the first tap left it.
    v.sweep_inflight = true;
    execute_aux_action(&mut v, AuxAction::OpenSweep, &mut buf)
        .await
        .unwrap();
    assert_eq!(v.sweep_action, Some(SweepAction::Counts));
    assert!(v
        .notice
        .as_ref()
        .is_some_and(|(notice, _)| notice.contains("already running")));

    v.sweep_inflight = false;
    for (action, scope) in [
        (AuxAction::SweepTabs, SweepScope::Tabs),
        (AuxAction::SweepDeadAgents, SweepScope::Dead),
        (AuxAction::SweepBoth, SweepScope::Both),
    ] {
        execute_aux_action(&mut v, action, &mut buf).await.unwrap();
        assert_eq!(v.sweep_action, Some(SweepAction::Apply(scope)));
        v.sweep_action = None;
    }
}

/// The screen cell carrying an overlay's `esc close` words, taken from
/// the same framed block the click router hit-tests against.
fn overlay_footer_cell(layout: &OverlayLayout) -> (u16, u16) {
    for (li, line) in layout.framed.lines.iter().enumerate() {
        if let Some(&(_t, off, len)) = line
            .hits
            .iter()
            .find(|(t, _, _)| *t == crate::chrome::ESC_CLOSE_HIT)
        {
            return (
                (layout.origin.0 + li) as u16,
                (layout.origin.1 + off + len / 2) as u16,
            );
        }
    }
    panic!("no esc close hit span anywhere in the overlay frame");
}

fn left_click(row: u16, col: u16) -> crate::mouse::MouseReport {
    crate::mouse::MouseReport {
        row,
        col,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    }
}

/// The update modal's footer words are a target, not a label: a left
/// click on them closes the popup.
#[tokio::test]
async fn update_modal_footer_esc_close_click_closes() {
    let mut v = view_with_agents(vec![]);
    v.term = (30, 100);
    v.aux = Some(build_update_modal(None));
    let r = v.aux.as_ref().unwrap().popup.render(v.term);
    let footer = overlay_footer_cell(&OverlayLayout {
        origin: r.origin,
        framed: chrome::Framed {
            lines: r
                .lines
                .iter()
                .map(|l| chrome::FramedLine {
                    text: l.text.clone(),
                    roles: l.roles.clone(),
                    hits: l.hits.clone(),
                })
                .collect(),
            width: r.width,
        },
    });
    let mut buf: Vec<u8> = Vec::new();
    aux_mouse(&mut v, left_click(footer.0, footer.1), &mut buf)
        .await
        .unwrap();
    assert!(v.aux.is_none(), "the footer's close words closed the modal");
}

/// The Connections modal's footer words close it; any other click is
/// swallowed rather than reaching the pane underneath.
#[tokio::test]
async fn connections_modal_footer_esc_close_click_closes() {
    let mut v = view_with_agents(vec![]);
    v.term = (30, 100);
    v.connections = Some(crate::connections_view::ConnectionsView::new());
    let layout = v.active_overlay_layout().expect("connections hit layout");
    let footer = overlay_footer_cell(&layout);
    assert!(
        modal_mouse(&mut v, left_click(footer.0, footer.1)),
        "the modal owns the pointer"
    );
    assert!(
        v.connections.is_none(),
        "the footer's close words closed the modal"
    );

    // A click that is not on the close words is swallowed, not forwarded.
    v.connections = Some(crate::connections_view::ConnectionsView::new());
    let layout = v.active_overlay_layout().unwrap();
    let inside = (layout.origin.0 as u16 + 1, layout.origin.1 as u16 + 1);
    assert!(modal_mouse(&mut v, left_click(inside.0, inside.1)));
    assert!(
        v.connections.is_some(),
        "an in-body click neither dismisses nor falls through"
    );
}

/// Peek's footer words close it, and everything else keeps falling
/// through - the x7683 click-through contract only excepts the close
/// words themselves.
#[tokio::test]
async fn peek_footer_esc_close_click_closes_and_the_rest_falls_through() {
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.term = (30, 100);
    v.peek = Some(PeekView {
        cursor: agent_row_at(&v, |a| a.name == "w"),
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    });
    let layout = v.active_overlay_layout().expect("peek hit layout");
    let footer = overlay_footer_cell(&layout);
    assert!(modal_mouse(&mut v, left_click(footer.0, footer.1)));
    assert!(v.peek.is_none(), "the footer's close words closed the peek");

    // A non-close event falls through: modal_mouse returns false, so the
    // router keeps the x7683 right-press and pane-forward behavior.
    v.peek = Some(PeekView {
        cursor: agent_row_at(&v, |a| a.name == "w"),
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    });
    let layout = v.active_overlay_layout().unwrap();
    let inside = (layout.origin.0 as u16 + 1, layout.origin.1 as u16 + 1);
    assert!(
        !modal_mouse(&mut v, left_click(inside.0, inside.1)),
        "peek stays click-through off the close words"
    );
    assert!(
        v.peek.is_some(),
        "a fall-through click does not dismiss peek"
    );
}

/// AC3-HP mirrored client-side: not-ready builds the menu with no row.
#[test]
fn sideline_menu_omits_update_row_when_not_ready() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: false,
        installed_rev: Some("same".into()),
        source_rev: Some("same".into()),
        changelog: vec![],
        guidance: "up to date at same - no update pending, 0 shell(s) unaffected".into(),
        degraded: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome));
    assert!(!menu.actions.contains(&AuxAction::OpenUpdate));
}

/// AC6-EDGE: no probe yet, and a degraded probe, both build a menu that
/// stays interactive - no missing keybinds row, no panic.
#[test]
fn sideline_menu_handles_missing_and_degraded_probe() {
    let none_menu = build_sideline_menu(Anchor::Center, None);
    assert!(!none_menu.actions.contains(&AuxAction::OpenUpdate));
    assert!(none_menu.actions.contains(&AuxAction::OpenKeybinds));

    let degraded = UpdateOutcome::Degraded("update --check: exit 1".into());
    let degraded_menu = build_sideline_menu(Anchor::Center, Some(&degraded));
    let labels: Vec<&str> = degraded_menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "update check failed");
    assert_eq!(degraded_menu.actions[0], AuxAction::OpenUpdate);
}

/// Regression: a successfully-parsed probe (Python `--check` always exits
/// 0) can still be internally degraded - `update_ready: false` with
/// `degraded: Some(_)`. That must still surface a menu row rather than
/// silently falling to the `_ => {}` arm, which would hide a real check
/// failure the operator has no other way to see.
#[test]
fn sideline_menu_shows_row_for_ok_but_internally_degraded_probe() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: false,
        installed_rev: Some("same".into()),
        source_rev: Some("same".into()),
        changelog: vec![],
        guidance: "update check degraded (fno mux ls --json failed) - ...".into(),
        degraded: Some("fno mux ls --json failed".into()),
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome));
    let labels: Vec<&str> = menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "update check degraded");
    assert_eq!(menu.actions[0], AuxAction::OpenUpdate);
}

/// AC5-HP: the overlay carries the version pair, changelog, and guidance.
#[test]
fn update_modal_renders_version_pair_changelog_and_guidance() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: true,
        installed_rev: Some("aaa1111".into()),
        source_rev: Some("bbb2222".into()),
        changelog: vec!["fix(x): thing".into(), "feat(y): other thing".into()],
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
    });
    let modal = build_update_modal(Some(&outcome));
    let headers: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.contains(&"aaa1111 -> bbb2222"));
    assert!(headers.contains(&"fix(x): thing"));
    assert!(headers.contains(&"feat(y): other thing"));
    assert!(headers.iter().any(|h| h.contains("14 shells survive")));
}

/// AC6-EDGE: a degraded probe renders the reason, never an empty body.
#[test]
fn update_modal_renders_degraded_reason_never_empty() {
    let degraded = UpdateOutcome::Degraded("update --check: timed out".into());
    let modal = build_update_modal(Some(&degraded));
    assert!(!modal.popup.rows.is_empty());
    let headers: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.iter().any(|h| h.contains("timed out")));

    // No probe run yet: still a non-empty, non-panicking body.
    let none_modal = build_update_modal(None);
    assert!(!none_modal.popup.rows.is_empty());
}

/// The client-side JSON contract with `fno doctor update --check`'s
/// payload shape (`cli/src/fno/update.py::update_readiness`).
#[test]
fn update_readiness_deserializes_the_real_payload_shape() {
    let json = r#"{
            "update_ready": true,
            "installed_rev": "aaa1111", "source_rev": "bbb2222",
            "wire": {"running": [47], "source": 48, "bump": true},
            "shells": 14, "shells_ended": 14, "sessions": 2,
            "revivable": 9,
            "changelog": ["fix(bootstrap): thing"],
            "guidance": "update ready bbb2222 - WIRE BUMP v47 -> v48 - ends 14 shells",
            "degraded": null
        }"#;
    let r: UpdateReadiness = serde_json::from_str(json).unwrap();
    assert!(r.update_ready);
    assert_eq!(r.installed_rev.as_deref(), Some("aaa1111"));
    assert_eq!(r.source_rev.as_deref(), Some("bbb2222"));
    assert_eq!(r.changelog, vec!["fix(bootstrap): thing".to_string()]);
    assert!(r.guidance.contains("14 shells"));
    assert!(r.degraded.is_none());
}

#[tokio::test]
async fn peek_from_right_click_esc_returns_to_pane_not_selector() {
    // US2 review fix: peek opened standalone (right-click a row, selector
    // closed) must close back to the pane on Esc, not drop into the panel
    // selector (which assumed peek was opened from it).
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    let mut buf: Vec<u8> = Vec::new();
    assert!(v.selector.is_none());
    fetch_peek(&mut v, idx, "bg-claude".to_string(), &mut buf)
        .await
        .unwrap();
    assert!(v.peek.is_some());
    peek_keys(&mut v, b"q", &mut buf).await.unwrap();
    assert!(v.peek.is_none(), "peek closed");
    assert!(
        v.selector.is_none(),
        "did NOT drop into panel-selector mode"
    );
}

#[tokio::test]
async fn settings_toggle_preserves_keyboard_selection() {
    // US5 review fix: toggling rebuilds the modal but keeps the selection, so
    // a keyboard Enter re-toggles the SAME row instead of alternating.
    let mut v = two_pane_view();
    v.aux = Some(v.build_settings_modal());
    assert!(v.aux.as_ref().unwrap().popup.targets().len() >= 2);
    v.aux.as_mut().unwrap().popup.sel = 1; // the second toggle
    let mut buf: Vec<u8> = Vec::new();
    aux_execute_selected(&mut v, &mut buf).await.unwrap();
    assert_eq!(
        v.aux.as_ref().unwrap().popup.sel,
        1,
        "selection stays on the toggled row after the rebuild"
    );
}

#[tokio::test]
async fn sideline_menu_detach_entry_detaches() {
    let mut v = two_pane_view();
    v.open_sideline_menu(Anchor::Center);
    let idx = v
        .aux
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| *a == AuxAction::Detach)
        .unwrap();
    v.aux.as_mut().unwrap().popup.sel = idx;
    let mut buf: Vec<u8> = Vec::new();
    assert!(matches!(
        aux_execute_selected(&mut v, &mut buf).await.unwrap(),
        DispatchFlow::Detach
    ));
    assert!(v.aux.is_none());
}

#[test]
fn client_compose_notice_overlays_a_full_tab_bar() {
    let mut view = two_pane_view();
    view.term = (30, 80);
    view.layout.squads[0] = meta(1, "long-workspace", 6, 5);
    for tab in &mut view.layout.squads[0].tabs {
        tab.name = "very-long-tab-name".into();
    }
    view.set_notice("no such tab".into());

    let text = frame_text(&view.compose());
    assert!(
        text.lines().next().unwrap().contains("no such tab"),
        "the stale-refusal notice remains visible over a dense tab bar"
    );
    let notice_start = 80 - "no such tab".chars().count() - 1;
    assert!(
        view.chrome_hit(0, notice_start as u16).is_none(),
        "clicks on the visible notice do not activate hidden tabs"
    );
}

#[test]
fn client_compose_hint_lists_the_find_chord() {
    // x-653d AC5-UI: the which-key hint lists `f find` (past the width
    // budget on a narrow terminal, so composed wide here to see it).
    let mut view = two_pane_view();
    view.term = (30, 240);
    view.hint = true;
    let text = frame_text(&view.compose());
    assert!(
        text.lines().last().unwrap().contains("f find"),
        "hint lists the navigator chord"
    );
}

#[test]
fn client_abbrev_home_only_at_component_boundary() {
    assert_eq!(
        abbrev_home_in("/home/u/code", Some("/home/u")),
        "~/code".to_string()
    );
    assert_eq!(abbrev_home_in("/home/u", Some("/home/u")), "~".to_string());
    // /home/u2 must never read as ~2.
    assert_eq!(
        abbrev_home_in("/home/u2/code", Some("/home/u")),
        "/home/u2/code".to_string()
    );
    assert_eq!(abbrev_home_in("/code", None), "/code".to_string());
    assert_eq!(abbrev_home_in("/code", Some("")), "/code".to_string());
}

#[test]
fn client_apply_style_reverses_selected_cell() {
    // US2 render: a SELECTED cell emits reverse-video (XOR with the cell's
    // own inverse so selection is always a visible delta).
    let sel = Cell {
        flags: cell_flags::SELECTED,
        ..Cell::default()
    };
    let mut buf = Vec::new();
    apply_style(&mut buf, &sel).unwrap();
    assert!(
        buf.windows(4).any(|w| w == b"\x1b[7m"),
        "reverse SGR emitted"
    );
    // SELECTED over already-inverse text cancels back to non-reverse.
    let both = Cell {
        flags: cell_flags::SELECTED | cell_flags::INVERSE,
        ..Cell::default()
    };
    let mut buf2 = Vec::new();
    apply_style(&mut buf2, &both).unwrap();
    assert!(
        !buf2.windows(4).any(|w| w == b"\x1b[7m"),
        "double-inverse cancels"
    );
}

#[test]
fn client_compose_agent_rows_render_under_squads_with_badges() {
    // 4a-G2 (AC1-UI/AC2 render side): agent rows appear under their
    // squad with the fact-badge glyph; exited rows dim with the exit
    // marker over any badge; orphans land under the catch-all header;
    // the selector highlight still tracks SELECTABLE rows only.
    let mut view = two_pane_view();
    let panes = view.layout.panes.clone();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
        active_squad: 1,
        panes,
        focus: 11,
        area: (29, 72),
        agents: vec![
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: Some(1),
                name: "peer".into(),
                pane_id: Some(10),
                portal: None,
                badge: Some(AgentBadge::Blocked),
                reason: Some("perm prompt".into()),
                exited: false,
                dnd: false,
                unmeasured: false,
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
            },
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: Some(1),
                name: "dead".into(),
                pane_id: Some(99),
                portal: None,
                badge: None,
                reason: None,
                exited: true,
                dnd: false,
                unmeasured: false,
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
            },
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: None,
                name: "bg-watch".into(),
                pane_id: None,
                portal: None,
                badge: Some(AgentBadge::Working),
                reason: None,
                exited: false,
                dnd: false,
                unmeasured: false,
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
            },
        ],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view.expand_pull_sections(); // (x-c5ee) ~ elsewhere now defaults Collapsed
    let frame = view.compose();
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    // Agents-first row order (x-0090; no tab rows): footnote (auto-expanded,
    // x-2f99), its two agent rows, a Blank spacer, notes squad, the footer
    // spacer, the "+ new workspace" footer, a spacer, the "~ elsewhere"
    // header, the orphan row. (x-cd67 US1) The sideline owns row 0.
    assert!(lines[0].contains("\u{25be}*footnote"), "{:?}", lines[0]);
    assert!(
        lines[1].contains("\u{25b2} peer: perm prompt"),
        "{:?}",
        lines[1]
    );
    assert!(lines[2].contains("\u{2717} dead"), "{:?}", lines[2]);
    assert!(lines[4].contains("\u{25b8} notes"), "{:?}", lines[4]);
    assert!(lines[6].contains("+ new workspace"), "{:?}", lines[6]);
    assert!(lines[8].contains("~ elsewhere"), "{:?}", lines[8]);
    assert!(lines[9].contains("\u{25cf} bg-watch"), "{:?}", lines[9]);
    // The exited row is DIM (fact beats badge, visually too). "dead" is
    // display index 2 -> frame row 2 (no spacer before it).
    let cols = frame.cols as usize;
    let dead_cell = frame.cells[2 * cols + 2];
    assert_eq!(dead_cell.flags & cell_flags::DIM, cell_flags::DIM);
    // The selector indexes display rows directly (x-260a): index 4 = the
    // notes squad row (after footnote, its two agent rows, and the spacer).
    let notes_row = 4usize;
    let unsel_cell = frame.cells[notes_row * cols + 2];
    let mut sel_view = view;
    sel_view.selector = Some(4);
    let sel_frame = sel_view.compose();
    let sel_cell = sel_frame.cells[notes_row * cols + 2];
    // (x-4374) The notes squad is a demoted header (no standing INVERSE); the
    // selector TOGGLES INVERSE, so selecting it ADDS the band and the cursor
    // row renders DIFFERENTLY from the unselected header.
    assert_ne!(
        sel_cell.flags & cell_flags::INVERSE,
        unsel_cell.flags & cell_flags::INVERSE,
        "selector highlight must visibly toggle the notes header"
    );
}

#[test]
fn client_agent_row_renders_dnd_as_presence_not_liveness() {
    let held: AgentRow = serde_json::from_str(
        r#"{"squad":1,"name":"dnd-worker","pane_id":10,
                "badge":"working","reason":null,"exited":false,"dnd":true}"#,
    )
    .unwrap();
    let mut view = two_pane_view();
    view.layout.agents = vec![held];
    let text = frame_text(&view.compose());
    let row = text
        .lines()
        .find(|line| line.contains("dnd-worker"))
        .unwrap();
    assert!(
        row.contains("● [DND] dnd-worker"),
        "DND leads the truncatable identity without replacing liveness: {row:?}"
    );
}

#[test]
fn squad_header_rollup_counts_in_every_view_state() {
    // x-6851 US2 (AC2-HP): each squad header carries always-on per-state
    // rollup counts (nonzero only, severity order), folded from its live rows
    // every paint - subsuming x-d140's collapsed-only worst-state glyph. The
    // counts read whether the squad is collapsed OR expanded, and an
    // all-exited squad keeps its ✗ count so dead agents stay discoverable.
    fn ar(squad: u64, name: &str, badge: Option<AgentBadge>, exited: bool) -> AgentRow {
        AgentRow {
            portal: None,
            harness: None,
            model: None,
            route: None,
            reach: Reach::Locate,
            spawned_by_session: None,
            harness_session_id: None,
            squad: Some(squad),
            name: name.into(),
            pane_id: None,
            badge,
            reason: None,
            exited,
            dnd: false,
            unmeasured: false,
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
        }
    }
    let mut view = two_pane_view();
    let panes = view.layout.panes.clone();
    view.set_layout(LayoutView {
        squads: vec![
            meta(1, "footnote", 1, 0),
            meta(2, "notes", 1, 0),
            meta(3, "quiet", 1, 0),
        ],
        active_squad: 1, // only footnote auto-expands; 2/3 stay collapsed
        panes,
        focus: 11,
        area: (29, 72),
        agents: vec![
            ar(1, "lb", Some(AgentBadge::Blocked), false), // active + expanded
            ar(2, "w", Some(AgentBadge::Working), false),
            ar(2, "b", Some(AgentBadge::Blocked), false),
            ar(3, "gone", Some(AgentBadge::Blocked), true), // exited -> ✗
        ],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    let lines: Vec<String> = frame_text(&view.compose())
        .lines()
        .map(str::to_string)
        .collect();
    let find = |needle: &str| {
        lines
            .iter()
            .find(|l| l.contains(needle))
            .cloned()
            .unwrap_or_else(|| panic!("row {needle:?} not found in {lines:#?}"))
    };

    // Collapsed inactive `notes`: 1 blocked + 1 working -> `▲1 ●1`, with the
    // ▲ (more severe) ahead of the ● in the strip.
    let notes = find("\u{25b8} notes");
    assert!(
        notes.contains("\u{25b2}1") && notes.contains("\u{25cf}1"),
        "notes counts \u{25b2}1 \u{25cf}1: {notes:?}"
    );
    assert!(
        notes.find('\u{25b2}').unwrap() < notes.find('\u{25cf}').unwrap(),
        "severity order (\u{25b2} before \u{25cf}): {notes:?}"
    );

    // Collapsed `quiet`: its only agent is exited -> `✗1` (dead stays counted,
    // never silently dropped).
    let quiet = find("\u{25b8} quiet");
    assert!(
        quiet.contains("\u{2717}1"),
        "quiet keeps its exited count \u{2717}1: {quiet:?}"
    );

    // Always-on: the ACTIVE, EXPANDED `footnote` header ALSO shows its count
    // (`▲1`) - counts read in every view state, unlike the old collapsed-only
    // glyph which suppressed on expand.
    let footnote = find("\u{25be}*footnote"); // ▾*footnote (expanded caret)
    assert!(
        footnote.contains("\u{25b2}1"),
        "expanded squad still shows counts: {footnote:?}"
    );
}

#[test]
fn section_rollup_folds_nonzero_states_in_severity_order() {
    // x-6851 US2 (AC2-HP): the fold counts each state, drops zeros, and
    // orders most-severe-first (▲ ✓ ● ○ ✗).
    use LatticeState::*;
    let states = [Working, Blocked, Working, Exited, Blocked, Working];
    let rollup = section_rollup(states.into_iter());
    assert_eq!(rollup, vec![(Blocked, 2), (Working, 3), (Exited, 1)]);
    // No zero pairs leak in (no Idle / DoneUnseen here).
    assert!(rollup.iter().all(|&(_, n)| n > 0));
    // An empty section yields an empty strip.
    assert!(section_rollup(std::iter::empty()).is_empty());
}

#[test]
fn header_band_text_truncates_least_severe_first_then_name() {
    // x-6851 US2 (AC11-EDGE): pairs drop atomically from the least-severe
    // (✗) end when the panel is too narrow; a glyph never renders without its
    // count; the name truncates only after every pair is gone.
    use LatticeState::*;
    let rollup = [(Blocked, 2), (Working, 3), (Exited, 1)];
    // Wide enough for everything: label left, counts right, exact width.
    let wide = header_band_text("sq", &rollup, 20);
    assert_eq!(wide.chars().count(), 20);
    assert!(wide.starts_with("sq") && wide.ends_with("\u{25b2}2 \u{25cf}3 \u{2717}1"));
    // Room for the two most-severe pairs only: the ✗ pair drops whole (no
    // orphan glyph), ▲ and ● survive. (All three need width 11; at 10 the ✗
    // pair must go.)
    let mid = header_band_text("sq", &rollup, 10);
    assert!(mid.contains("\u{25b2}2") && mid.contains("\u{25cf}3"));
    assert!(
        !mid.contains('\u{2717}'),
        "least-severe pair dropped whole: {mid:?}"
    );
    // Too narrow for any pair: all drop, the name renders (truncated by
    // pad_to only once every pair is gone).
    let narrow = header_band_text("a-very-long-section-name", &rollup, 8);
    assert!(!narrow.contains('\u{25b2}') && !narrow.contains('\u{2717}'));
    assert_eq!(narrow.chars().count(), 8);
}

#[test]
fn headers_demoted_and_focused_row_wears_the_band() {
    // x-4374 (AC1-HP, was header_band_is_inverse_and_agent_rows_are_not):
    // headers lose the always-on INVERSE band (active squad keeps BOLD,
    // inactive renders plain), and the full-width band moves to the agent row
    // that owns the focused pane. Exactly one standing band, and it is the
    // focus row.
    let mut view = two_pane_view();
    let panes = view.layout.panes.clone();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 1, 1), meta(2, "notes", 1, 0)],
        active_squad: 1,
        panes,
        focus: 11,
        area: (29, 72),
        agents: vec![blocked_row("lb", 11, None)], // owns the focused pane 11
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    let (rows, cols, panel_w) = (29usize, 72usize, 28usize);
    let mut cells = vec![Cell::default(); rows * cols];
    view.draw_sideline(&mut cells, rows, cols, panel_w);
    // Row 0 = active squad header: demoted - BOLD, NO INVERSE.
    assert_eq!(
        cells[0].flags & cell_flags::INVERSE,
        0,
        "the active header no longer paints a standing band"
    );
    assert_eq!(cells[0].flags & cell_flags::BOLD, cell_flags::BOLD);
    // Row 1 = the agent row owning the focused pane: the sole standing band.
    assert_eq!(
        cells[cols].flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the focused row wears the full-width band"
    );
    // The band spans the full width (a right-edge text cell is still INVERSE).
    assert_eq!(
        cells[cols + panel_w - 2].flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "band fills the panel width"
    );
    // Row 2 = the Blank spacer between squads (inert, no INVERSE). Row 3 =
    // inactive `notes` header: demoted to plain - NO INVERSE, NO DIM.
    assert_eq!(cells[2 * cols].flags & cell_flags::INVERSE, 0);
    assert_eq!(
        cells[3 * cols].flags & cell_flags::INVERSE,
        0,
        "the inactive header no longer paints a standing band"
    );
    assert_eq!(
        cells[3 * cols].flags & cell_flags::DIM,
        0,
        "the inactive header is plain, not DIM (present, not disabled)"
    );
}

#[test]
fn tab_badge_marks_only_rows_on_other_tabs() {
    // x-4374 (AC6): the tab badge means "this session lives on a tab you are
    // not looking at". A row whose pane is in the viewer's active (squad, tab)
    // drops the badge; a row in a background named tab keeps it.
    let mut view = two_pane_view();
    let panes = view.layout.panes.clone();
    let mut footnote = meta(1, "footnote", 2, 0); // active tab = id 0
    footnote.tabs[1].name = "reviews".into();
    footnote.tabs[1].named = true;
    let here = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        name: "here".into(),
        tab: Some(0), // the viewer's active tab -> no badge
        ..focus_agent(0)
    };
    let elsewhere = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        name: "elsewhere".into(),
        tab: Some(1), // a background tab -> badge
        ..focus_agent(0)
    };
    view.set_layout(LayoutView {
        squads: vec![footnote],
        active_squad: 1,
        panes,
        focus: 999, // no row owns focus; keep the band out of this test
        area: (29, 72),
        agents: vec![here, elsewhere],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    let (rows, cols, panel_w) = (29usize, 72usize, 28usize);
    let mut cells = vec![Cell::default(); rows * cols];
    view.draw_sideline(&mut cells, rows, cols, panel_w);
    let row_text =
        |r: usize| -> String { (0..panel_w - 1).map(|c| cells[r * cols + c].c).collect() };
    // Row 0 = squad header, row 1 = "here", row 2 = "elsewhere" (single squad,
    // so no spacer precedes the rows).
    let here_line = row_text(1);
    let elsewhere_line = row_text(2);
    assert!(here_line.contains("here"), "sanity: {here_line:?}");
    assert!(
        !here_line.contains('·'),
        "the active-tab row drops the badge: {here_line:?}"
    );
    assert!(
        elsewhere_line.contains("·reviews"),
        "the background-tab row keeps its badge: {elsewhere_line:?}"
    );
}

#[test]
fn focus_change_scrolls_the_band_into_view() {
    // x-4374 (AC auto-scroll): when focus moves to a row below the fold, the
    // sideline scrolls the least it takes to reveal the focused-row band; a
    // top-row focus needs no scroll.
    let mut view = two_pane_view();
    view.term = (6, 100); // a short panel: fewer visible rows than total
    let panes = view.layout.panes.clone();
    let agents: Vec<AgentRow> = (0..8)
        .map(|i| AgentRow {
            harness: None,
            model: None,
            route: None,
            name: format!("a{i}"),
            pane_id: Some(100 + i),
            portal: None,
            ..focus_agent(0)
        })
        .collect();
    let layout = |focus: u64, agents: Vec<AgentRow>| LayoutView {
        squads: vec![meta(1, "footnote", 1, 0)],
        active_squad: 1,
        panes: panes.clone(),
        focus,
        area: (5, 72),
        agents,
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    };
    // Focus on the top agent row's pane: it already fits, so no scroll.
    view.set_layout(layout(100, agents.clone()));
    assert_eq!(view.sideline_offset, 0, "a top focus needs no scroll");
    // Focus jumps to the last agent (pane 107), well below the fold.
    view.set_layout(layout(107, agents.clone()));
    let visible = view.sideline_visible_rows();
    let idx = view
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if a.pane_id == Some(107)))
        .unwrap();
    assert!(
        idx >= view.sideline_offset && idx < view.sideline_offset + visible,
        "focused row {idx} is inside the window [{}, {})",
        view.sideline_offset,
        view.sideline_offset + visible
    );
    assert!(
        view.sideline_offset > 0,
        "the sideline scrolled to reveal the off-screen focus"
    );
}

#[test]
fn focus_reveal_never_scrolls_an_open_selector_off_screen() {
    // x-4374 (codex P2): an open selector owns the scroll - `clamp_sideline_offset`
    // keeps that actionable cursor visible, and a focus change must NOT scroll
    // it off-screen (Enter/lifecycle keys would then act on an invisible row).
    let mut view = two_pane_view();
    view.term = (6, 100);
    let panes = view.layout.panes.clone();
    let agents: Vec<AgentRow> = (0..8)
        .map(|i| AgentRow {
            harness: None,
            model: None,
            route: None,
            name: format!("a{i}"),
            pane_id: Some(100 + i),
            portal: None,
            ..focus_agent(0)
        })
        .collect();
    let layout = |focus: u64, agents: Vec<AgentRow>| LayoutView {
        squads: vec![meta(1, "footnote", 1, 0)],
        active_squad: 1,
        panes: panes.clone(),
        focus,
        area: (5, 72),
        agents,
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    };
    view.set_layout(layout(100, agents.clone()));
    // Park the selector on the top agent row (display index 1) and pin the view.
    view.selector = Some(1);
    view.clamp_sideline_offset();
    let visible = view.sideline_visible_rows();
    // Focus jumps far below the fold - with a selector open, reveal must not fire.
    view.set_layout(layout(107, agents.clone()));
    let sel = view.selector.expect("selector still open");
    assert!(
        sel >= view.sideline_offset && sel < view.sideline_offset + visible,
        "the selector {sel} stays visible in [{}, {})",
        view.sideline_offset,
        view.sideline_offset + visible
    );
}

#[test]
fn footer_buttons_rest_bold_and_invert_on_hover() {
    // The footer buttons rest BOLD; DIM is reserved for inert rows.
    let mut view = two_pane_view();
    view.term = (29, 72);
    let (rows, cols, panel_w) = (29usize, 72usize, 28usize);
    let footer = view
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::NewSquad))
        .unwrap();
    let at = |v: &View| {
        let mut cells = vec![Cell::default(); rows * cols];
        v.draw_sideline(&mut cells, rows, cols, panel_w);
        cells[(footer - v.sideline_offset) * cols].flags
    };

    let rest = at(&view);
    assert_eq!(rest & cell_flags::BOLD, cell_flags::BOLD);
    assert_eq!(rest & cell_flags::DIM, 0, "DIM reads as disabled");
    assert_eq!(rest & cell_flags::INVERSE, 0);

    // The `N marked ·R` variant rides the same row and the same style.
    let mut marked = two_pane_view();
    marked.term = (29, 72);
    marked.marks.insert("a1".to_string());
    let marked_flags = at(&marked);
    assert_eq!(marked_flags & cell_flags::BOLD, cell_flags::BOLD);
    assert_eq!(marked_flags & cell_flags::DIM, 0);

    // Hover still toggles INVERSE on top of BOLD (the row is not inert).
    view.hover_row = Some(footer);
    let hovered = at(&view);
    assert_eq!(hovered & cell_flags::INVERSE, cell_flags::INVERSE);
    assert_eq!(hovered & cell_flags::BOLD, cell_flags::BOLD);
}

#[test]
fn zero_agent_squad_band_has_no_counts() {
    // x-6851 US1 (AC4-EDGE): a squad with no agents renders its band with no
    // count glyphs and no rows.
    let view = two_pane_view(); // squad 1/2 have no agents
    let lines: Vec<String> = frame_text(&view.compose())
        .lines()
        .map(str::to_string)
        .collect();
    let footnote = lines
        .iter()
        .find(|l| l.contains("\u{25be}*footnote"))
        .unwrap();
    for g in ['\u{25b2}', '\u{2713}', '\u{25cf}', '\u{25cb}', '\u{2717}'] {
        assert!(
            !footnote.contains(g),
            "empty squad has no count glyph: {footnote:?}"
        );
    }
}

#[test]
fn external_live_row_is_dim_and_distinct_from_exited_and_fno_live() {
    // x-0a2e AC1-UI: the three sideline row kinds are pairwise distinct -
    // `✗`+DIM (exited), `·`+DIM (external, roster-surfaced live), `·` bright
    // (fno-owned live). External dims a live `·` row without stealing the
    // exit glyph or the bright-live glyph.
    let mut view = two_pane_view();
    let panes = view.layout.panes.clone();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 1, 1)],
        active_squad: 1,
        panes,
        focus: 11,
        area: (29, 72),
        agents: vec![
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: None,
                name: "z-exited".into(),
                pane_id: None,
                portal: None,
                badge: None,
                reason: None,
                exited: true,
                dnd: false,
                unmeasured: false,
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
            },
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: None,
                name: "z-external".into(),
                pane_id: None,
                portal: None,
                badge: None,
                reason: None,
                exited: false,
                dnd: false,
                unmeasured: false,
                answerable: None,
                attach_id: Some("ab12cd34".into()),
                external: true,
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
            },
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: None,
                name: "z-fnolive".into(),
                pane_id: None,
                portal: None,
                badge: None,
                reason: None,
                exited: false,
                dnd: false,
                unmeasured: false,
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
            },
            // x-df4c AC1-UI: an EXTERNAL row that is also Blocked - the
            // load-bearing "attention is never dimmed" branch. The accent
            // must win over the external DIM modifier.
            AgentRow {
                harness: None,
                model: None,
                route: None,
                reach: Reach::Locate,
                spawned_by_session: None,
                harness_session_id: None,
                squad: None,
                name: "z-extblocked".into(),
                pane_id: None,
                portal: None,
                badge: Some(AgentBadge::Blocked),
                reason: None,
                exited: false,
                dnd: false,
                unmeasured: false,
                answerable: None,
                attach_id: Some("ff99ff99".into()),
                external: true,
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
            },
        ],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    view.expand_pull_sections(); // (x-c5ee) ~ elsewhere now defaults Collapsed
    let frame = view.compose();
    let cols = frame.cols as usize;
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    // Locate each row by name and read its glyph cell (col 2) + DIM flag.
    let probe = |needle: &str| -> (char, bool) {
        let r = lines.iter().position(|l| l.contains(needle)).unwrap();
        let cell = frame.cells[r * cols + 2];
        (cell.c, cell.flags & cell_flags::DIM == cell_flags::DIM)
    };
    // x-df4c: idle was the outline `○` (was the near-invisible `·`); x-d401
    // moves a badgeless reading-less row to the marked absence `?` - the
    // mux cannot see an external/fno live row's workload, so it says so.
    // The external DIM modifier and the no-reading DIM coincide, and this
    // is a KNOWN, accepted collision, not an oversight: a badgeless local
    // row and a badgeless external row both paint `?` + DIM, where before
    // they were a bright `○` and a dim `○`. What the glyph discriminates
    // is STATE, never external-ness, and DIM only reinforces it - the
    // earlier wording read as though the glyph told the two rows apart,
    // which it does not. External-ness reads through the row's ACTIONS.
    // Restoring the distinction here means giving `external` a channel of
    // its own; DIM cannot carry it once the state is already dim, which
    // was already true of `✗` before this branch. The exited precedence
    // below is unchanged.
    assert_eq!(probe("z-exited"), ('\u{2717}', true), "exited: ✗ + DIM");
    assert_eq!(probe("z-external"), ('?', true), "external: ? + DIM");
    assert_eq!(
        probe("z-fnolive"),
        ('?', true),
        "fno-live: ? + DIM (no reading; bright-idle is gone)"
    );
    // AC1-UI: external + Blocked renders the amber `▲`, BOLD, and NOT dimmed
    // even though it is external - the accent beats the external DIM.
    let eb_row = lines
        .iter()
        .position(|l| l.contains("z-extblocked"))
        .unwrap();
    let eb = frame.cells[eb_row * cols + 2];
    assert_eq!(eb.c, '\u{25b2}', "external-blocked: ▲");
    assert_eq!(eb.fg, LATTICE_ACCENT, "external-blocked: amber accent");
    assert_eq!(
        eb.flags & cell_flags::DIM,
        0,
        "external-blocked: attention is never dimmed"
    );
    assert_eq!(
        eb.flags & cell_flags::BOLD,
        cell_flags::BOLD,
        "external-blocked: BOLD"
    );
}

#[test]
fn client_compose_panel_autohides_below_min_width() {
    let mut view = two_pane_view();
    // AC6-EDGE: on a 60-col terminal a Regular tree cannot leave MIN_CONTENT
    // (max = 20 < min_admit_width(Regular) = 28), so the rail auto-hides and
    // content takes the full width - the shipped narrow-terminal behaviour,
    // preserved through the free-width change (x-2e86 admits a density only
    // when the terminal can show it usefully; a squished tree yields to the
    // panes).
    view.term = (30, 60);
    assert_eq!(view.panel_w(), 0);
    // 30 rows minus tab bar + status row (both visible at this height).
    assert_eq!(view.content_dims(), (28, 60));
    let frame = view.compose();
    let text = frame_text(&frame);
    let row1 = text.lines().nth(1).unwrap();
    assert!(
        row1.starts_with('a'),
        "content must start at column 0 when the panel hides: {row1:?}"
    );
}

#[test]
fn client_compose_agents_first_omits_tab_rows_and_highlights_squad() {
    // x-0090 (Locked 4): tab rows left the sideline. The active squad arrives
    // expanded (View::new seeds it, x-2f99) but two_pane_view has no agents,
    // so its expanded body is empty and the SelRows are just the squad names.
    let mut view = two_pane_view();
    view.selector = Some(2); // squad 2's name row (x-cd67 US3: Blank spacer at 1)
    let sel: Vec<SelRow> = view
        .display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Sel(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(
        sel,
        vec![
            SelRow {
                squad: 1,
                tab: None
            },
            SelRow {
                squad: 2,
                tab: None
            },
        ]
    );
    let frame = view.compose();
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    // (x-cd67 US1 owns row 0; US3 Blank spacer at line 1) squad 1 leads line
    // 0, squad 2 follows on line 2.
    assert!(lines[0].contains("▾*footnote"), "{:?}", lines[0]);
    assert!(
        lines[2].contains("▸ notes"),
        "next squad follows the spacer, no tab rows: {:?}",
        lines[2]
    );
    assert!(
        !lines.iter().any(|l| l.contains("*2")),
        "no active-tab row renders in the sideline"
    );
    // The selector row (squad 2, display index 2 -> frame row 2). squad 2 is
    // an inactive header band (INVERSE+DIM); the selector TOGGLES INVERSE
    // (x-6851 US1), so it must render DIFFERENTLY from the same row
    // unselected rather than simply carrying INVERSE.
    let cols = frame.cols as usize;
    let unsel_frame = two_pane_view().compose();
    assert_ne!(
        frame.cells[2 * cols].flags & cell_flags::INVERSE,
        unsel_frame.cells[2 * cols].flags & cell_flags::INVERSE,
        "selector cursor row must be visibly toggled"
    );
    // While the selector is open the terminal cursor hides.
    assert!(!frame.cursor_visible);
}

#[test]
fn client_compose_ignores_stale_frames_and_clips_overflow() {
    let mut view = two_pane_view();
    // A frame bigger than its rect (resize in flight) must clip, not
    // panic or bleed into the divider.
    view.frames.insert(10, text_frame(40, 60, 'X'));
    let frame = view.compose();
    let text = frame_text(&frame);
    let row1: Vec<char> = text.lines().nth(1).unwrap().chars().collect();
    assert_eq!(row1[28 + 34], 'X', "last in-rect column draws");
    assert_eq!(row1[28 + 35], '│', "divider survives an oversized frame");
    // set_layout drops frames for panes the new Layout does not know.
    let mut view = two_pane_view();
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 1, 0)],
        active_squad: 1,
        panes: vec![(
            10,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 10,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert!(view.frames.contains_key(&10));
    assert!(
        !view.frames.contains_key(&11),
        "frames for dead panes are dropped at Layout"
    );
}

#[test]
fn client_compose_letterboxes_beyond_the_clamped_area() {
    // AC1-UI: a 29x72 local content area showing a tab clamped to 20x50
    // - content anchors top-left, everything beyond is dim '·' filler,
    // and the cursor never enters the filler.
    let mut view = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: vec![meta(1, "footnote", 1, 0)],
            active_squad: 1,
            panes: vec![(
                10,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 20,
                    cols: 50,
                },
            )],
            focus: 10,
            area: (20, 50),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    view.frames.insert(10, text_frame(20, 50, 'a'));
    let frame = view.compose();
    let cols = frame.cols as usize;
    // In-area content cell.
    assert_eq!(frame.cells[cols + 28].c, 'a');
    // One column beyond the area: filler, dim.
    let beyond_col = &frame.cells[cols + 28 + 50];
    assert_eq!(beyond_col.c, '·', "beyond-area column must be filler");
    assert!(beyond_col.flags & cell_flags::DIM != 0);
    // One row beyond the area (content row 20): filler too.
    let beyond_row = &frame.cells[(1 + 20) * cols + 28];
    assert_eq!(beyond_row.c, '·', "beyond-area row must be filler");
    // Cursor confined to content even against a lying frame cursor.
    assert!(frame.cursor_row < 1 + 20 && frame.cursor_col < 28 + 50);
}

#[test]
fn client_selector_fold_handles_split_escape_sequences() {
    // Gemini medium: an arrow sequence split across reads must fold into
    // one nav key - never a bare-Esc close plus leaked tail bytes.
    let mut esc = Vec::new();
    let mut keys = Vec::new();
    for chunk in [&b"\x1b"[..], &b"["[..], &b"B"[..]] {
        keys.extend(fold_selector_keys(&mut esc, chunk));
    }
    assert_eq!(keys, b"j".to_vec());
    assert!(esc.is_empty());
    // Whole-chunk arrows and hjkl mix.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b[Aj\x1b[C"), b"kjl");
    // A bare Esc resolves on the NEXT byte (which is swallowed).
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b"), b"");
    assert_eq!(esc, vec![0x1b], "lone ESC stays pending");
    assert_eq!(fold_selector_keys(&mut esc, b"x"), vec![0x1b]);
    assert!(esc.is_empty());
    // Unknown sequences are swallowed whole, selector unaffected.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b[Z"), b"");
}

#[test]
fn client_selector_fold_swallows_a_whole_parameterised_csi() {
    // "swallowed whole" was a comment, not a behaviour: only the first byte
    // after `ESC [` was dropped, so a modified arrow leaked its tail as
    // plain keys. Harmless while every overlay closed on an unrecognised
    // key; destructive once a picker gained a cursor, because the leaked
    // bytes include DIGITS (which commit in the move picker) and UPPERCASE
    // LETTERS (which commit a split in the attach picker).
    for (name, seq) in [
        ("ctrl-up", &b"\x1b[1;5A"[..]),
        ("ctrl-left", &b"\x1b[1;5D"[..]),
        ("shift-home", &b"\x1b[1;5H"[..]),
        ("f5", &b"\x1b[15~"[..]),
        ("page-up", &b"\x1b[5~"[..]),
        ("mouse-ish", &b"\x1b[<35;80;24M"[..]),
    ] {
        let mut esc = Vec::new();
        assert_eq!(
            fold_selector_keys(&mut esc, seq),
            Vec::<u8>::new(),
            "{name} must leak nothing"
        );
        assert!(esc.is_empty(), "{name} leaves no carry");
    }

    // A parameterised arrow is NOT aliased onto the plain one: it means
    // something this layer has no mapping for, so it is dropped rather than
    // silently acted on as an unmodified press.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b[1;5B"), Vec::<u8>::new());

    // Still split-safe: a parameterised sequence broken across reads leaks
    // nothing either, and a real key after it still arrives.
    let mut esc = Vec::new();
    let mut keys = Vec::new();
    for chunk in [&b"\x1b[1"[..], &b";5"[..], &b"A"[..], &b"j"[..]] {
        keys.extend(fold_selector_keys(&mut esc, chunk));
    }
    assert_eq!(keys, b"j".to_vec(), "only the real keypress survives");
    assert!(esc.is_empty());
}

#[test]
fn every_escape_fold_swallows_an_unrecognised_sequence_whole() {
    // PARITY over all four folds, deliberately not four separate tests.
    // The leak-whole-sequence guarantee was only ever asserted against
    // `fold_search_input`, and the two folds nobody tested were the two that
    // leaked: `fold_selector_keys` and `fold_modal_keys` each dropped ONE
    // byte after `ESC [` and let the tail out as plain keys.
    //
    // That was survivable while every overlay closed on a key it did not
    // recognise. Giving the pickers cursors is what weaponised it, because
    // the leaked bytes are exactly the ones that now commit: a digit commits
    // a MoveTab, and `ESC [ 1 ; 5 H` ends in the `H` that commits an attach
    // split. A capability upgrade turned a dormant defect destructive.
    //
    // Written as one sweep so a FIFTH fold added later inherits the
    // guarantee instead of inheriting nothing.
    let sequences: &[(&str, &[u8])] = &[
        ("ctrl-up", b"\x1b[1;5A"),
        ("ctrl-down", b"\x1b[1;5B"),
        ("ctrl-home", b"\x1b[1;5H"),
        ("shift-tab-ish", b"\x1b[1;2Z"),
        ("f5", b"\x1b[15~"),
        ("f12", b"\x1b[24~"),
        ("sgr-mouse", b"\x1b[<35;80;24M"),
        ("unknown-final", b"\x1b[?1049h"),
    ];
    // Each fold reduced to "how many keys did this leak", so one loop covers
    // four different return types.
    type Fold = (&'static str, fn(&mut Vec<u8>, &[u8]) -> usize);
    let folds: &[Fold] = &[
        ("fold_modal_keys", |e, b| fold_modal_keys(e, b).len()),
        ("fold_selector_keys", |e, b| fold_selector_keys(e, b).len()),
        ("fold_search_input", |e, b| fold_search_input(e, b).len()),
        ("fold_nav_input", |e, b| fold_nav_input(e, b).len()),
    ];
    for (fold_name, fold) in folds {
        for (seq_name, seq) in sequences {
            // Whole sequence in one read.
            let mut esc = Vec::new();
            assert_eq!(fold(&mut esc, seq), 0, "{fold_name} leaked on {seq_name}");
            assert!(esc.is_empty(), "{fold_name} left carry after {seq_name}");

            // ...and split at every boundary, since a real terminal read can
            // cut anywhere and a fold that only works on whole chunks is not
            // actually safe.
            for cut in 1..seq.len() {
                let mut esc = Vec::new();
                let leaked = fold(&mut esc, &seq[..cut]) + fold(&mut esc, &seq[cut..]);
                assert_eq!(leaked, 0, "{fold_name} leaked on {seq_name} split at {cut}");
                assert!(
                    esc.is_empty(),
                    "{fold_name} left carry after split {seq_name} at {cut}"
                );
            }
        }
        // A pathological parameter run is dropped rather than growing the
        // carry without limit.
        let mut esc = Vec::new();
        let flood: Vec<u8> = b"\x1b[".iter().copied().chain([b'1'; 500]).collect();
        fold(&mut esc, &flood);
        assert!(
            esc.len() <= MAX_ESC_CARRY,
            "{fold_name} carry grew to {} on a parameter flood",
            esc.len()
        );
    }
}

#[test]
fn client_selector_fold_abandons_a_malformed_csi_and_frees_the_cancel() {
    // A CSI carry must never swallow the operator's escape hatch. Alt-`[`
    // emits exactly `ESC [`, which leaves a truncated sequence in the carry.
    // Treating every non-final byte as a parameter would then absorb the Esc
    // meant to cancel, and absorb the following `q` too (it is in the
    // final-byte range) - the cancel eaten twice. ECMA-48 parameter and
    // intermediate bytes are 0x20-0x3f, so a C0 control is malformed and
    // ends the sequence instead.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b["), Vec::<u8>::new());
    assert_eq!(esc, vec![0x1b, b'['], "the truncated CSI is pending");
    // Esc abandons it and starts a fresh bare-Esc, which resolves on the
    // next byte exactly as a normal bare Esc does.
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b"), Vec::<u8>::new());
    assert_eq!(
        esc,
        vec![0x1b],
        "the stale CSI is gone, a bare Esc is armed"
    );
    assert_eq!(
        fold_selector_keys(&mut esc, b"q"),
        vec![0x1b],
        "the cancel reaches the overlay instead of being eaten"
    );

    // A non-Esc control mid-sequence is delivered rather than swallowed.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b[1;5"), Vec::<u8>::new());
    assert_eq!(
        fold_selector_keys(&mut esc, b"\r"),
        b"\r".to_vec(),
        "Enter mid-sequence is not lost"
    );
    assert!(esc.is_empty(), "and the carry is released");
}

#[tokio::test]
async fn move_picker_ignores_a_modified_arrow_instead_of_moving_a_tab() {
    // The regression probe, at the door rather than the fold: Ctrl-Up used
    // to leak `;`, `5`, `A`, and the `5` COMMITTED a MoveTab into whatever
    // squad happened to be fifth. Before the cursor landed those bytes only
    // closed the picker, so this was a change from harmless to destructive.
    for seq in [&b"\x1b[1;5A"[..], &b"\x1b[15~"[..]] {
        let mut v = two_pane_view();
        widen_to_squads(&mut v, 14);
        v.selector = Some(0);
        selector_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        move_pick_keys(&mut v, seq, &mut buf).await.unwrap();
        assert!(buf.is_empty(), "{seq:?} must send no MoveTab");
        assert!(v.move_pick.is_some(), "{seq:?} leaves the picker open");
        assert_eq!(v.move_pick.as_ref().unwrap().cursor, 0, "and unmoved");
    }
}

#[tokio::test]
async fn attach_picker_ignores_a_modified_arrow_instead_of_attaching() {
    // Same leak, other door. `ESC [ 1 ; 5 H` ends in the letter `H`, which
    // this PR made a commit key, so the tail committed an attach splitting
    // left - the exact defect class this PR exists to remove, reached
    // through a key nobody thinks of as a split.
    for seq in [&b"\x1b[1;5H"[..], &b"\x1b[1;5A"[..]] {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf: Vec<u8> = Vec::new();
        attach_place_keys(&mut v, seq, &mut buf).await.unwrap();
        assert!(buf.is_empty(), "{seq:?} must send no AttachAgent");
        let picker = v.attach_place.as_ref().expect("picker stays open");
        assert_eq!(picker.cursor, 0, "{seq:?} moves nothing");
    }
}

#[test]
fn client_selector_rows_reanchor_on_catalog_shrink() {
    let mut view = two_pane_view();
    view.selector = Some(3);
    // AC6-FR: the catalog shrinks (squad 1 gone); the cursor re-anchors
    // to a live row instead of pointing off the end.
    view.set_layout(LayoutView {
        squads: vec![meta(2, "notes", 1, 0)],
        active_squad: 2,
        panes: vec![(
            20,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 20,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    // Display rows are now [notes squad (auto-expanded, no agents),
    // + new workspace] (x-0090: no tab rows): the cursor clamps to the last
    // live row (the footer, an actionable stop).
    assert_eq!(view.selector, Some(1), "cursor clamped to the live rows");
}

// ---- x-260a: unified selector rows (keyboard reaches every actionable row) ----

/// A sideline with every row kind: squad 1 + its hosted agent, squad 2,
/// the footer, an orphan-agents section (attachable bg + watch-only), and
/// a work-queue lane (ready + blocked cards).
///
/// Display rows (x-0090 agents-first; sq1 active/expanded, no tab rows;
/// x-cd67 US3 adds Blank spacers between groups and before the trailing
/// headers since there are 2 squads):
/// 0 sq1 · 1 hosted agent · 2 Blank · 3 sq2 · 4 "+ new workspace" ·
/// 5 Blank · 6 "~ elsewhere" · 7 bg-attach · 8 bg-plain · 9 Blank ·
/// 10 "~ backlog" · 11 ready card · 12 blocked card · 13 in-flight card.
fn unified_rows_view() -> View {
    let agent = |squad: Option<u64>, name: &str, pane_id, attach_id: Option<&str>| AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad,
        name: name.into(),
        pane_id,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        answerable: None,
        attach_id: attach_id.map(Into::into),
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
    let card = |id: &str, state| BacklogCard {
        id: id.into(),
        slug: String::new(),
        priority: "p2".into(),
        state,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    };
    let mut v = view_with_agents(vec![
        agent(Some(1), "worker", Some(10), None),
        agent(None, "bg-claude", None, Some("c19cd2c3")),
        agent(None, "bg-other", None, None),
    ]);
    v.layout.backlog = vec![
        card("x-rdy", CardState::Ready),
        card("x-blk", CardState::Blocked),
        card("x-fly", CardState::InFlight),
    ];
    // (x-c5ee) These tests exercise rows INSIDE `~ elsewhere` and `~ backlog`
    // (peek, attach, selector nav, card actions), so expand both past their
    // new Collapsed defaults - the collapse itself has its own AC test.
    v.section_view
        .insert(SectionKey::Elsewhere, SectionView::Expanded);
    v.section_view
        .insert(SectionKey::WorkQueue, SectionView::Expanded);
    v
}

// ---- the Backlog section (x-1d91) --------------------------------------

/// A view whose Backlog section holds `cards` out of `total` on the board
/// (all in one lane unless the cards say otherwise).
fn backlog_view(cards: Vec<BacklogCard>, total: usize) -> View {
    let mut v = two_pane_view();
    v.set_layout(backlog_layout(cards, total));
    // (x-c5ee) `~ backlog` now defaults Collapsed; these tests assert on
    // rendered card rows, so open it. The collapse has its own AC test.
    v.section_view
        .insert(SectionKey::WorkQueue, SectionView::Expanded);
    v
}

fn backlog_layout(cards: Vec<BacklogCard>, total: usize) -> LayoutView {
    let mut layout = two_squad_layout(1);
    let lane = cards
        .first()
        .map(|c| card_lane(c).to_string())
        .unwrap_or_else(|| crate::backlog_view::UNLANED.into());
    layout.backlog = cards;
    layout.backlog_lanes = vec![(lane, total)];
    layout
}

fn bcard(id: &str, state: CardState) -> BacklogCard {
    BacklogCard {
        id: id.into(),
        slug: format!("{id}-slug"),
        priority: "p2".into(),
        state,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    }
}

fn sublines(v: &View) -> Vec<String> {
    v.display_rows()
        .into_iter()
        .filter_map(|r| match r {
            DisplayRow::Sub(s) => Some(s),
            _ => None,
        })
        .collect()
}

#[test]
fn section_header_reads_backlog() {
    // AC1-HP: the section is titled "Backlog", not "work queue".
    let v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    assert!(
        v.display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Header { label, .. } if label.contains("backlog"))),
        "the section header names the Backlog"
    );
}

#[test]
fn card_attribution_subline_renders_present_halves_only() {
    // AC1-HP: `project · lane` on line 2 - but an unscoped, unlaned card
    // stays ONE row rather than emitting a blank subline, and either half
    // alone renders without a dangling separator.
    let with = |p: Option<&str>, l: Option<&str>| {
        let mut c = bcard("x-a", CardState::Ready);
        c.project = p.map(Into::into);
        c.lane = l.map(Into::into);
        c
    };
    assert_eq!(
        card_attribution(&with(Some("fno"), Some("ready"))).as_deref(),
        Some("fno · ready")
    );
    assert_eq!(
        card_attribution(&with(Some("fno"), None)).as_deref(),
        Some("fno")
    );
    assert_eq!(
        card_attribution(&with(None, Some("ready"))).as_deref(),
        Some("ready")
    );
    assert_eq!(card_attribution(&with(None, None)), None);
    // And the subline actually reaches display_rows.
    let v = backlog_view(vec![with(Some("fno"), Some("ready"))], 1);
    assert!(sublines(&v).contains(&"fno · ready".to_string()));
}

#[test]
fn overflow_line_states_the_exact_remainder() {
    // AC5-EDGE: the count is total-minus-shown exactly, and no line appears
    // when the whole board fits.
    let cards = vec![
        bcard("x-a", CardState::Ready),
        bcard("x-b", CardState::Blocked),
    ];
    let v = backlog_view(cards.clone(), 57);
    assert!(
        sublines(&v).contains(&"+55 more".to_string()),
        "57 on the board, 2 shown -> +55"
    );
    let exact = backlog_view(cards, 2);
    assert!(
        !sublines(&exact).iter().any(|s| s.ends_with("more")),
        "nothing cut -> no remainder line"
    );
}

#[tokio::test]
async fn card_menu_float_sends_verb_and_arms_pending() {
    // AC2-HP + AC3-UI: the menu's float entry sends BacklogVerb::RankTop for
    // the pinned node and the card immediately wears the pending marker.
    let mut v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    let i = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Card(_)))
        .expect("a card row");
    assert!(v.open_row_menu(i, Anchor::Center), "cards open the menu");
    let mut wire = Vec::new();
    row_menu_execute_selected(&mut v, &mut wire).await.unwrap();
    let sent = String::from_utf8_lossy(&wire);
    assert!(sent.contains("BacklogVerb") && sent.contains("RankTop") && sent.contains("x-a"));
    assert!(v.card_pending("x-a"), "the card shows it dispatched");
    assert!(v.row_menu.is_none(), "the menu closes after execute");
}

#[tokio::test]
async fn card_menu_refuses_a_card_that_left_the_feed() {
    // Concurrency: a card claimed/dispatched between menu-open and Enter is a
    // notice, and NOTHING goes on the wire (no shellout for a gone node).
    let mut v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    let i = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Card(_)))
        .expect("a card row");
    v.open_row_menu(i, Anchor::Center);
    v.layout.backlog.clear(); // the card races out
    let mut wire = Vec::new();
    row_menu_execute_selected(&mut v, &mut wire).await.unwrap();
    assert!(wire.is_empty(), "a stale card sends nothing");
    assert!(v.notice.is_some(), "and says so");
    assert!(!v.card_pending("x-a"), "no marker for a verb never sent");
}

#[test]
fn second_dispatch_is_suppressed_until_the_first_resolves() {
    // Concurrency: a double-press must not fire two shellouts (nor churn rank
    // with a no-op second `--top`).
    let mut v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    assert!(v.arm_backlog_pending("x-a", BacklogVerb::RankTop));
    assert!(
        !v.arm_backlog_pending("x-a", BacklogVerb::RankTop),
        "one verb in flight at a time"
    );
}

#[test]
fn pending_clears_only_when_the_target_card_itself_moves() {
    // AC3-UI + Concurrency: layouts push on every scrape tick, and claims
    // churn OTHER cards constantly. Only the target card's own movement is
    // confirmation - anything looser is a false confirm, which both lies to
    // the operator and releases the single-flight guard mid-verb.
    let cards = vec![
        bcard("x-a", CardState::Ready),
        bcard("x-b", CardState::Ready),
    ];
    let mut v = backlog_view(cards.clone(), 2);
    v.arm_backlog_pending("x-a", BacklogVerb::RankTop);
    let mut same = two_squad_layout(1);
    same.backlog = cards.clone();
    v.set_layout(same);
    assert!(v.card_pending("x-a"), "an unchanged feed confirms nothing");
    // Someone else's card gets claimed: the SET changed, this verb did not.
    let mut other_churned = two_squad_layout(1);
    let mut churn = cards.clone();
    churn[1].state = CardState::InFlight;
    other_churned.backlog = churn;
    v.set_layout(other_churned);
    assert!(
        v.card_pending("x-a"),
        "another card's churn is not this verb's confirmation"
    );
    let mut moved = two_squad_layout(1);
    moved.backlog = vec![cards[1].clone(), cards[0].clone()];
    v.set_layout(moved);
    assert!(
        !v.card_pending("x-a"),
        "the reorder landed -> marker clears"
    );
}

#[test]
fn a_defer_confirms_by_the_card_leaving_the_feed() {
    // A successful defer takes the node off the board, so absence is what
    // confirmation looks like for that verb.
    let cards = vec![
        bcard("x-a", CardState::Ready),
        bcard("x-b", CardState::Ready),
    ];
    let mut v = backlog_view(cards.clone(), 2);
    v.arm_backlog_pending("x-a", BacklogVerb::Defer);
    let mut gone = two_squad_layout(1);
    gone.backlog = vec![cards[1].clone()];
    v.set_layout(gone);
    assert!(
        !v.card_pending("x-a"),
        "gone from the board is confirmation"
    );
}

#[test]
fn a_verb_verdict_settles_the_marker_instead_of_spinning() {
    // AC3-UI: a FAILED verb reports via a notice. Without settling here the
    // card kept its `…`, every further verb stayed blocked behind the
    // single-flight guard, and the timeout later replaced the real reason
    // with a generic one.
    let mut v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    v.arm_backlog_pending("x-a", BacklogVerb::RankTop);
    v.settle_backlog_pending_on_notice();
    assert!(!v.card_pending("x-a"), "the verdict ends the wait");
    assert!(
        v.arm_backlog_pending("x-a", BacklogVerb::RankTop),
        "and the next verb is not blocked behind a stale marker"
    );
}

#[test]
fn unconfirmed_verb_expires_loudly() {
    // AC3-UI: a verb the feed never confirms clears with a notice - the row
    // must never keep a `…` nothing will resolve, and silence would read as
    // success.
    let mut v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    v.arm_backlog_pending("x-a", BacklogVerb::Defer);
    assert!(
        v.backlog_pending_deadline().is_some(),
        "the wait is bounded"
    );
    v.expire_backlog_pending();
    assert!(!v.card_pending("x-a"));
    assert!(v.notice.is_some(), "expiry is never silent");
}

#[test]
fn float_hint_only_on_ready_cards() {
    // Domain pitfall: floating a READY card to the top makes it the
    // dispatcher's next pick, so that entry says so; a blocked card carries
    // no such consequence and no such hint.
    let hint_of = |state| {
        let m = build_card_menu(
            &bcard("x-a", state),
            &crate::digest_overlay::ObsidianCfg::default(),
            Anchor::Center,
        );
        match &m.popup.rows[2] {
            PopupRow::Entry { hint, .. } => hint.clone(),
            other => panic!("expected the float entry, got {other:?}"),
        }
    };
    assert_eq!(hint_of(CardState::Ready), "may dispatch");
    assert_eq!(hint_of(CardState::Blocked), "");
}

#[test]
fn card_menu_open_plan_follows_ld7_grey_versus_absent() {
    let mut card = bcard("x-a", CardState::Ready);
    let off = crate::digest_overlay::ObsidianCfg::default();
    let on = crate::digest_overlay::ObsidianCfg {
        enabled: true,
        // Absolute, so resolution never depends on the test host's HOME.
        vault: Some("/tmp/vault".into()),
    };

    // Obsidian disabled: the item cannot apply no matter what the operator
    // does in this menu, so LD7 says absent, never greyed.
    card.plan_path = Some("/tmp/vault/plans/x-a.md".into());
    let m = build_card_menu(&card, &off, Anchor::Center);
    assert_eq!(
        m.popup.rows.len(),
        4,
        "no open-plan row when obsidian is off"
    );
    assert_eq!(
        m.actions.len(),
        2,
        "no OpenPlan action when obsidian is off"
    );

    // No plan_path: state can change (a plan can be added later), so LD7
    // says greyed with the reason, not absent.
    card.plan_path = None;
    let m = build_card_menu(&card, &on, Anchor::Center);
    match &m.popup.rows[4] {
        PopupRow::Entry {
            label,
            hint,
            enabled,
            ..
        } => {
            assert_eq!(label, "Open plan");
            assert_eq!(hint, "no plan");
            assert!(!enabled);
        }
        other => panic!("expected the open-plan entry, got {other:?}"),
    }
    assert_eq!(
        m.actions.len(),
        2,
        "a disabled entry contributes no action slot"
    );

    // Plan present and obsidian on: enabled, and the third action lines up
    // with the third selectable target.
    card.plan_path = Some("/tmp/vault/plans/x-a.md".into());
    let m = build_card_menu(&card, &on, Anchor::Center);
    match &m.popup.rows[4] {
        PopupRow::Entry { label, enabled, .. } => {
            assert_eq!(label, "Open plan");
            assert!(enabled);
        }
        other => panic!("expected the open-plan entry, got {other:?}"),
    }
    assert_eq!(m.actions.len(), 3);
    assert_eq!(m.actions[2], MenuAction::OpenPlan);
}

#[test]
fn stale_feed_keeps_its_cards_and_says_so() {
    // AC7-FR: a failing graph read must never blank the section - it keeps
    // the last-known cards under a header that admits they are memory.
    let mut layout = backlog_layout(vec![bcard("x-a", CardState::Ready)], 1);
    layout.backlog_stale = true;
    let mut v = two_pane_view();
    v.set_layout(layout);
    v.expand_pull_sections(); // (x-c5ee) ~ backlog now defaults Collapsed
    let rows = v.display_rows();
    assert!(
        rows.iter()
            .any(|r| matches!(r, DisplayRow::Header { label, .. } if label.contains("stale"))),
        "the header admits the section is stale"
    );
    assert!(
        rows.iter().any(|r| matches!(r, DisplayRow::Card(_))),
        "and the cards are still there"
    );
}

#[test]
fn kanban_lanes_carry_true_counts_and_flag_what_was_cut() {
    // AC5-EDGE: the header count is the lane's REAL size, so a lane whose
    // cards were cut by the feed cap must say so rather than let the header
    // silently disagree with the rows beneath it.
    let laned = |id: &str, lane: &str| {
        let mut c = bcard(id, CardState::Ready);
        c.lane = Some(lane.into());
        c
    };
    let cards = vec![laned("x-a", "ready"), laned("x-b", "triage")];
    let counts = vec![("ready".to_string(), 9), ("triage".to_string(), 1)];
    let k = build_kanban(&cards, &counts, Anchor::Center);
    let headers: Vec<&str> = k
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.contains(&"ready  9"), "lane states its true size");
    assert!(headers.contains(&"triage  1"));
    assert!(
        headers.iter().any(|h| h.contains("+8 more")),
        "a lane holding more than the feed carries says so"
    );
    // One action per rendered card, none for the headers.
    assert_eq!(k.actions.len(), 2);
}

#[test]
fn kanban_gives_unlaned_cards_a_home() {
    // A card with no `_kanban_column` must still appear on the board rather
    // than vanishing from it.
    let cards = vec![bcard("x-a", CardState::Ready)];
    let counts = vec![(crate::backlog_view::UNLANED.to_string(), 1)];
    let k = build_kanban(&cards, &counts, Anchor::Center);
    assert_eq!(
        k.actions,
        vec![AuxAction::BacklogGoto("x-a".into())],
        "the unlaned card is reachable"
    );
}

#[tokio::test]
async fn kanban_goto_moves_the_selector_to_that_card() {
    // AC6-FR: acting on a card in the overlay hands you back to its sideline
    // row - the same feed, so the two views can never show different orders.
    let mut v = backlog_view(
        vec![
            bcard("x-a", CardState::Ready),
            bcard("x-b", CardState::Ready),
        ],
        2,
    );
    v.open_kanban(Anchor::Center);
    assert!(v.aux.is_some(), "the overlay opens");
    let mut wire = Vec::new();
    execute_aux_action(&mut v, AuxAction::BacklogGoto("x-b".into()), &mut wire)
        .await
        .unwrap();
    assert!(v.aux.is_none(), "and closes on act");
    let landed = v.selector.expect("the selector moved");
    assert!(
        matches!(v.display_rows().get(landed), Some(DisplayRow::Card(c)) if c.id == "x-b"),
        "onto the card that was picked"
    );
}

#[tokio::test]
async fn kanban_goto_on_a_vanished_card_notices_instead_of_jumping() {
    // Concurrency: a card closed between opening the overlay and acting must
    // not move the cursor somewhere arbitrary.
    let mut v = backlog_view(vec![bcard("x-a", CardState::Ready)], 1);
    v.open_kanban(Anchor::Center);
    v.layout.backlog.clear();
    let mut wire = Vec::new();
    execute_aux_action(&mut v, AuxAction::BacklogGoto("x-a".into()), &mut wire)
        .await
        .unwrap();
    assert!(v.notice.is_some(), "says the card is gone");
    assert!(wire.is_empty(), "and sends nothing");
}

#[test]
fn selector_nav_skips_headers_and_clamps() {
    // AC2-UI + Boundaries: j/k stop on every actionable row, skip the two
    // section headers, and clamp (no wrap) at both ends.
    // Blank spacers sit at 2, 4, 6, 10 (the 4 = footer spacer); footer at 5,
    // headers at 7 and 11.
    let v = unified_rows_view();
    assert_eq!(
        v.selector_down(5),
        8,
        "j from the footer skips the spacer + '~ elsewhere'"
    );
    assert_eq!(v.selector_down(9), 12, "j skips the spacer + '~ backlog'");
    assert_eq!(v.selector_down(14), 14, "clamp at the last row");
    assert_eq!(v.selector_up(8), 5, "k skips '~ elsewhere' + spacer upward");
    assert_eq!(v.selector_up(12), 9, "k skips '~ backlog' + spacer upward");
    assert_eq!(v.selector_up(0), 0, "clamp at the top");
}

#[test]
fn selector_anchor_steps_off_headers() {
    // AC1-FR / AC2-EDGE: a re-anchored cursor never rests on a Header -
    // forward first, and an out-of-range index clamps to the last row.
    // Headers sit at 7 and 11 (Blank spacers at 2, 4, 6, 10).
    let v = unified_rows_view();
    assert_eq!(v.selector_anchor(7), Some(8), "header steps forward");
    assert_eq!(v.selector_anchor(11), Some(12), "header steps forward");
    assert_eq!(v.selector_anchor(50), Some(14), "stale index clamps");
    assert_eq!(v.selector_anchor(0), Some(0), "actionable row stays put");
}

// (x-cd67 US3, AC2-UI) Section spacing: with more than one squad, exactly one
// Blank separates each workspace group and precedes each trailing header -
// never doubled; with a single squad there are no spacers at all.
#[test]
fn blank_spacers_separate_groups_only_when_multi_squad() {
    let v = unified_rows_view(); // 2 squads, orphan section, backlog
    let rows = v.display_rows();
    let blanks = rows
        .iter()
        .filter(|r| matches!(r, DisplayRow::Blank))
        .count();
    assert_eq!(
        blanks, 4,
        "one between the two groups + one before the footer + one before each of the two headers"
    );
    // Never two spacers in a row.
    assert!(
        !rows
            .windows(2)
            .any(|w| matches!(w, [DisplayRow::Blank, DisplayRow::Blank])),
        "spacers are never doubled"
    );
    // A spacer precedes every trailing header.
    for (i, r) in rows.iter().enumerate() {
        if matches!(r, DisplayRow::Header { .. }) {
            assert!(
                matches!(rows[i - 1], DisplayRow::Blank),
                "a spacer precedes the header at {i}"
            );
        }
    }
    // A single squad has nothing to separate: no spacers.
    let single = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: vec![meta(1, "footnote", 1, 0)],
            active_squad: 1,
            panes: vec![],
            focus: 0,
            area: (28, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    assert!(
        !single
            .display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Blank)),
        "no spacers with a single squad"
    );
}

pub(super) fn agent_row_at(v: &View, pred: impl Fn(&AgentRow) -> bool) -> usize {
    v.display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if pred(a)))
        .expect("a matching agent row")
}

#[tokio::test]
async fn selector_tab_marks_attachable_and_notices_unmarkable() {
    // AC1-UI: Tab marks an attachable watch-only row (toggling), and gives
    // a notice on a pane-hosted (unmarkable) row without marking. (x-c376
    // moved the mark toggle from Space to Tab; Space now opens peek.)
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let idx = agent_row_at(&v, |a| a.attach_id.as_deref() == Some("c19cd2c3"));
    v.selector = Some(idx);
    selector_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert!(v.marks.contains("c19cd2c3"), "Tab marks the attachable row");
    v.selector = Some(idx);
    selector_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert!(!v.marks.contains("c19cd2c3"), "Tab toggles the mark off");
    // A pane-hosted row (no attach_id) is unmarkable -> notice, no mark.
    let hosted = agent_row_at(&v, |a| a.pane_id == Some(10));
    v.selector = Some(hosted);
    selector_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert!(v.marks.is_empty(), "an unmarkable row is not marked");
    assert!(v.notice.is_some(), "an unmarkable row gives a notice");
}

// x-c376 AC1-HP: Space on a selector agent row opens the peek overlay, sends
// a PeekAgent for that row's name, and leaves the selector open underneath.
#[tokio::test]
async fn peek_space_opens_overlay_and_sends_peekagent() {
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    v.selector = Some(idx);
    selector_keys(&mut v, b" ", &mut buf).await.unwrap();
    let peek = v.peek.as_ref().expect("Space opens peek");
    assert_eq!(peek.cursor, idx);
    assert!(peek.body.is_none(), "starts loading");
    assert_eq!(v.selector, Some(idx), "selector stays open underneath");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::PeekAgent { name, seq }) => {
            assert_eq!(name, "bg-claude");
            assert_eq!(seq, peek.seq);
        }
        other => panic!("expected PeekAgent, got {other:?}"),
    }
}

// x-c376: Space on a non-agent row (a section header) BELs, never opens peek.
#[tokio::test]
async fn peek_space_on_header_does_not_open() {
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let header = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { .. }))
        .expect("a header row exists");
    v.selector = Some(header);
    selector_keys(&mut v, b" ", &mut buf).await.unwrap();
    assert!(v.peek.is_none(), "Space on a header never opens peek");
}

// x-c376 AC2-HP: j moves the peek to the next agent row and refetches with a
// fresh, higher seq (stale bodies then drop by seq).
#[tokio::test]
async fn peek_j_moves_to_adjacent_agent_and_refetches() {
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let first = agent_row_at(&v, |a| a.name == "worker");
    v.selector = Some(first);
    selector_keys(&mut v, b" ", &mut buf).await.unwrap();
    let seq0 = v.peek.as_ref().unwrap().seq;
    buf.clear();
    peek_keys(&mut v, b"j", &mut buf).await.unwrap();
    let peek = v.peek.as_ref().expect("still open after j");
    assert!(peek.cursor > first, "moved down to the next agent row");
    assert!(peek.seq > seq0, "a fresh request seq");
    assert!(peek.body.is_none(), "the new row starts loading again");
    let mut cur = std::io::Cursor::new(buf);
    assert!(
        matches!(
            crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur),
            Ok(ClientMsg::Command(Command::PeekAgent { .. }))
        ),
        "j fires a fresh PeekAgent"
    );
}

// x-c376 AC2-UI: Esc closes peek back to the selector at the peeked row.
#[tokio::test]
async fn peek_esc_returns_to_selector_at_peeked_row() {
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let idx = agent_row_at(&v, |a| a.name == "bg-claude");
    v.selector = Some(idx);
    selector_keys(&mut v, b" ", &mut buf).await.unwrap();
    // A bare Esc resolves on the following byte (fold_selector_keys); "\x1bq"
    // yields one bare-Esc key (the q is swallowed by the pending-esc branch).
    peek_keys(&mut v, b"\x1bq", &mut buf).await.unwrap();
    assert!(v.peek.is_none(), "Esc closes peek");
    assert_eq!(v.selector, Some(idx), "selector cursor sits on the row");
}

// x-c376 AC1-FR: a PeekBody whose seq is not current is dropped; the matching
// seq applies.
#[test]
fn peek_body_seq_guard_drops_stale() {
    let mut v = unified_rows_view();
    v.peek = Some(PeekView {
        cursor: 0,
        seq: 5,
        body: None,
        name: String::new(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    });
    assert!(
        !v.apply_peek_body(4, vec!["stale".into()]),
        "an older seq is dropped"
    );
    assert!(v.peek.as_ref().unwrap().body.is_none());
    assert!(
        v.apply_peek_body(5, vec!["fresh".into()]),
        "the current seq applies"
    );
    assert_eq!(
        v.peek.as_ref().unwrap().body.as_deref(),
        Some(["fresh".to_string()].as_slice())
    );
}

// x-c376: peek_overlay_lines renders loading, then the transcript, and folds
// in the x-c929 answerable block for a blocked row.
#[test]
fn peek_overlay_renders_loading_transcript_and_answerable() {
    let row = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: None,
        name: "w".into(),
        pane_id: Some(3),
        portal: None,
        badge: Some(AgentBadge::Blocked),
        reason: Some("waiting on a menu".into()),
        exited: false,
        dnd: false,
        unmeasured: false,
        answerable: Some(answerable(&[("1", "Yes"), ("2", "No")], 7)),
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
    let loading = PeekView {
        cursor: 0,
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    let out = peek_overlay_lines(Some(&row), &loading, None, 0).join("\n");
    assert!(
        out.contains("waiting on a menu"),
        "shows the status sentence"
    );
    assert!(
        out.contains("1. Yes") && out.contains("2. No"),
        "answerable"
    );
    assert!(out.contains("loading"), "loading placeholder before a body");
    let loaded = PeekView {
        cursor: 0,
        seq: 1,
        body: Some(vec!["line one".into(), "line two".into()]),
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    let out = peek_overlay_lines(Some(&row), &loaded, None, 0).join("\n");
    assert!(out.contains("line one") && out.contains("line two"));
    assert!(!out.contains("loading"), "no placeholder once loaded");
    // A vanished row renders a safe placeholder, never a panic.
    assert!(peek_overlay_lines(None, &loaded, None, 0)[0].contains("row gone"));
}

// x-c376 (codex review): a layout shift that lands a DIFFERENT agent on the
// peeked index refetches (header + transcript never disagree); the same agent
// holds.
#[test]
fn peek_reanchor_refetches_on_identity_change_holds_on_same() {
    let mut v = unified_rows_view();
    let idx = agent_row_at(&v, |a| a.name == "worker");
    v.open_peek(idx, "worker".into());
    assert_eq!(v.peek_reanchor(), None, "same agent at the index holds");
    v.open_peek(idx, "was-someone-else".into());
    assert_eq!(
        v.peek_reanchor(),
        Some((idx, "worker".to_string())),
        "a changed row identity refetches"
    );
}

// x-c376 (codex review): raw transcript control chars (ESC/CR/TAB) are
// stripped before rendering so they never reach the operator's terminal.
#[test]
fn peek_overlay_sanitizes_control_chars_in_body() {
    let row = agent_row("w", 3, Some(AgentBadge::Working), false);
    let peek = PeekView {
        cursor: 0,
        seq: 1,
        body: Some(vec!["a\x1b[31mred\x1b[0m\tb\rc".into()]),
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    let out = peek_overlay_lines(Some(&row), &peek, None, 0).join("\n");
    assert!(!out.contains('\x1b'), "ESC stripped");
    assert!(!out.contains('\r'), "CR stripped");
    assert!(!out.contains('\t'), "TAB replaced");
    assert!(
        out.contains("red") && out.contains('c'),
        "printable text kept"
    );
}

// x-c914 piece 2 (AC2-UI): the account glyph rides the peek header for a
// row that bills a non-default account; a default-account row shows none.
#[test]
fn peek_header_carries_account_glyph() {
    let mut row = agent_row("w", 3, Some(AgentBadge::Working), false);
    row.account = Some("readyrule".into());
    let peek = PeekView {
        cursor: 0,
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    assert!(peek_overlay_lines(Some(&row), &peek, None, 0)[0].contains("@readyrule"));

    row.account = None; // default account -> no glyph
    assert!(!peek_overlay_lines(Some(&row), &peek, None, 0)[0].contains('@'));
}

#[test]
fn humanize_ago_thresholds() {
    assert_eq!(humanize_ago(30), "30s");
    assert_eq!(humanize_ago(90), "1m");
    assert_eq!(humanize_ago(3700), "1h");
    assert_eq!(humanize_ago(90_000), "1d");
}

// x-9c5f US7/US8: the peek header shows `changed Ns ago` + `PR #N` when the
// data exists, and NEITHER (no placeholder) when absent. AC2-EDGE.
#[test]
fn peek_header_shows_changed_ago_and_pr_when_present_else_absent() {
    let peek = PeekView {
        cursor: 0,
        seq: 1,
        body: None,
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    let mut row = agent_row("w", 3, Some(AgentBadge::Working), false);
    row.updated_at = Some(1_000);
    row.pr = Some(385);
    let header = &peek_overlay_lines(Some(&row), &peek, None, 1_090)[0];
    assert!(header.contains("changed 1m ago"), "header: {header}");
    assert!(header.contains("PR #385"), "header: {header}");

    row.updated_at = None;
    row.pr = None;
    let header = &peek_overlay_lines(Some(&row), &peek, None, 1_090)[0];
    assert!(!header.contains("changed"), "no changed line: {header}");
    assert!(!header.contains("PR #"), "no pr label: {header}");
}

// x-9c5f AC2-UI: the footer swaps by row state (exited -> `r respawn`, not
// `⏎ attach`; live -> the inverse) and shows `m reply` in both.
#[test]
fn peek_footer_swaps_on_exited_and_offers_m_reply() {
    let peek = PeekView {
        cursor: 0,
        seq: 1,
        body: Some(vec![]),
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    let mut row = agent_row("w", 3, Some(AgentBadge::Working), false);
    let live = peek_overlay_lines(Some(&row), &peek, None, 0).join("\n");
    assert!(live.contains("⏎ attach") && live.contains("m reply"));
    assert!(!live.contains("r respawn"));

    row.exited = true;
    let exited = peek_overlay_lines(Some(&row), &peek, None, 0).join("\n");
    assert!(exited.contains("r respawn") && exited.contains("m reply"));
    assert!(
        !exited.contains("⏎ attach"),
        "attach is a dead end on exited"
    );
}

// x-9c5f US5: while the reply input is open its line replaces the footer.
#[test]
fn peek_reply_input_line_replaces_footer() {
    let peek = PeekView {
        cursor: 0,
        seq: 1,
        body: Some(vec![]),
        name: "w".into(),
        last_fetch: Instant::now(),
        refresh_pending: false,
        squad: None,
    };
    let row = agent_row("w", 3, Some(AgentBadge::Working), false);
    let out = peek_overlay_lines(Some(&row), &peek, Some("fix the test"), 0).join("\n");
    assert!(out.contains("reply: fix the test"), "input line: {out}");
    assert!(!out.contains("⏎ attach"), "footer hidden while typing");
}

// x-c376 AC3-HP / AC2-ERR: a digit on a blocked, pane-hosted peeked row sends
// the exact x-c929 PaneAnswer payload and keeps the overlay open; a digit on a
// non-answerable row sends nothing (BEL).
#[tokio::test]
async fn peek_digit_answers_blocked_row_and_bels_non_answerable() {
    let mut v = view_with_agents(vec![
        blocked_row("peer", 4, Some(answerable(&[("1", "Yes"), ("2", "No")], 9))),
        blocked_row("plain", 5, None),
    ]);
    let mut buf: Vec<u8> = Vec::new();
    let blocked = agent_row_at(&v, |a| a.name == "peer");
    v.selector = Some(blocked);
    v.open_peek(blocked, "peer".into());
    peek_keys(&mut v, b"1", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::PaneAnswer {
            pane,
            fingerprint,
            region_lines,
            keystroke,
        } => {
            assert_eq!(pane, 4);
            assert_eq!(fingerprint, [9u8; 32]);
            assert_eq!(region_lines, 8);
            assert_eq!(keystroke, b"1");
        }
        other => panic!("expected PaneAnswer, got {other:?}"),
    }
    assert!(v.peek.is_some(), "overlay stays open after answering");
    // A non-answerable (focus-only) row: a digit sends nothing.
    let plain = agent_row_at(&v, |a| a.name == "plain");
    v.open_peek(plain, "plain".into());
    let mut buf2: Vec<u8> = Vec::new();
    peek_keys(&mut v, b"1", &mut buf2).await.unwrap();
    assert!(buf2.is_empty(), "no PaneAnswer for a non-answerable row");
}

// x-9c5f US5 / AC1-UI / AC3-UI: `m` opens the reply input; while open, input
// mode wins the key route (digits/j are literal buffer chars, not nav);
// Enter-with-text sends MailAgent and keeps peek open; empty-Enter keeps the
// input open (nothing sent).
#[tokio::test]
async fn peek_m_reply_sends_mail_and_input_mode_wins_over_nav() {
    let mut v = view_with_agents(vec![blocked_row("peer", 4, None)]);
    let idx = agent_row_at(&v, |a| a.name == "peer");
    v.open_peek(idx, "peer".into());
    // `m` opens the input (nothing sent yet).
    let mut buf: Vec<u8> = Vec::new();
    peek_keys(&mut v, b"m", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "m alone sends nothing");
    assert!(v.peek_input.is_some(), "input opened");
    // Input mode wins: `j` and a digit type into the buffer (no nav/answer).
    peek_keys(&mut v, b"j1", &mut Vec::new()).await.unwrap();
    assert_eq!(v.peek_input.as_ref().unwrap().1, "j1");
    // Enter with text sends MailAgent, closes the input, leaves peek open.
    let mut buf2: Vec<u8> = Vec::new();
    peek_keys(&mut v, b"\r", &mut buf2).await.unwrap();
    let mut cur = std::io::Cursor::new(buf2);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::MailAgent { name, text }) => {
            assert_eq!(name, "peer");
            assert_eq!(text, "j1");
        }
        other => panic!("expected MailAgent, got {other:?}"),
    }
    assert!(v.peek_input.is_none(), "input closed after send");
    assert!(
        v.peek.is_some(),
        "peek stays open (the notice is the feedback)"
    );
}

#[tokio::test]
async fn peek_empty_enter_keeps_reply_input_open() {
    let mut v = view_with_agents(vec![blocked_row("peer", 4, None)]);
    let idx = agent_row_at(&v, |a| a.name == "peer");
    v.open_peek(idx, "peer".into());
    peek_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    peek_keys(&mut v, b"\r", &mut buf).await.unwrap(); // empty Enter
    assert!(buf.is_empty(), "empty-Enter sends nothing (AC3-UI)");
    assert!(v.peek_input.is_some(), "input stays open on empty-Enter");
}

// x-9c5f US6 / AC1-EDGE: `r` on an exited row sends RespawnAgent; on a live
// row it BELs (nothing sent). The server re-validates uuid/external.
#[tokio::test]
async fn peek_r_respawns_exited_row_and_bels_live() {
    let mut dead = blocked_row("dead", 6, None);
    dead.exited = true;
    let mut v = view_with_agents(vec![dead, blocked_row("live", 5, None)]);
    let didx = agent_row_at(&v, |a| a.name == "dead");
    v.open_peek(didx, "dead".into());
    let mut buf: Vec<u8> = Vec::new();
    peek_keys(&mut v, b"r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::RespawnAgent { name }) => assert_eq!(name, "dead"),
        other => panic!("expected RespawnAgent, got {other:?}"),
    }
    // A live row: `r` sends nothing (BEL only).
    let lidx = agent_row_at(&v, |a| a.name == "live");
    v.open_peek(lidx, "live".into());
    let mut buf2: Vec<u8> = Vec::new();
    peek_keys(&mut v, b"r", &mut buf2).await.unwrap();
    assert!(buf2.is_empty(), "r on a live row sends nothing");
}

// x-c376 AC4-HP: Enter on a pane-hosted peeked row focuses its pane and
// closes BOTH overlays; (x-07c2) Enter on a NOT-yet-spawned watch-only row
// reaches the dedicated thread pane (one AttachAgent, thread_pane flag,
// no picker); a dead paneless row still refuses with a notice and keeps
// both overlays open.
#[tokio::test]
async fn peek_attaches_and_refuses_a_paneless_row() {
    // Pane-hosted "worker" (pane_id 10): Enter -> FocusPane, both close.
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let worker = agent_row_at(&v, |a| a.pane_id == Some(10));
    v.selector = Some(worker);
    v.open_peek(worker, "worker".into());
    peek_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(v.peek.is_none(), "attach closes peek");
    assert_eq!(v.selector, None, "attach closes the selector too");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::FocusPane(p)) => assert_eq!(p, 10),
        other => panic!("expected FocusPane, got {other:?}"),
    }
    // Watch-only "bg-claude" (attach_id, not yet spawned): Enter reaches
    // the dedicated thread pane directly - no picker on the plain reach
    // (the picker stays on its own explicit doors: selector `p`, menu).
    let mut v = unified_rows_view();
    let mut buf2: Vec<u8> = Vec::new();
    let bg = agent_row_at(&v, |a| a.name == "bg-claude");
    v.selector = Some(bg);
    v.open_peek(bg, "bg-claude".into());
    peek_keys(&mut v, b"\r", &mut buf2).await.unwrap();
    assert!(
        v.peek.is_none() && v.selector.is_none(),
        "the reach closes both"
    );
    assert!(v.attach_place.is_none(), "no placement dialog on a reach");
    let mut cur = std::io::Cursor::new(buf2);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert_eq!(
                placement.portal_target(),
                Some(0),
                "the reach drives portal 0"
            );
            assert!(placement.split.is_none() && !placement.here);
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
    // Orphan "bg-other" (no pane, no attach_id, DEAD): Enter refuses,
    // overlays stay.
    let mut v = unified_rows_view();
    v.layout
        .agents
        .iter_mut()
        .find(|a| a.name == "bg-other")
        .unwrap()
        .exited = true;
    let mut buf3: Vec<u8> = Vec::new();
    let orphan = agent_row_at(&v, |a| a.name == "bg-other");
    v.selector = Some(orphan);
    v.open_peek(orphan, "bg-other".into());
    peek_keys(&mut v, b"\r", &mut buf3).await.unwrap();
    assert!(v.peek.is_some(), "a refusal keeps peek open");
    assert_eq!(v.selector, Some(orphan), "and the selector open");
    assert!(v.notice.is_some(), "with a notice");
    assert!(buf3.is_empty(), "no command sent on a refusal");
}

#[tokio::test]
async fn selector_recruit_key_opens_recruit_and_falls_back_to_focused_row() {
    // R with marks opens the prompt; with no marks it marks the focused
    // attachable row first (the grid single-recruit generalized).
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let idx = agent_row_at(&v, |a| a.attach_id.as_deref() == Some("c19cd2c3"));
    v.selector = Some(idx);
    selector_keys(&mut v, b"R", &mut buf).await.unwrap();
    assert!(v.recruit.is_some(), "R opens the recruit prompt");
    assert!(
        v.marks.contains("c19cd2c3"),
        "zero-mark R marks the focused row"
    );
    assert_eq!(v.selector, None, "recruit is modal - the selector closes");
}

#[tokio::test]
async fn recruit_keys_enter_sends_marked_ids_and_clears_marks() {
    // AC2-HP (client half): a name + Enter sends one RecruitAgents with the
    // marked ids, and the marks clear.
    let mut v = unified_rows_view();
    v.marks.insert("c19cd2c3".into());
    v.open_recruit();
    let mut buf: Vec<u8> = Vec::new();
    recruit_keys(&mut v, b"team\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    match msg {
        ClientMsg::Command(Command::RecruitAgents { squad, ids }) => {
            assert_eq!(squad, "team");
            assert_eq!(ids, vec!["c19cd2c3".to_string()]);
        }
        other => panic!("expected RecruitAgents, got {other:?}"),
    }
    assert!(v.marks.is_empty(), "submit clears the marks");
    assert_eq!(v.recruit, None, "submit closes the overlay");
}

#[tokio::test]
async fn recruit_keys_esc_keeps_marks() {
    // AC2-UI: Esc cancels the prompt but keeps the marks for a re-open.
    let mut v = unified_rows_view();
    v.marks.insert("c19cd2c3".into());
    v.open_recruit();
    let mut buf: Vec<u8> = Vec::new();
    // A lone ESC is CSI-ambiguous until a following byte resolves it (the
    // fold's arrow-key safety); the trailing `x` surfaces the bare Esc and
    // then dies with the overlay.
    recruit_keys(&mut v, b"\x1bx", &mut buf).await.unwrap();
    assert_eq!(v.recruit, None, "esc closes the overlay");
    assert!(v.marks.contains("c19cd2c3"), "esc keeps the marks");
    assert!(buf.is_empty(), "esc sends nothing");
}

#[tokio::test]
async fn selector_x_on_a_tombstone_sends_dismiss() {
    // AC4-EDGE (client half): x on a tombstone member row sends
    // DismissMember for its squad + attach_id (not a squad remove).
    let tomb = AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "cc-deadbeef".into(),
        pane_id: None,
        portal: None,
        badge: None,
        reason: None,
        exited: true,
        dnd: false,
        unmeasured: false,
        answerable: None,
        attach_id: Some("deadbeef".into()),
        external: false,
        seen: false,
        cwd_base: None,
        tombstone: true,
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
    let mut v = view_with_agents(vec![tomb]);
    v.set_squad_view(1, SectionView::Expanded);
    let idx = agent_row_at(&v, |a| a.tombstone);
    v.selector = Some(idx);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::DismissMember {
            squad: 1,
            attach_id: "deadbeef".into()
        })
    );
}

// -- x-76ea agent-row lifecycle -------------------------------------

/// A plain (non-tombstone) registry agent row under squad 1, varied by state.
pub(super) fn lifecycle_row(name: &str, exited: bool, external: bool) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: name.into(),
        pane_id: None,
        badge: None,
        reason: None,
        exited,
        dnd: false,
        unmeasured: false,
        answerable: None,
        attach_id: None,
        external,
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
    }
}

#[tokio::test]
async fn selector_x_on_live_agent_arms_remove_confirm() {
    // x-f191 scope b: x on a live row arms ONE remove confirm (the server
    // composes stop-then-rm); nothing sends until the confirm commits.
    let mut v = view_with_agents(vec![lifecycle_row("worker-a", false, false)]);
    v.set_squad_view(1, SectionView::Expanded);
    v.selector = Some(agent_row_at(&v, |a| a.name == "worker-a"));
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "arming a confirm sends nothing");
    assert_eq!(v.selector, None, "the confirm closes the selector");
    match v.confirm.as_ref().map(|c| (&c.action, c.label.as_str())) {
        Some((ConfirmKind::RemoveAgent { name }, label)) => {
            assert_eq!(name, "worker-a");
            assert_eq!(label, "worker-a");
        }
        _ => panic!("expected a RemoveAgent confirm"),
    }
}

#[tokio::test]
async fn selector_x_on_exited_agent_arms_remove_confirm() {
    // US2 / AC2-HP (client half): x on an exited row arms a RemoveAgent
    // confirm - the row's own state (exited) selects the verb, no timer.
    let mut v = view_with_agents(vec![lifecycle_row("worker-b", true, false)]);
    v.set_squad_view(1, SectionView::Expanded);
    v.selector = Some(agent_row_at(&v, |a| a.name == "worker-b"));
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::RemoveAgent { name }) => assert_eq!(name, "worker-b"),
        _ => panic!("expected a RemoveAgent confirm"),
    }
}

#[tokio::test]
async fn selector_x_on_pane_hosted_agent_does_not_arm_lifecycle() {
    // codex review: a PANE-hosted Agent row (a real agent's pane or a bare
    // shell pane agent_rows() surfaces) must NOT arm stop/remove - its name
    // can be a cmd/cwd label with no registry entry, and it is managed via
    // its tab. `x` there falls through to a bell, arming nothing, sending
    // nothing.
    let mut pane_hosted = lifecycle_row("shell", false, false);
    pane_hosted.pane_id = Some(99);
    let mut v = view_with_agents(vec![pane_hosted]);
    v.set_squad_view(1, SectionView::Expanded);
    v.selector = Some(agent_row_at(&v, |a| a.name == "shell"));
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert!(
        buf.is_empty(),
        "a pane-hosted row sends no lifecycle command"
    );
    assert!(
        v.confirm.is_none(),
        "a pane-hosted row arms no lifecycle confirm"
    );
}

#[tokio::test]
async fn selector_x_on_agent_refuses_on_short_terminal() {
    // AC4-UI (x-260a): a too-short terminal refuses with a notice rather than
    // arm an invisible confirm - same rule the squad arm follows.
    let mut v = view_with_agents(vec![lifecycle_row("worker-c", false, false)]);
    v.set_squad_view(1, SectionView::Expanded);
    v.term.0 = MIN_ROWS_FOR_STATUS - 1;
    v.selector = Some(agent_row_at(&v, |a| a.name == "worker-c"));
    selector_keys(&mut v, b"x", &mut Vec::new()).await.unwrap();
    assert!(
        v.confirm.is_none(),
        "no invisible confirm on a short terminal"
    );
    assert!(v.notice.is_some(), "the refusal is surfaced");
}

#[tokio::test]
async fn selector_uppercase_x_on_agent_arms_reap_confirm() {
    // AC1-HP (client half): uppercase `X` on ANY agent row arms a ReapAgents
    // confirm (no payload) and sends nothing until it commits. Contextual on
    // an agent row - headers stay inert, no selector surgery.
    let mut v = view_with_agents(vec![lifecycle_row("worker-a", true, false)]);
    v.set_squad_view(1, SectionView::Expanded);
    v.selector = Some(agent_row_at(&v, |a| a.name == "worker-a"));
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"X", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "arming a confirm sends nothing");
    assert_eq!(v.selector, None, "the confirm closes the selector");
    assert!(
        matches!(
            v.confirm.as_ref().map(|c| &c.action),
            Some(ConfirmKind::ReapAgents)
        ),
        "expected a ReapAgents confirm"
    );
}

#[tokio::test]
async fn selector_uppercase_x_on_non_agent_row_arms_nothing() {
    // Contextual: `X` on a squad-header row (not an agent row) BELs and arms
    // no confirm - the bulk-reap gesture only fires from an agent row.
    let mut v = view_with_agents(vec![lifecycle_row("worker-a", true, false)]);
    let squad_row = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Sel(s) if s.tab.is_none()))
        .expect("a squad-header row");
    v.selector = Some(squad_row);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"X", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a non-agent row sends nothing");
    assert!(v.confirm.is_none(), "a non-agent row arms no reap confirm");
}

#[tokio::test]
async fn selector_uppercase_x_refuses_on_short_terminal() {
    // AC1-UI (x-260a): a too-short terminal refuses with a notice rather than
    // arm an invisible confirm, matching the per-row `x` arm.
    let mut v = view_with_agents(vec![lifecycle_row("worker-a", true, false)]);
    v.set_squad_view(1, SectionView::Expanded);
    v.term.0 = MIN_ROWS_FOR_STATUS - 1;
    v.selector = Some(agent_row_at(&v, |a| a.name == "worker-a"));
    selector_keys(&mut v, b"X", &mut Vec::new()).await.unwrap();
    assert!(
        v.confirm.is_none(),
        "no invisible confirm on a short terminal"
    );
    assert!(v.notice.is_some(), "the refusal is surfaced");
}

#[tokio::test]
async fn confirm_keys_enter_sends_reap_agents() {
    // AC1-HP (client half): Enter on an armed ReapAgents confirm sends the
    // payload-free ReapAgents command.
    let mut v = view_with_agents(vec![]);
    v.confirm = Some(ConfirmAction {
        action: ConfirmKind::ReapAgents,
        label: String::new(),
    });
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(decoded, ClientMsg::Command(Command::ReapAgents));
}

#[tokio::test]
async fn selector_x_on_live_external_arms_stop_external() {
    // AC2-HP (client half): x on a live external row routes to StopExternal
    // by its stable attach_id, NOT fno-agents-by-name.
    let mut row = lifecycle_row("ext-a", false, true);
    row.attach_id = Some("deadbeef".into());
    let mut v = view_with_agents(vec![row]);
    v.set_squad_view(1, SectionView::Expanded);
    v.selector = Some(agent_row_at(&v, |a| a.name == "ext-a"));
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "arming a confirm sends nothing");
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::StopExternal { attach_id, name }) => {
            assert_eq!(attach_id, "deadbeef");
            assert_eq!(name, "ext-a");
        }
        _ => panic!("expected a StopExternal confirm"),
    }
}

#[tokio::test]
async fn selector_x_on_stopped_external_arms_remove_external() {
    // AC3-HP (client half): x on an exited external tombstone routes to
    // RemoveExternal by attach_id (the stopped tombstone `exited` maps to rm).
    let mut row = lifecycle_row("ext-b", true, true);
    row.attach_id = Some("cafef00d".into());
    let mut v = view_with_agents(vec![row]);
    v.set_squad_view(1, SectionView::Expanded);
    v.selector = Some(agent_row_at(&v, |a| a.name == "ext-b"));
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::RemoveExternal { attach_id, name }) => {
            assert_eq!(attach_id, "cafef00d");
            assert_eq!(name, "ext-b");
        }
        _ => panic!("expected a RemoveExternal confirm"),
    }
}

#[tokio::test]
async fn confirm_keys_enter_sends_external_commands() {
    for (kind, want) in [
        (
            ConfirmKind::StopExternal {
                attach_id: "deadbeef".into(),
                name: "e".into(),
            },
            Command::StopExternal {
                attach_id: "deadbeef".into(),
                name: "e".into(),
            },
        ),
        (
            ConfirmKind::RemoveExternal {
                attach_id: "cafef00d".into(),
                name: "e".into(),
            },
            Command::RemoveExternal {
                attach_id: "cafef00d".into(),
                name: "e".into(),
            },
        ),
    ] {
        let mut v = view_with_agents(vec![]);
        v.confirm = Some(ConfirmAction {
            action: kind,
            label: "e".into(),
        });
        let mut buf: Vec<u8> = Vec::new();
        confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(decoded, ClientMsg::Command(want));
    }
}

#[tokio::test]
async fn confirm_keys_enter_sends_stop_then_remove_agent() {
    // US1/US2 (client half): Enter on an armed StopAgent/RemoveAgent confirm
    // sends the captured-name command (the row index is never re-read).
    for (kind, want) in [
        (
            ConfirmKind::StopAgent { name: "w".into() },
            Command::StopAgent { name: "w".into() },
        ),
        (
            ConfirmKind::RemoveAgent { name: "w".into() },
            Command::RemoveAgent { name: "w".into() },
        ),
    ] {
        let mut v = view_with_agents(vec![]);
        v.confirm = Some(ConfirmAction {
            action: kind,
            label: "w".into(),
        });
        let mut buf: Vec<u8> = Vec::new();
        confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(decoded, ClientMsg::Command(want));
    }
}

#[test]
fn display_rows_footer_keeps_empty_session_actionable() {
    // AC3-EDGE: zero squads/agents/cards still yields the footer, so
    // prefix+w always has a row to open on and Enter opens the create
    // overlay.
    let v = View::new(
        (30, 100),
        "main".into(),
        LayoutView {
            squads: vec![],
            active_squad: 0,
            panes: vec![],
            focus: 0,
            area: (28, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    assert_eq!(v.display_rows().len(), 1, "footer only");
    assert!(matches!(v.row_action(0), Some(ChromeHit::OpenCreate)));
}

#[tokio::test]
async fn selector_enter_focuses_hosted_agent_pane() {
    // AC1-HP: Enter on a pane-hosted agent row sends FocusPane and closes.
    let mut v = unified_rows_view();
    v.selector = Some(1);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync(&mut cur).unwrap() {
        ClientMsg::Command(Command::FocusPane(10)) => {}
        other => panic!("expected FocusPane(10), got {other:?}"),
    }
    assert_eq!(v.selector, None, "acting closes the selector");
}

#[tokio::test]
async fn selector_enter_reaches_bg_agent_thread_pane() {
    // (x-07c2) Enter on a not-yet-spawned claude bg row reaches the ONE
    // dedicated thread pane: one AttachAgent with the thread_pane flag,
    // no placement dialog, selector closed. The explicit placement
    // gestures (selector `p`, menu splits, open-here, drag) still pin a
    // persisted pane for an operator who wants one.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(v.selector, None, "the reach closes the selector");
    assert!(v.attach_place.is_none(), "no placement dialog on a reach");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert_eq!(placement.portal_target(), Some(0), "drives portal 0");
            assert!(placement.split.is_none() && !placement.here && placement.at.is_none());
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn selector_p_opens_attach_placement_without_sending() {
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    let picker = v.attach_place.as_ref().expect("placement picker opens");
    assert_eq!(picker.id, "c19cd2c3");
    assert_eq!(picker.target(), Some(1));
    assert_eq!(picker.squads, vec![1, 2]);
    // The footer must let each axis name its OWN keys, and must say which
    // key acts on the `›` marker. The old footer listed the split
    // directions as if they were list navigation, which was the mislabel
    // half of the reported defect. This footer collapses it to two lines
    // and spells the split row `shift+HJKL` so the shift relationship to
    // lowercase hjkl reads in words, not just in case.
    let overlay = v.attach_place_lines(picker).join("\n");
    for label in [
        "hjkl/arrows move",
        "1-9 jump",
        // enter/t send byte-identical new-tab messages; space/. send
        // byte-identical here messages. Each pair is one footer entry.
        "enter/t new tab in ›",
        "shift+HJKL split",
        "space/. here",
        "cancel",
    ] {
        assert!(overlay.contains(label), "missing {label}: {overlay}");
    }
    assert!(buf.is_empty());
    assert_eq!(v.selector, None);
}

#[tokio::test]
async fn attach_placement_selects_target_and_direction() {
    // The digit jumps the cursor; UPPERCASE commits with a split direction.
    // Lowercase `h` here would now only move the cursor (see
    // attach_placement_arrows_move_the_cursor_without_attaching).
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"2H", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                portal_new: false,
                portal: None,
                tab: None,
                at: None,
                target: PaneTarget::SquadId(2),
                split: Some(Dir::Left),
                here: false,
                fallback: PlacementFallback::NewTab,
                max_panes: None,
                thread_pane: false,
            },
        })
    );
    assert!(v.attach_place.is_none());
}

/// Replace a view's layout with `n` real workspaces (ids 1..=n), so a picker
/// opened over it has to cope with more destinations than there are digits.
/// 14 is the operator's real number, and the number at which five used to
/// vanish.
fn widen_to_squads(v: &mut View, n: u64) {
    v.layout.squads = (1..=n).map(|i| meta(i, &format!("ws{i}"), 1, 0)).collect();
}

/// The `display_rows()` index of the attachable bg-claude agent row. Read
/// rather than hardcoded, because widening the workspace list shifts every
/// row index below it.
fn attachable_agent_row(v: &View) -> usize {
    v.display_rows()
        .iter()
        .position(|r| {
            matches!(r, DisplayRow::Agent(a)
                if a.pane_id.is_none() && !a.exited && a.attach_id.is_some())
        })
        .expect("an attachable agent row")
}

/// Open the attach picker the way the `p` key does (no sideline row
/// index needed), which since x-07c2 is the picker's only door.
async fn open_attach_by_click(v: &mut View) {
    let squads = v.attach_dst_squads();
    v.open_attach_place("c19cd2c3".into(), None, squads);
}

#[tokio::test]
async fn attach_picker_reaches_past_the_ninth_workspace() {
    // AC3-HP: with 14 workspaces, the 10th and the 14th are REACHABLE. Under
    // `.take(9)` they were not in the list at all, so no key could get to
    // them and nothing told the operator they existed.
    let mut v = unified_rows_view();
    widen_to_squads(&mut v, 14);
    open_attach_by_click(&mut v).await;
    let mut buf = Vec::new();
    assert_eq!(
        v.attach_place.as_ref().unwrap().squads.len(),
        14,
        "every workspace is a candidate, not the first nine"
    );

    // Walk to the 10th (index 9): past the digit range by construction.
    attach_place_keys(&mut v, b"jjjjjjjjj", &mut buf)
        .await
        .unwrap();
    assert!(buf.is_empty(), "walking the list attaches nothing");
    assert_eq!(v.attach_place.as_ref().unwrap().target(), Some(10));

    // ...and on to the 14th, where the clamp stops it.
    attach_place_keys(&mut v, b"jjjjjjjj", &mut buf)
        .await
        .unwrap();
    let picker = v.attach_place.as_ref().unwrap();
    assert_eq!(picker.target(), Some(14), "the last workspace is reachable");
    assert_eq!(picker.cursor, 13, "clamped at the end, no wrap");

    // Committing there really targets the 14th, not a nine-capped stand-in.
    attach_place_keys(&mut v, b"L", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert_eq!(placement.target, PaneTarget::SquadId(14));
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn attach_picker_rows_past_nine_carry_no_digit() {
    // The drawn numbering must never lie about what a digit will do: nine
    // digits exist, so exactly nine rows are numbered and the rest are not.
    let mut v = unified_rows_view();
    widen_to_squads(&mut v, 14);
    open_attach_by_click(&mut v).await;
    let picker = v.attach_place.as_ref().unwrap();
    let lines = v.attach_place_lines(picker);
    assert!(lines[9].contains("9 ws9"), "the ninth row is numbered");
    assert!(
        !lines[10].contains("10 ws10"),
        "the tenth carries no ordinal: {:?}",
        lines[10]
    );
    assert!(lines[10].contains("ws10"), "but it IS listed");
}

#[tokio::test]
async fn both_attach_picker_entry_paths_build_the_same_candidate_list() {
    // The click path (apply_hit) and the keyboard path (p/Enter) used to
    // build this list independently, which is how they drifted before. One
    // helper now feeds both; this pins that they cannot drift again by
    // asserting the two paths agree on the SAME layout.
    // Two views built identically: unified_rows_view() is deterministic, so
    // the only difference between them is which door gets opened.
    let widen = |v: &mut View| {
        widen_to_squads(v, 14);
        v.layout.squads.push(mission_meta(5, "epic  0/4"));
    };
    let mut click = unified_rows_view();
    widen(&mut click);
    let mut keyboard = unified_rows_view();
    widen(&mut keyboard);

    open_attach_by_click(&mut click).await;

    keyboard.selector = Some(attachable_agent_row(&keyboard));
    selector_keys(&mut keyboard, b"p", &mut Vec::new())
        .await
        .unwrap();

    let a = click.attach_place.as_ref().expect("click opens the picker");
    let b = keyboard
        .attach_place
        .as_ref()
        .expect("keyboard opens the picker");
    assert_eq!(a.squads, b.squads, "one list, two doors");
    assert_eq!(a.squads.len(), 14, "and it is the uncapped one");
    assert!(
        !a.squads.iter().any(|&id| is_mission_squad(id)),
        "with the mission sentinel excluded on both"
    );
}

#[tokio::test]
async fn move_picker_reaches_past_the_ninth_workspace() {
    // The same ceiling in the OTHER overlay. Pinning it only where the
    // operator happened to hit it would leave the class half-fixed - which
    // is the whole reason wave 3b exists.
    let mut v = two_pane_view();
    widen_to_squads(&mut v, 14);
    v.selector = Some(0); // squad 1
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"m", &mut buf).await.unwrap();
    let picker = v.move_pick.as_ref().expect("picker opens");
    assert_eq!(
        picker.squads.len(),
        13,
        "every workspace but the source is a destination"
    );

    // Destinations are 2..=14; index 12 is the 14th workspace.
    move_pick_keys(&mut v, b"jjjjjjjjjjjj", &mut buf)
        .await
        .unwrap();
    assert!(buf.is_empty(), "walking the list moves nothing");
    let picker = v.move_pick.as_ref().expect("the picker stays open");
    assert_eq!(picker.target(), Some(14), "the last workspace is reachable");

    // Enter commits the cursor, so the destination past nine is not just
    // visible but selectable.
    move_pick_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::MoveTab { squad, .. }) => assert_eq!(squad, 14),
        other => panic!("expected MoveTab, got {other:?}"),
    }
    assert!(v.move_pick.is_none(), "committing closes the picker");
}

#[tokio::test]
async fn move_picker_ignores_an_unmapped_key_instead_of_closing() {
    // It used to close on ANY key that was not a digit. That was safe while
    // it was single-shot and digit-only; with a cursor it would mean the
    // picker vanished under the operator mid-scan.
    let mut v = two_pane_view();
    widen_to_squads(&mut v, 14);
    v.selector = Some(0);
    selector_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    move_pick_keys(&mut v, b"z", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert!(
        v.move_pick.is_some(),
        "an unmapped key is ignored, not fatal"
    );
    // `q` still closes instantly. A bare Esc lands on the FOLLOWING keypress
    // instead, which is fold_selector_keys' documented behaviour and the
    // price of supporting arrows at all: a lone ESC is indistinguishable
    // from the head of `ESC [ A` until the next byte arrives. Every other
    // folded overlay behaves the same way.
    move_pick_keys(&mut v, b"q", &mut buf).await.unwrap();
    assert!(v.move_pick.is_none(), "q cancels");
}

#[tokio::test]
async fn move_picker_out_of_range_digit_bels_and_keeps_the_picker() {
    // The same answer the attach picker gives: a digit past the END of the
    // list names a row that was never drawn, so it BELs and the picker
    // stays open. Closing on it would be the "my picker vanished" surprise
    // the cursor was added to remove, and it fired on the one keypress an
    // operator makes while learning how wide the list is.
    let mut v = two_pane_view();
    v.selector = Some(0); // squad 1; two_pane_view has squad 2 as the only dst
    selector_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
    assert_eq!(v.move_pick.as_ref().unwrap().squads.len(), 1);
    let mut buf: Vec<u8> = Vec::new();
    move_pick_keys(&mut v, b"5", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "an out-of-range digit sends nothing");
    assert!(
        v.move_pick.is_some(),
        "and leaves the picker open to try again"
    );
    assert_eq!(v.move_pick.as_ref().unwrap().cursor, 0, "cursor unmoved");
}

#[tokio::test]
async fn attach_placement_keys_do_not_depend_on_cursor_history() {
    // The hard constraint behind superseding x-fbb1: a key must mean the
    // same thing whether or not the cursor has moved. The tempting cheap
    // fix was "Enter means here on the starting row, and commits the cursor
    // once moved", which is a hidden mode - the exact class this node
    // closes. Enter on an UNMOVED cursor must still commit that cursor,
    // never silently fall back to the route.
    let mut v = unified_rows_view();
    widen_to_squads(&mut v, 14);
    open_attach_by_click(&mut v).await;
    let start = v.attach_place.as_ref().unwrap().target().unwrap();
    let mut buf = Vec::new();
    attach_place_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert_eq!(
                placement.target,
                PaneTarget::SquadId(start),
                "Enter commits the cursor even when it has never moved"
            );
            assert!(!placement.here, "and is not secretly the route");
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }

    // And the cursor's ROUTE to a row does not change what commits: digit
    // and arrows landing on the same index must produce the same command.
    let by_digit = {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf = Vec::new();
        attach_place_keys(&mut v, b"4\r", &mut buf).await.unwrap();
        buf
    };
    let by_arrows = {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf = Vec::new();
        attach_place_keys(&mut v, b"jjj\r", &mut buf).await.unwrap();
        buf
    };
    assert_eq!(
        by_digit, by_arrows,
        "how the cursor got there cannot matter"
    );
}

#[tokio::test]
async fn attach_placement_out_of_range_digit_bels_and_moves_nothing() {
    // A digit past the end of the list is a BEL, not a selection, and the
    // notice says the row is not there rather than claiming the workspace is
    // "no longer available" - it never was. The cursor stays put and the
    // picker stays open, so the operator can just press the right key next.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"9", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "an out-of-range digit sends nothing");
    let picker = v.attach_place.as_ref().expect("picker stays open");
    assert_eq!(picker.cursor, 0, "and moves the cursor nowhere");
}

#[tokio::test]
async fn attach_placement_out_of_range_digit_drops_the_rest_of_the_batch() {
    // The BEL alone is not enough. Terminal reads arrive in batches, so the
    // keys typed AFTER a bad digit are already in the same buffer - and they
    // were composed believing row 9 existed. `L` would commit a right split
    // into whatever the cursor happened to be on, a placement the operator
    // never chose, with only a beep between intent and commit. So the bad
    // digit abandons the whole read: nothing is sent, the cursor is untouched
    // and the picker stays open on screen the operator can now actually read.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"9L", &mut buf).await.unwrap();
    assert!(
        buf.is_empty(),
        "the trailing commit key must not reach the socket"
    );
    let picker = v.attach_place.as_ref().expect("picker stays open");
    assert_eq!(picker.cursor, 0, "and the cursor never moved");
    assert!(
        v.notice.is_some(),
        "the operator is told why nothing happened"
    );
}

#[tokio::test]
async fn attach_placement_arrows_move_the_cursor_without_attaching() {
    // AC3-FR, the exact reported defect: `j` (and therefore Down, which
    // fold_selector_keys rewrites to `j`) used to return
    // Some(Some(Dir::Down)) and attach IMMEDIATELY. Scanning the list with
    // the arrow keys finalized a placement the operator never chose. This is
    // the test that had to fail before the fix.
    for key in [b"j".as_slice(), b"\x1b[B".as_slice()] {
        let mut v = unified_rows_view();
        v.selector = Some(8); // bg-claude
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        assert_eq!(v.attach_place.as_ref().unwrap().cursor, 0);
        attach_place_keys(&mut v, key, &mut buf).await.unwrap();
        assert!(buf.is_empty(), "key {key:?} must send no AttachAgent");
        let picker = v.attach_place.as_ref().expect("picker stays open");
        assert_eq!(picker.cursor, 1, "key {key:?} moves the cursor");
        assert_eq!(picker.target(), Some(2));
    }
    // ...and back up, clamped at the top rather than wrapping.
    let mut v = unified_rows_view();
    v.selector = Some(8);
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"kk", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert_eq!(
        v.attach_place.as_ref().unwrap().cursor,
        0,
        "clamped, no wrap"
    );
}

#[tokio::test]
async fn attach_placement_new_tab_and_cancel_are_distinct() {
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    // `t` is Enter's named alias: both open a new tab in the
    // cursor-marked workspace. Space and `.` are the separate "here" pair.
    attach_place_keys(&mut v, b"t", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                portal_new: false,
                portal: None,
                tab: None,
                at: None,
                target: PaneTarget::SquadId(1),
                split: None,
                here: false,
                fallback: PlacementFallback::NewTab,
                max_panes: None,
                thread_pane: false,
            },
        })
    );

    v.selector = Some(8); // bg-claude
    let mut cancelled = Vec::new();
    selector_keys(&mut v, b"p", &mut cancelled).await.unwrap();
    attach_place_keys(&mut v, b"q", &mut cancelled)
        .await
        .unwrap();
    assert!(cancelled.is_empty());
    assert!(v.attach_place.is_none());
}

#[tokio::test]
async fn attach_placement_enter_commits_the_cursor_and_space_attaches_here() {
    // SUPERSEDES x-fbb1's Enter-is-here ruling, which was correct while the
    // picker had no cursor to contradict it. Adding a cursor removed its
    // premise: the overlay drew a marker on one workspace and Enter
    // attached to another, which is this node's own defect one layer up.
    //
    // This re-splits Enter and Space, which had been merged into one
    // "new tab in ›" commit: Enter (and its alias `t`) always commits the
    // cursor; Space (and its alias `.`) always attaches HERE. No key's
    // meaning depends on cursor history either way.
    let mut v = unified_rows_view();
    widen_to_squads(&mut v, 14);
    open_attach_by_click(&mut v).await;
    let mut buf = Vec::new();
    // Drive the cursor somewhere the digits cannot reach, which is the
    // case the cursor exists for and the case the old Enter broke.
    attach_place_keys(&mut v, b"jjjjjjjjj", &mut buf)
        .await
        .unwrap();
    assert_eq!(v.attach_place.as_ref().unwrap().target(), Some(10));
    attach_place_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                portal_new: false,
                portal: None,
                tab: None,
                at: None,
                target: PaneTarget::SquadId(10),
                split: None,
                here: false,
                fallback: PlacementFallback::NewTab,
                max_panes: None,
                thread_pane: false,
            },
        }),
        "Enter must attach to the marked workspace, not here"
    );
    assert!(v.attach_place.is_none());

    // Space and `.` both ignore the cursor BY DESIGN rather than by
    // accident - including after the cursor has moved, so their meaning
    // is history-independent too.
    for key in [b" ".as_slice(), b".".as_slice()] {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf = Vec::new();
        attach_place_keys(&mut v, b"jjj", &mut buf).await.unwrap();
        attach_place_keys(&mut v, key, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
                assert_eq!(placement.target, PaneTarget::CurrentRoute);
                assert!(placement.here, "key {key:?} is route-anchored");
            }
            other => panic!("expected AttachAgent, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn attach_placement_refuses_stale_target_without_sending() {
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    // Park the cursor on squad 2, then delete it out from under the open
    // picker: the cursor guarantees an in-range index, never a live squad.
    v.attach_place.as_mut().unwrap().cursor = 1;
    v.layout.squads.retain(|s| s.id != 2);
    attach_place_keys(&mut v, b"L", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert!(v.notice.is_some());
    assert!(v.attach_place.is_none());
}

#[tokio::test]
async fn selector_enter_refusal_keeps_selector_open() {
    // AC1-ERR + AC2-ERR (locked 3): a refusal row (a DEAD paneless agent,
    // blocked card, in-flight card) shows a notice, sends nothing, and the
    // selector stays open. A live paneless row is no longer a refusal -
    // it reaches the dedicated thread pane (x-07c2) - so the dead-row
    // case carries this invariant now.
    let mut v = unified_rows_view();
    v.layout
        .agents
        .iter_mut()
        .find(|a| a.name == "bg-other")
        .unwrap()
        .exited = true; // the dead paneless row
                        // bg-other (9), blocked card (13), in-flight card (14).
    for row in [9usize, 13, 14] {
        v.selector = Some(row);
        v.notice = None;
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "refusal sends nothing (row {row})");
        assert!(v.notice.is_some(), "refusal explains itself (row {row})");
        assert_eq!(v.selector, Some(row), "selector stays open (row {row})");
    }
}

#[tokio::test]
async fn selector_enter_ready_card_opens_confirm() {
    // AC2-HP: Enter on a Ready card closes the selector and arms the
    // one-keypress dispatch confirm - nothing on the wire yet; the second
    // Enter (confirm_keys) sends the DispatchNode (AC2-FR: the confirm
    // takes the action, so one dispatch at most).
    let mut v = unified_rows_view();
    v.selector = Some(12); // ready card
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "confirm first, dispatch on the next Enter");
    assert_eq!(v.selector, None);
    assert!(
        matches!(&v.confirm.as_ref().unwrap().action, ConfirmKind::Dispatch { node } if node == "x-rdy"),
        "the Ready card's node is armed for dispatch"
    );
}

#[tokio::test]
async fn selector_enter_footer_opens_create_overlay() {
    // AC3-HP: Enter on "+ new workspace" opens the name-input overlay.
    let mut v = unified_rows_view();
    v.selector = Some(5); // footer
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert_eq!(v.selector, None, "open_create clears the selector");
    assert_eq!(v.create.as_deref(), Some(""));
}

#[test]
fn open_confirm_is_modal_over_keyboard_overlays() {
    // sigma review x-260a: a mouse click arming the confirm while the
    // keyboard selector (or answer overlay) is open must clear it - the
    // confirm wins stdin routing, so a lingering selector would swallow
    // the keystrokes after the confirm resolves. Same discipline as
    // open_create.
    let mut view = unified_rows_view();
    view.selector = Some(8);
    view.answers = Some(0);
    view.create = Some("half-typed".into());
    view.nav = Some(NavView {
        query: "half".into(),
        state_filter: None,
        cursor: 0,
    });
    view.open_confirm(ConfirmAction {
        action: ConfirmKind::Dispatch {
            node: "x-rdy".into(),
        },
        label: "x-rdy".into(),
    });
    assert!(view.selector.is_none(), "confirm clears an open selector");
    assert!(view.answers.is_none(), "confirm clears the answer overlay");
    assert!(view.create.is_none(), "confirm drops a half-typed create");
    assert!(view.search.is_none());
    assert!(
        view.nav.is_none(),
        "confirm clears an open navigator (x-653d)"
    );
    assert!(
        matches!(&view.confirm.as_ref().unwrap().action, ConfirmKind::Dispatch { node } if node == "x-rdy"),
        "the armed confirm carries the node"
    );
}

#[test]
fn short_terminal_degrades_prompts_to_notices() {
    // sigma review x-260a: below MIN_ROWS_FOR_STATUS the bottom-row
    // prompt cannot render, so a Ready card and the footer refuse with a
    // notice instead of arming an invisible modal (which could dispatch
    // blind on the next Enter).
    // ready card at 12, footer at 5.
    let mut v = unified_rows_view();
    v.term.0 = MIN_ROWS_FOR_STATUS - 1;
    assert!(
        matches!(v.row_action(12), Some(ChromeHit::Notice(_))),
        "ready card refuses on a too-short terminal"
    );
    assert!(
        matches!(v.row_action(5), Some(ChromeHit::Notice(_))),
        "footer refuses on a too-short terminal"
    );
    // At the minimum height both act normally again.
    v.term.0 = MIN_ROWS_FOR_STATUS;
    assert!(matches!(v.row_action(12), Some(ChromeHit::Confirm(_))));
    assert!(matches!(v.row_action(5), Some(ChromeHit::OpenCreate)));
}

#[tokio::test]
async fn selector_keys_navigate_unified_rows() {
    // AC1-UI / AC2-UI: j/k through selector_keys land on agent and card
    // rows, skipping headers, without sending anything.
    // footer at 5; j lands on bg-claude (8) past the spacer + header.
    let mut v = unified_rows_view();
    v.selector = Some(5);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"j", &mut buf).await.unwrap();
    assert_eq!(
        v.selector,
        Some(8),
        "j skips the spacer + '~ elsewhere' header"
    );
    selector_keys(&mut v, b"k", &mut buf).await.unwrap();
    assert_eq!(v.selector, Some(5), "k skips it back");
    assert!(buf.is_empty(), "navigation sends nothing");
}

#[tokio::test]
async fn selector_d_on_live_pane_row_sends_row_detach_shape() {
    let row = pane_hosted_row("worker", 10);
    let mut v = view_with_agents(vec![row]);
    let idx = agent_row_at(&v, |a| a.name == "worker");
    v.selector = Some(idx);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"d", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::DetachPane { pane: 10 }],
        "sideline d sends the row-scoped detach command"
    );
}

// ---- x-653d: session navigator (prefix+f) ----

#[test]
fn nav_rows_lists_every_squads_tabs_ignoring_expand() {
    // AC1-HP + Locked 3: the flat catalog lists a COLLAPSED squad's tabs too
    // (the sideline gates tabs behind expand; the navigator never does).
    let v = two_pane_view(); // footnote(active, tabs 1&2), notes(collapsed, tab 1)
    assert!(
        v.squad_view(2) == SectionView::Collapsed,
        "notes is collapsed"
    );
    let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
    for want in [
        "footnote",
        "footnote › 1",
        "footnote › 2",
        "notes",
        "notes › 1",
    ] {
        assert!(
            labels.iter().any(|l| l == want),
            "missing {want:?} in {labels:?}"
        );
    }
}

#[test]
fn nav_rows_agent_label_carries_tab_ordinal() {
    // x-0090 US4: a pane-hosted agent's nav label names its tab with a `·N`
    // ordinal (fixture tab id 1 is the 2nd tab -> ·2), coherent with the
    // sideline; a watch-only row (no tab) carries none.
    let mut v = two_pane_view();
    v.layout.agents = vec![
        AgentRow {
            harness: None,
            model: None,
            route: None,
            reach: Reach::Locate,
            spawned_by_session: None,
            harness_session_id: None,
            squad: Some(1),
            name: "build".into(),
            pane_id: Some(10),
            portal: None,
            badge: Some(AgentBadge::Working),
            reason: None,
            exited: false,
            dnd: false,
            unmeasured: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            subline: None,
            tab: Some(1),
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
        },
        AgentRow {
            harness: None,
            model: None,
            route: None,
            reach: Reach::Locate,
            spawned_by_session: None,
            harness_session_id: None,
            squad: Some(1),
            name: "watcher".into(),
            pane_id: None,
            portal: None,
            badge: None,
            reason: None,
            exited: false,
            dnd: false,
            unmeasured: false,
            answerable: None,
            attach_id: Some("deadbee1".into()),
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
        },
    ];
    let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
    assert!(
        labels.iter().any(|l| l == "footnote › build ·2"),
        "unnamed pane row names its tab with `·N`: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "footnote › watcher"),
        "watch-only row has no ordinal: {labels:?}"
    );

    // x-0f9d US3/AC4: name the hosting tab (id 1, the 2nd tab) - the pane
    // row now resolves inside-out (agent leads, tab NAME as context, no
    // `·N`); the watch-only row still falls back to the squad.
    v.layout.squads[0].tabs[1].name = "reviews".into();
    v.layout.squads[0].tabs[1].named = true;
    let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
    assert!(
        labels.iter().any(|l| l == "build › reviews"),
        "named tab supplies the hosting context, not `·N`: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "footnote › build ·2"),
        "the `·N` ordinal is gone once the tab is named: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "footnote › watcher"),
        "watch-only row still falls back to the squad: {labels:?}"
    );
}

#[test]
fn squad_rollup_bare_pane_folds_to_idle() {
    // x-0090 US4: the pane-union adds bare panes to the agent set; a bare
    // pane whose vt reads Idle folds to Idle so it never overrides a
    // blocked sibling in the x-d140 collapsed-squad rollup (Ord-min over
    // states). (x-d401: a bare pane with NO reading folds to Unmeasured -
    // that path is covered by pane_activity_folds_*, this one pins the
    // rollup with a measured idle pane.)
    let row = |name: &str, pane, badge| AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: name.into(),
        pane_id: Some(pane),
        portal: None,
        badge,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    let bare = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        pane_activity: Some(ShellActivity::Idle),
        ..row("zsh", 10, None)
    };
    let blocked = row("claude", 11, Some(AgentBadge::Blocked));
    assert_eq!(nav_agent_state(&bare), PaneState::Idle);
    let worst = [&bare, &blocked]
        .iter()
        .map(|a| nav_agent_state(a))
        .min()
        .unwrap();
    assert_eq!(
        worst,
        PaneState::Blocked,
        "blocked wins; the bare pane folds to Idle"
    );
}

#[tokio::test]
async fn selector_opens_on_the_focused_panes_row() {
    // x-e10f AC6-HP: prefix+w opens on the FOCUSED pane's row, not the
    // first actionable one - the operator-confirmed `ctrl+w goes to the
    // top of the sideline each time` defect. AC7-EDGE: a focused pane with
    // no visible row opens on the old row-zero anchor, never an invalid
    // index.
    let mut v = two_pane_view(); // focus = pane 11, the SECOND agent row
    v.layout.agents = vec![
        agent_row("alpha", 10, None, false),
        agent_row("omega", 11, None, false),
    ];
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::OpenSelector, &mut buf)
        .await
        .unwrap();
    let sel = v.selector.expect("selector opened");
    assert!(
        matches!(v.display_rows()[sel], DisplayRow::Agent(a) if a.pane_id == Some(11)),
        "cursor rests on the focused pane's row, not row zero"
    );
    v.layout.focus = 99; // no row hosts pane 99
    dispatch_event(&mut v, Event::OpenSelector, &mut buf)
        .await
        .unwrap();
    assert_eq!(
        v.selector,
        v.selector_anchor(0),
        "an unfocusable seed falls back to the row-zero anchor"
    );
}

#[tokio::test]
async fn navigator_opens_on_the_focused_panes_row_and_re_anchors_on_filter() {
    // x-e10f: prefix+f opens on the focused pane's row (the same seed the
    // global chord rides, AC11's client half); the filter-change
    // cursor = 0 sites keep re-anchoring (ruling d-771c1d85, AC8-FR); a
    // focused pane with no row opens at zero (AC7-EDGE).
    let mut v = two_pane_view();
    v.layout.agents = vec![
        agent_row("alpha", 10, None, false),
        agent_row("omega", 11, None, false),
    ];
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::OpenNav, &mut buf)
        .await
        .unwrap();
    let cursor = v.nav.as_ref().unwrap().cursor;
    let rows = v.nav_filtered(v.nav.as_ref().unwrap());
    assert_ne!(cursor, 0, "pane 11's row is not the first row");
    assert!(
        nav_row_targets_pane(&rows[cursor], 11),
        "opens on the focused pane 11's row"
    );
    v.nav_cycle_state();
    assert_eq!(
        v.nav.as_ref().unwrap().cursor,
        0,
        "a state-filter change still re-anchors to zero"
    );
    v.nav = None;
    v.layout.focus = 99; // no row hosts pane 99
    dispatch_event(&mut v, Event::OpenNav, &mut buf)
        .await
        .unwrap();
    assert_eq!(v.nav.as_ref().unwrap().cursor, 0);
}

#[tokio::test]
async fn global_chord_opens_the_sideline_on_the_focused_row() {
    // x-e10f AC11-HP: ESC[1;7D with no prefix opens the sideline seeded on
    // the focused pane's row. AC12 (consumed, never forwarded) is asserted
    // at the scanner in keys.rs; this proves the emitted event lands the
    // SEEDED selector, end to end through dispatch_event.
    let mut v = two_pane_view();
    v.layout.agents = vec![
        agent_row("alpha", 10, None, false),
        agent_row("omega", 11, None, false),
    ];
    let events = crate::keys::Scanner::default().scan(b"\x1b[1;7D", Instant::now());
    let mut buf: Vec<u8> = Vec::new();
    for ev in events {
        if let Event::Forward(chunk) = &ev {
            panic!("AC12: the chord leaked to the pane: {chunk:?}");
        }
        dispatch_event(&mut v, ev, &mut buf).await.unwrap();
    }
    let sel = v.selector.expect("the chord opened the sideline");
    assert!(
        matches!(v.display_rows()[sel], DisplayRow::Agent(a) if a.pane_id == Some(11)),
        "seeded on the focused pane's row"
    );
}

#[test]
fn nav_cursor_clamps_no_wrap() {
    // Boundaries: Ctrl-p at the top and Ctrl-n past the last filtered row
    // both clamp, never wrap.
    let mut v = two_pane_view();
    let n = v.nav_rows().len();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: 0,
    });
    v.nav_move_cursor(-1);
    assert_eq!(v.nav.as_ref().unwrap().cursor, 0, "clamp at the top");
    for _ in 0..(n + 5) {
        v.nav_move_cursor(1);
    }
    assert_eq!(
        v.nav.as_ref().unwrap().cursor,
        n - 1,
        "clamp at the last row"
    );
}

#[tokio::test]
async fn nav_goto_teleports_cross_squad_then_focuses() {
    // AC4-HP: goto an agent in a collapsed, non-active squad sends
    // SelectSquad then FocusPane in order, and closes the navigator.
    let mut v = two_pane_view(); // active squad = 1 (footnote)
    v.layout.agents = vec![AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(2),
        name: "stuck".into(),
        pane_id: Some(9),
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    }];
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label == "notes › stuck")
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_goto(&mut v, &mut buf).await.unwrap();
    assert!(v.nav.is_none(), "goto closes the navigator");
    let mut cur = std::io::Cursor::new(buf);
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::SelectSquad(2))
    ));
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::FocusPane(9))
    ));
}

#[tokio::test]
async fn nav_goto_same_squad_is_a_bare_focus() {
    // AC4-UI: a pane already in the active squad collapses to a bare
    // FocusPane - no redundant SelectSquad.
    let mut v = unified_rows_view(); // worker: sq1 (active), pane 10
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label == "footnote › worker")
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_goto(&mut v, &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::FocusPane(10))
    ));
    assert!(
        crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).is_err(),
        "bare focus only - no SelectSquad"
    );
}

#[tokio::test]
async fn nav_goto_refusal_keeps_navigator_open() {
    // AC4-FR + Locked 6: Enter on a Blocked card shows a notice, sends
    // nothing, and the navigator stays open.
    let mut v = unified_rows_view();
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label.starts_with("x-blk"))
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    v.notice = None;
    let mut buf: Vec<u8> = Vec::new();
    nav_goto(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "refusal sends nothing");
    assert!(v.notice.is_some(), "refusal explains itself");
    assert!(v.nav.is_some(), "navigator stays open");
}

#[tokio::test]
async fn nav_keys_type_tab_and_esc() {
    // AC2-HP: printable bytes edit the query (never leak). AC3-UI: Tab cycles
    // the state chip. Esc closes (a lone ESC stays pending until the next
    // byte disambiguates it - same fold as search; the trailing byte is
    // swallowed on close).
    let mut v = two_pane_view();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: 0,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_keys(&mut v, b"notes", &mut buf).await.unwrap();
    assert_eq!(v.nav.as_ref().unwrap().query, "notes");
    assert!(buf.is_empty(), "typing sends nothing");
    nav_keys(&mut v, b"\t", &mut buf).await.unwrap();
    assert_eq!(
        v.nav.as_ref().unwrap().state_filter,
        Some(PaneState::Blocked),
        "Tab advances the chip to [blocked]"
    );
    nav_keys(&mut v, b"\x1bx", &mut buf).await.unwrap();
    assert!(v.nav.is_none(), "Esc closes; the trailing x is swallowed");
}

#[tokio::test]
async fn nav_keys_bare_right_gotos_and_left_closes() {
    // x-e10f AC9-HP/AC10-ERR: bare Right reaches the selected row through
    // the same nav_goto Enter uses (a refusal keeps the overlay open and
    // sends nothing - the Notice path is nav_goto's, not new logic), and
    // bare Left closes like Esc. Bare arrows never leak to the pane.
    let mut v = unified_rows_view();
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label.starts_with("x-blk"))
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    v.notice = None;
    let mut buf: Vec<u8> = Vec::new();
    nav_keys(&mut v, b"\x1b[C", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a Notice row sends nothing on Right");
    assert!(v.notice.is_some(), "the refusal names itself");
    assert!(v.nav.is_some(), "AC10-ERR: the overlay stays open");
    nav_keys(&mut v, b"\x1b[D", &mut buf).await.unwrap();
    assert!(v.nav.is_none(), "bare Left closes, like Esc");
    assert!(buf.is_empty(), "arrows never leak to the pane");
    // AC9-HP positive half: Right on an actionable row acts and closes,
    // exactly like Enter (row 0 is the active squad's SelectSquad row).
    let mut v = two_pane_view();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: 0,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_keys(&mut v, b"\x1b[C", &mut buf).await.unwrap();
    assert!(!buf.is_empty(), "Right on an actionable row acts");
    assert!(v.nav.is_none(), "and closes the overlay, like Enter");
}

#[tokio::test]
async fn nav_keys_arrows_move_cursor() {
    // AC1 (ab-63b44059): Down/Up move the cursor one filtered row (same as
    // Ctrl-n/Ctrl-p), clamped no-wrap; arrows never leak to the pane; and
    // printable input still edits the query afterwards (Locked-5).
    let mut v = two_pane_view();
    assert!(v.nav_rows().len() >= 2, "fixture needs >=2 nav rows");
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: 0,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_keys(&mut v, b"\x1b[B", &mut buf).await.unwrap();
    assert_eq!(v.nav.as_ref().unwrap().cursor, 1, "Down -> row 1");
    nav_keys(&mut v, b"\x1b[A", &mut buf).await.unwrap();
    assert_eq!(v.nav.as_ref().unwrap().cursor, 0, "Up -> row 0");
    nav_keys(&mut v, b"\x1b[A", &mut buf).await.unwrap();
    assert_eq!(
        v.nav.as_ref().unwrap().cursor,
        0,
        "Up at top clamps (no wrap)"
    );
    assert!(buf.is_empty(), "arrows send nothing to the pane");
    nav_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert_eq!(
        v.nav.as_ref().unwrap().query,
        "x",
        "letter still edits query"
    );
}

#[tokio::test]
async fn nav_keys_shift_tab_reverse_cycles_state() {
    // AC2 (ab-63b44059): Shift-Tab steps to the PREVIOUS state in the ring
    // (reverse of Tab, which advances None -> Blocked) and re-clamps cursor 0.
    let mut v = two_pane_view();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: 1,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_keys(&mut v, b"\x1b[Z", &mut buf).await.unwrap();
    assert_eq!(
        v.nav.as_ref().unwrap().state_filter,
        Some(PaneState::Empty),
        "Shift-Tab reverses to [empty] (x-d401: the cycle gained empty/unread)"
    );
    assert_eq!(v.nav.as_ref().unwrap().cursor, 0, "cursor re-clamped to 0");
    assert!(buf.is_empty(), "Shift-Tab sends nothing to the pane");
}

#[tokio::test]
async fn nav_keys_split_arrow_carries_across_reads() {
    // AC4 (ab-63b44059): a Down arrow split across two reads (ESC[ then B)
    // carries via nav_esc and still moves the cursor, with no stray byte
    // leaking to the query or the pane.
    let mut v = two_pane_view();
    assert!(v.nav_rows().len() >= 2);
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: 0,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_keys(&mut v, b"\x1b[", &mut buf).await.unwrap();
    assert_eq!(
        v.nav.as_ref().unwrap().cursor,
        0,
        "partial seq: no motion yet"
    );
    nav_keys(&mut v, b"B", &mut buf).await.unwrap();
    assert_eq!(
        v.nav.as_ref().unwrap().cursor,
        1,
        "completed Down moves cursor"
    );
    assert!(
        v.nav.as_ref().unwrap().query.is_empty(),
        "no escape tail leaked into the query"
    );
    assert!(buf.is_empty(), "nothing leaked to the pane");
}

#[test]
fn sideline_scroll_follows_cursor_and_maps_hit() {
    // AC1+AC2 (x-a621): a selector driven below the fold scrolls the sideline
    // to keep it visible, and a click on a scrolled row hit-tests to the right
    // display index (no off-by-offset).
    let mut v = two_pane_view();
    let total = v.display_rows().len();
    assert!(total >= 2, "fixture needs >=2 sideline rows");
    // (x-cd67 US1) The sideline owns row 0, so a `total`-tall terminal now
    // FITS the whole catalog; shrink by one to keep one row below the fold.
    v.term = ((total - 1) as u16, 100); // visible = total - 1
    let visible = v.sideline_visible_rows();
    v.selector = Some(total - 1);
    v.clamp_sideline_offset();
    assert_eq!(
        v.sideline_offset,
        total - visible,
        "offset follows the cursor"
    );
    assert!(
        (total - 1) >= v.sideline_offset && (total - 1) < v.sideline_offset + visible,
        "the cursor row is inside the visible window"
    );
    assert!(v.panel_w() > 1, "fixture panel is visible");
    assert_eq!(
        v.sideline_row_at(0, 0),
        Some(v.sideline_offset),
        "the top drawn row (row 0) hit-tests to the scrolled index"
    );
}

#[test]
fn wheel_scrolls_sideline_offset_when_no_selector() {
    // Fix 3: a wheel over a focused (overflowing) sideline nudges the scroll
    // offset directly when the selector is closed, and stays in range.
    let mut v = two_pane_view();
    let total = v.display_rows().len();
    assert!(total >= 2, "fixture needs >=2 sideline rows");
    v.term = ((total - 1) as u16, 100); // one row below the fold
    let visible = v.sideline_visible_rows();
    v.selector = None;
    v.sideline_offset = 0;
    v.scroll_sideline(true);
    assert_eq!(v.sideline_offset, 1, "wheel-down advances one row");
    v.scroll_sideline(false);
    assert_eq!(v.sideline_offset, 0, "wheel-up retreats one row");
    v.scroll_sideline(false);
    assert_eq!(v.sideline_offset, 0, "wheel-up saturates at the top");
    for _ in 0..total + 5 {
        v.scroll_sideline(true);
    }
    assert_eq!(
        v.sideline_offset,
        total - visible,
        "wheel-down stops at the last full window"
    );
}

#[test]
fn wheel_walks_the_selector_when_open() {
    // Fix 3: with the selector open the wheel reuses the j/k cursor walk so
    // the highlight and offset stay coherent (no raw-offset drift).
    let mut v = two_pane_view();
    let total = v.display_rows().len();
    v.term = ((total - 1) as u16, 100); // overflow, else scroll is a no-op
    let first = v.selector_down(0); // first non-inert stop from the top
    v.selector = Some(first);
    v.scroll_sideline(true);
    assert_eq!(
        v.selector,
        Some(v.selector_down(first)),
        "wheel-down walks the selector to the next stop"
    );
}

#[test]
fn sideline_scroll_zero_when_rows_fit() {
    // AC3 (x-a621): when every row fits the height the offset stays 0, so the
    // frame renders exactly as a non-scrolling sideline.
    let mut v = two_pane_view(); // tall terminal, small catalog
    assert!(
        v.display_rows().len() <= v.sideline_visible_rows(),
        "catalog fits the window"
    );
    v.selector = Some(0);
    v.sideline_offset = 9; // stale offset from a prior scrolled session
    v.clamp_sideline_offset();
    assert_eq!(v.sideline_offset, 0, "fits -> offset resets to 0");
}

#[test]
fn sideline_scroll_never_past_last_row() {
    // AC4 (x-a621): an offset left too large by a catalog shrink re-clamps into
    // [0, rows - visible]; it never scrolls past the last row.
    let mut v = two_pane_view();
    let total = v.display_rows().len();
    assert!(total >= 2);
    v.term = (total as u16, 100); // visible = total - 1
    v.selector = None;
    v.hover_row = None;
    v.sideline_offset = 999; // absurd, e.g. after the catalog shrank
    v.clamp_sideline_offset();
    assert_eq!(
        v.sideline_offset,
        total - v.sideline_visible_rows(),
        "clamped to the last full window"
    );
}

#[test]
fn sideline_scroll_window_excludes_chrome_bottom_row() {
    // Regression (code-reviewer): the bottom status row is chrome-owned and
    // overwritten after the sideline paints, and sideline_row_at excludes it,
    // so it must not count as a scroll slot - otherwise follow-cursor scroll
    // parks the last row under the status bar.
    let mut v = two_pane_view();
    v.term = ((MIN_ROWS_FOR_STATUS as usize).max(10) as u16, 100);
    // Clear every chrome trigger, then toggle only status_on so the branch
    // under test is the bottom-chrome subtraction, nothing else.
    v.confirm = None;
    v.create = None;
    v.rename = None;
    v.search = None;
    v.hint = false;
    v.status_on = true;
    assert!(
        v.bottom_row_is_chrome(),
        "status bar occupies the bottom row"
    );
    // (x-cd67 US1) The sideline owns row 0, so the usable height no longer
    // subtracts the tab-bar row - only the chrome bottom row.
    assert_eq!(
        v.sideline_visible_rows(),
        v.term.0 as usize - 1,
        "chrome bottom row is not a scroll slot"
    );
    v.status_on = false;
    assert!(
        !v.bottom_row_is_chrome(),
        "no chrome -> bottom row reclaimed"
    );
    assert_eq!(
        v.sideline_visible_rows(),
        v.term.0 as usize,
        "with no chrome the full terminal height is usable"
    );
}

#[test]
fn nav_cursor_re_clamps_on_layout_shrink() {
    // AC1-FR / AC2-EDGE: a layout push that shrinks the catalog under an open
    // navigator re-clamps the cursor into the live rows (no past-the-end
    // marker, no mis-targeted Enter) without reopening the overlay.
    let mut v = two_pane_view();
    let last = v.nav_rows().len() - 1;
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: last,
    });
    v.set_layout(LayoutView {
        squads: vec![meta(2, "notes", 1, 0)],
        active_squad: 2,
        panes: vec![(
            20,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 72,
            },
        )],
        focus: 20,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    let n = v.nav_rows().len();
    assert!(n < last + 1, "catalog shrank");
    assert_eq!(
        v.nav.as_ref().unwrap().cursor,
        n - 1,
        "cursor clamped into the shrunk catalog"
    );
    assert!(v.nav.is_some(), "navigator stays open across the push");
}

#[test]
fn nav_rows_lists_plain_panes_and_dedups_agent_panes() {
    // Fold-in (codex): plain panes become goto rows; a pane already shown as
    // an agent row is NOT double-listed (the agent row is the richer view).
    let mut v = two_pane_view();
    v.layout.squads[0].tabs[1].panes = vec![
        PaneMeta {
            id: 10,
            label: "claude".into(),
        },
        PaneMeta {
            id: 20,
            label: "htop".into(),
        },
    ];
    v.layout.agents = vec![AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: "worker".into(),
        pane_id: Some(10),
        portal: None,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
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
    }];
    let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
    assert!(
        labels.iter().any(|l| l == "footnote › 2 › htop"),
        "plain pane listed: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l.ends_with("› claude")),
        "agent-hosted pane not double-listed: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "footnote › worker"),
        "the agent keeps its own row"
    );
}

#[tokio::test]
async fn nav_goto_pane_cross_squad_sends_squad_tab_focus() {
    // Fold-in AC4-HP (now fulfilled): a pane in a non-active squad+tab sends
    // SelectSquad, SelectTab, FocusPane in order.
    let mut v = two_pane_view(); // active squad 1; notes = squad 2, tab id 0
    v.layout.squads[1].tabs[0].panes = vec![PaneMeta {
        id: 55,
        label: "vim".into(),
    }];
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label == "notes › 1 › vim")
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_goto(&mut v, &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::SelectSquad(2))
    ));
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::SelectTab(0))
    ));
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::FocusPane(55))
    ));
}

#[tokio::test]
async fn nav_goto_pane_active_view_is_bare_focus() {
    // AC4-UI: a pane in the active squad AND active tab collapses to a bare
    // FocusPane - no redundant SelectSquad/SelectTab.
    let mut v = two_pane_view(); // active squad 1, active_tab idx 1 (tab id 1)
    v.layout.squads[0].tabs[1].panes = vec![PaneMeta {
        id: 77,
        label: "shell".into(),
    }];
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label == "footnote › 2 › shell")
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_goto(&mut v, &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::FocusPane(77))
    ));
    assert!(
        crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).is_err(),
        "bare focus, no prefix"
    );
}

#[tokio::test]
async fn nav_goto_pane_same_squad_other_tab_selects_tab_only() {
    // A pane in the active squad but a different tab: SelectTab then
    // FocusPane, no SelectSquad.
    let mut v = two_pane_view(); // active squad 1, active_tab idx 1 (id 1)
    v.layout.squads[0].tabs[0].panes = vec![PaneMeta {
        id: 88,
        label: "logs".into(),
    }]; // tab idx 0, id 0
    let idx = v
        .nav_rows()
        .iter()
        .position(|r| r.label == "footnote › 1 › logs")
        .unwrap();
    v.nav = Some(NavView {
        query: String::new(),
        state_filter: None,
        cursor: idx,
    });
    let mut buf: Vec<u8> = Vec::new();
    nav_goto(&mut v, &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::SelectTab(0))
    ));
    assert!(matches!(
        crate::proto::read_msg_sync(&mut cur).unwrap(),
        ClientMsg::Command(Command::FocusPane(88))
    ));
    assert!(
        crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).is_err(),
        "no SelectSquad for the active squad"
    );
}

// ---- x-c929: answer overlay + next-blocked cycle ----

fn answerable(idx_labels: &[(&str, &str)], fp: u8) -> AnswerablePrompt {
    AnswerablePrompt {
        prompt: "Do you want to proceed?".into(),
        options: idx_labels
            .iter()
            .map(|(i, l)| AnswerOption {
                idx: (*i).into(),
                label: (*l).into(),
                keystroke: i.as_bytes().to_vec(),
            })
            .collect(),
        fingerprint: [fp; 32],
        region_lines: 8,
    }
}

fn blocked_row(name: &str, pane: u64, ans: Option<AnswerablePrompt>) -> AgentRow {
    AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: Some(1),
        name: name.into(),
        pane_id: Some(pane),
        badge: Some(AgentBadge::Blocked),
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        answerable: ans,
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
    }
}

// ---- x-b186: density toggle + extended agent table ----

/// A view whose terminal is wide enough for the full extended table.
fn wide_view(agents: Vec<AgentRow>) -> View {
    let mut v = view_with_agents(agents);
    v.term = (24, EXTENDED_PANEL_W + MIN_CONTENT_COLS + 10);
    v
}

// AC1-HP: the cycle passes through all three states and lands back where it
// started, and each state renders a DISTINCT panel geometry - so no press is
// visually inert.
#[test]
fn density_cycle_visits_three_distinct_geometries() {
    let mut v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    assert_eq!(v.density, Density::Regular);
    let regular = v.panel_w();
    v.cycle_density();
    assert_eq!(v.density, Density::Extended);
    let extended = v.panel_w();
    v.cycle_density();
    assert_eq!(v.density, Density::Slim);
    let slim = v.panel_w();
    v.cycle_density();
    assert_eq!(v.density, Density::Regular, "the cycle is closed");
    assert!(
        slim < regular && regular < extended,
        "each density has its own width: slim {slim}, regular {regular}, extended {extended}"
    );
}

// AC1-HP: slim keeps the squad headers AND their rollup counts - the whole
// point of the rail is that it is legible, not blind.
#[test]
fn slim_keeps_header_bands_with_rollups_and_drops_agent_rows() {
    let mut v = wide_view(vec![
        agent_row("w", 4, Some(AgentBadge::Working), false),
        agent_row("b", 5, Some(AgentBadge::Blocked), false),
    ]);
    set_density(&mut v, Density::Slim);
    let rows = v.display_rows();
    assert!(
        rows.iter()
            .all(|r| matches!(r, DisplayRow::Sel(s) if s.tab.is_none())
                || matches!(r, DisplayRow::Header { .. })),
        "slim emits header bands only"
    );
    assert!(
        !rows.is_empty(),
        "a rail with no rows would be blind, not slim"
    );
    // The rollup still folds live state, so squad health reads at rail width.
    let frame = v.compose();
    let top = frame_text(&frame).lines().next().unwrap().to_string();
    assert!(
        top.contains('▲'),
        "the blocked rollup glyph survives the rail width: {top:?}"
    );
}

// An fno-owned row shows every column. Unknown PR is explicit neutral
// state; missing message and age remain empty rather than fabricated.
#[test]
fn extended_table_renders_columns_and_leaves_unknown_cells_empty() {
    let mut owned = agent_row("owned", 4, Some(AgentBadge::Working), false);
    owned.pr = Some(482);
    owned.updated_at = Some(crate::digest_overlay::now_secs().saturating_sub(120));
    owned.tail = Some("wired the reader".into());
    let mut external = agent_row("stranger", 5, Some(AgentBadge::Working), false);
    external.external = true; // no pr, no stamp, no tail by construction

    let mut v = wide_view(vec![owned, external]);
    set_density(&mut v, Density::Extended);
    let frame = v.compose();
    let text = frame_text(&frame);
    let line = |needle: &str| {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no row for {needle} in:\n{text}"))
            .to_string()
    };

    let owned_line = line("owned");
    assert!(owned_line.contains("wired the reader"), "{owned_line:?}");
    assert!(owned_line.contains("#482"), "{owned_line:?}");
    assert!(owned_line.contains("2m"), "{owned_line:?}");

    let ext_line = line("stranger");
    assert!(!ext_line.contains('#'), "no fabricated PR: {ext_line:?}");
    assert!(
        ext_line.contains('—'),
        "unknown PR is explicit: {ext_line:?}"
    );
    // No fabricated message or age.
    for fake in ["n/a", "0s", "?"] {
        assert!(
            !ext_line.contains(fake),
            "external row must not fabricate {fake:?}: {ext_line:?}"
        );
    }
}

// Expanded density retains the regular section visibility policy while
// changing only the composition of agent rows.
#[test]
fn extended_table_preserves_collapsed_and_live_only_section_state() {
    let mut exited = agent_row("dead", 6, None, false);
    exited.exited = true;
    let mut v = wide_view(vec![
        agent_row("alive", 4, Some(AgentBadge::Working), false),
        exited,
    ]);
    // Collapse the squad: in the tree these rows are hidden.
    let key = squad_key(&v.layout, v.layout.active_squad).unwrap();
    v.set_section_view(key.clone(), SectionView::Collapsed);
    v.density = Density::Regular;
    assert!(
        !v.display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Agent(_))),
        "precondition: the collapsed tree hides its agent rows"
    );

    set_density(&mut v, Density::Extended);
    let names: Vec<String> = v
        .display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        names.is_empty(),
        "collapsed section stays hidden: {names:?}"
    );

    v.set_section_view(key.clone(), SectionView::Expanded);
    let names: Vec<String> = v
        .display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["alive", "dead"]);

    v.set_section_view(key, SectionView::LiveOnly);
    let names: Vec<String> = v
        .display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["alive"]);
}

// AC3-UI: the sort toggle re-orders rows AND relabels the header, so the
// press is visible even when the two orders coincide. The attention side
// of the toggle is keyed on evidence (basis + age), not badges - badges
// are a scraped report that reads healthy for a worker dead under two
// hours, which is exactly the row this sort exists to surface.
#[test]
fn sort_toggle_reorders_by_attention_and_relabels() {
    let stale = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        basis: Some("transcript".into()),
        last_activity_age_s: Some(1800),
        ..agent_row("stale-live", 4, Some(AgentBadge::Working), false)
    };
    let fresh = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        basis: Some("transcript".into()),
        last_activity_age_s: Some(30),
        ..agent_row("fresh-live", 5, Some(AgentBadge::Working), false)
    };
    let sunk = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        basis: Some("process-gone".into()),
        last_activity_age_s: Some(40000),
        ..agent_row("gone", 6, None, true)
    };
    let mut v = wide_view(vec![fresh, sunk, stale]);
    v.agent_sort = AgentSort::Squad;
    set_density(&mut v, Density::Extended);

    let names = |v: &View| -> Vec<String> {
        v.display_rows()
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Agent(a) => Some(a.name.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        names(&v),
        ["fresh-live", "gone", "stale-live"],
        "by-squad keeps the tree's own order"
    );
    assert!(frame_text(&v.compose()).contains("agent ↑"));

    v.toggle_agent_sort();
    assert_eq!(
        names(&v),
        ["stale-live", "gone", "fresh-live"],
        "the next prefix+o state reverses the agent-name order"
    );
    assert!(frame_text(&v.compose()).contains("agent ↓"));
}

#[test]
fn missing_sort_values_stay_after_known_values_in_both_directions() {
    let mut message = agent_row("message", 4, Some(AgentBadge::Working), false);
    message.tail = Some("hello".into());
    let mut pr = agent_row("pr", 5, Some(AgentBadge::Working), false);
    pr.pr = Some(482);
    let mut age = agent_row("age", 6, Some(AgentBadge::Working), false);
    age.last_activity_age_s = Some(30);
    let mut v = wide_view(vec![message, pr, age]);
    set_density(&mut v, Density::Extended);

    let names = |v: &View| {
        v.display_rows()
            .iter()
            .filter_map(|row| match row {
                DisplayRow::Agent(agent) => Some(agent.name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for column in [
        AgentSortColumn::LastMessage,
        AgentSortColumn::Pr,
        AgentSortColumn::Age,
    ] {
        v.agent_sort = AgentSort {
            column,
            direction: SortDirection::Ascending,
        };
        let ascending = names(&v);
        v.agent_sort.direction = SortDirection::Descending;
        let descending = names(&v);
        if column == AgentSortColumn::Age {
            assert_eq!(ascending.first().unwrap(), "age");
            assert_eq!(descending.first().unwrap(), "age");
        } else {
            assert_eq!(ascending.last().unwrap(), "age");
            assert_eq!(descending.last().unwrap(), "age");
        }
    }
}

#[test]
fn extended_sort_keeps_lineage_subtrees_together() {
    let mut v = wide_view(vec![
        lineage_row("zeta", 4, None),
        lineage_row("alpha", 5, Some("sid-zeta")),
        lineage_row("aardvark", 6, None),
    ]);
    set_density(&mut v, Density::Extended);
    v.agent_sort = AgentSort::Squad;
    let names = agent_order(&v);
    assert_eq!(names, ["aardvark", "zeta", "alpha"]);
    assert_eq!(rendered_depth(&v, "zeta"), 0);
    assert_eq!(rendered_depth(&v, "alpha"), 1);
}

#[test]
fn status_sort_arrow_fits_inside_the_status_header_span() {
    let layout = TableLayout::fitting(EXTENDED_PANEL_W - 1).unwrap();
    let header = table_head_text(layout, AgentSort::Attention);
    let status: String = header.chars().take(layout.status.width as usize).collect();
    assert!(
        status.contains('↑'),
        "status header must show direction: {status:?}"
    );
}

#[test]
fn age_sort_arrow_survives_the_density_button_on_header_row() {
    let mut v = wide_view(vec![agent_row(
        "agent",
        4,
        Some(AgentBadge::Working),
        false,
    )]);
    set_density(&mut v, Density::Extended);
    v.agent_sort = AgentSort {
        column: AgentSortColumn::Age,
        direction: SortDirection::Ascending,
    };
    let first_line = frame_text(&v.compose()).lines().next().unwrap().to_string();
    assert!(
        first_line.contains("age↑"),
        "age header must remain visible: {first_line:?}"
    );
}

#[test]
fn extended_pr_cell_shows_number_or_neutral_value() {
    let mut known = agent_row("known", 4, Some(AgentBadge::Working), false);
    known.pr = Some(482);
    let unknown = agent_row("unknown", 5, Some(AgentBadge::Working), false);
    let layout = TableLayout::fitting(EXTENDED_PANEL_W - 1).unwrap();
    assert!(table_row_text(&known, layout, 0, 0).contains("#482"));
    assert!(table_row_text(&unknown, layout, 0, 0).contains('—'));
}

// The mux ranker's attention order, pinned to the shared fixture: the
// same file the daemon projection and the Python serializer assert
// against, keeping three independently-implemented sorts identical. The
// `need-decision` row is the seam test - the Decision kind has no
// producer yet, and this fixture row is the only proof its seat is
// reserved and ranked first. Deleting it because "no real row has this"
// removes that proof.
// The attention key reads `NeedKind`'s declaration order as a rank
// (`k as u8`), so the full order is load-bearing twice over: the
// needs-me queue bands on it AND the table's first term reads it. Pin
// every adjacent pair so a reorder fails here instead of silently
// re-tiering the fleet.
#[test]
fn need_kind_declaration_order_is_the_severity_contract() {
    assert!(NeedKind::Decision < NeedKind::MailQuestion);
    assert!(NeedKind::MailQuestion < NeedKind::BlockedAnswerable);
    assert!(NeedKind::BlockedAnswerable < NeedKind::BlockedFocusOnly);
    assert!(NeedKind::BlockedFocusOnly < NeedKind::ReviewWedged);
    assert!(NeedKind::ReviewWedged < NeedKind::BudgetStop);
    assert!(NeedKind::BudgetStop < NeedKind::DoneUnseen);
}

#[test]
fn attention_key_orders_the_shared_fixture() {
    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/agents-attention-order.json"
    ));
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
    let need_of = |kind: Option<&str>| -> Option<NeedKind> {
        match kind? {
            "decision" => Some(NeedKind::Decision),
            "mail_question" => Some(NeedKind::MailQuestion),
            "review_wedged" => Some(NeedKind::ReviewWedged),
            "budget_stop" => Some(NeedKind::BudgetStop),
            _ => None,
        }
    };
    let mut rows: Vec<(AgentRow, Option<NeedKind>)> = fixture["rows"]
        .as_array()
        .expect("rows is an array")
        .iter()
        .map(|r| {
            let row = AgentRow {
                portal: None,
                harness: None,
                model: None,
                route: None,
                basis: r["basis"].as_str().map(str::to_string),
                last_activity_age_s: r["last_activity_age_s"].as_u64(),
                exited: r["exited"].as_bool().unwrap_or(false),
                dnd: false,
                unmeasured: r["unmeasured"].as_bool().unwrap_or(false),
                ..blocked_row(r["name"].as_str().expect("row has a name"), 0, None)
            };
            (row, need_of(r["need"].as_str()))
        })
        .collect();
    rows.sort_by(|(a, n), (b, m)| attention_key(a, *n).cmp(&attention_key(b, *m)));
    let got: Vec<&str> = rows.iter().map(|(a, _)| a.name.as_str()).collect();
    let expected: Vec<&str> = fixture["expected_order"]
        .as_array()
        .expect("expected_order is an array")
        .iter()
        .map(|v| v.as_str().expect("order entry is a string"))
        .collect();
    assert_eq!(got, expected);

    // The seam, asserted on its own so a fixture edit cannot silently
    // drop it: the Decision kind outranks MailQuestion even though
    // nothing constructs it today.
    let (decision_row, decision_need) = rows
        .iter()
        .find(|(a, _)| a.name == "need-decision")
        .expect("fixture carries the decision row");
    let decision_key = attention_key(decision_row, *decision_need);
    let (mail_row, mail_need) = rows
        .iter()
        .find(|(a, _)| a.name == "need-mail")
        .expect("fixture carries the mail row");
    assert!(
        decision_key < attention_key(mail_row, *mail_need),
        "the reserved decision seat leads the severity order"
    );

    // An absent age never floats a row above a real age in the same
    // tier: the ghost row (age null) sits below the worker row (age 30)
    // in the fixture order above, pinned here by direct comparison.
    let (ghost, _) = rows
        .iter()
        .find(|(a, _)| a.name == "ghost")
        .expect("fixture carries the ghost row");
    let (worker, _) = rows
        .iter()
        .find(|(a, _)| a.name == "worker")
        .expect("fixture carries the worker row");
    assert!(attention_key(worker, None) < attention_key(ghost, None));
}

#[test]
fn humanize_age_is_fixed_width_and_renders_absent_as_a_question_mark() {
    for s in [12u64, 2700, 10800, 345600] {
        assert_eq!(humanize_age(Some(s)).chars().count(), 4, "{s}");
    }
    assert_eq!(humanize_age(Some(12)), " 12s");
    assert_eq!(humanize_age(Some(2700)), " 45m");
    assert_eq!(humanize_age(Some(10800)), "  3h");
    assert_eq!(humanize_age(Some(345600)), "  4d");
    // Absent renders EMPTY (a 4-space blank), never a fabricated age.
    assert_eq!(humanize_age(None), "    ");
}

#[test]
fn humanize_age_caps_the_day_count_at_three_digits() {
    // 1000+ days would otherwise render "1000d" (5 chars), breaking the
    // fixed-width-4 invariant the column exists to hold.
    assert_eq!(humanize_age(Some(1000 * 86_400)), "999d");
    assert_eq!(humanize_age(Some(1000 * 86_400)).chars().count(), 4);
}

// The severity bands must come from the ONE existing authority. LatticeState
// declares Working before Blocked, so sorting on it instead of PaneState
// would silently produce the wrong order - this pins the right one.
#[test]
fn status_sort_uses_pane_state_severity_not_lattice_order() {
    assert!(
        PaneState::Blocked < PaneState::Working,
        "PaneState is the severity contract"
    );
    let mut exited = agent_row("gone", 8, Some(AgentBadge::Blocked), false);
    exited.exited = true;
    let mut v = wide_view(vec![
        exited,
        agent_row("live", 9, Some(AgentBadge::Working), false),
    ]);
    set_density(&mut v, Density::Extended);
    v.agent_sort = AgentSort::Attention;
    let names: Vec<String> = v
        .display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        ["live", "gone"],
        "exited sorts last: it is the absence of a severity, not a severity"
    );
}

// AC5-EDGE: extended clamps to the widest legal width and drops columns by
// priority (tail first, then age) rather than crushing the work panes.
#[test]
fn extended_clamps_and_drops_columns_before_starving_panes() {
    let mut v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    set_density(&mut v, Density::Extended);
    assert_eq!(v.panel_w(), EXTENDED_PANEL_W, "wide terminal: every column");
    let layout = TableLayout::fitting(EXTENDED_PANEL_W - 1).unwrap();
    assert!(
        layout.tail.is_some(),
        "wide table includes the message cell"
    );
    assert_eq!(layout.age.start + layout.age.width, layout.text_w);

    // Narrow enough that the full table cannot fit beside a usable pane.
    v.term = (24, MIN_EXTENDED_PANEL_W + MIN_CONTENT_COLS + 3);
    let w = v.panel_w();
    assert!(w < EXTENDED_PANEL_W, "clamped down");
    assert!(
        v.term.1 - w >= MIN_CONTENT_COLS,
        "the work pane keeps its minimum: term {} panel {w}",
        v.term.1
    );
    // The message cell gives its space to the agent name, while age stays
    // visible and right-anchored.
    let layout = TableLayout::fitting(w - 1).unwrap();
    assert!(layout.tail.is_none(), "message cell drops below its floor");
    assert_eq!(layout.age.start + layout.age.width, layout.text_w);

    // Narrower still: the panel hides rather than rendering a nameless table.
    v.term = (24, MIN_CONTENT_COLS + 4);
    assert_eq!(v.panel_w(), 0);
    assert!(v.content_dims().1 >= 1, "never a zero-width content area");
}

#[test]
fn responsive_table_layout_anchors_age_and_shares_flexible_space() {
    let narrow = TableLayout::fitting(MIN_EXTENDED_PANEL_W - 1).unwrap();
    assert_eq!(narrow.agent.width, COL_MIN_NAME);
    assert!(narrow.tail.is_none(), "message is omitted below its floor");
    assert_eq!(narrow.age.start + narrow.age.width, narrow.text_w);

    let wide = TableLayout::fitting(EXTENDED_PANEL_W - 1).unwrap();
    let tail = wide.tail.unwrap();
    assert_eq!(wide.agent.width + tail.width, 54);
    assert!(wide.agent.width > COL_MIN_NAME);
    assert!(tail.width > COL_MIN_TAIL);
}

#[test]
fn extended_preserves_section_hierarchy_and_sorts_within_groups() {
    let mut notes = agent_row("notes-agent", 6, Some(AgentBadge::Working), false);
    notes.squad = Some(2);
    let mut orphan = agent_row("orphan-agent", 7, Some(AgentBadge::Working), false);
    orphan.squad = None;
    let mut v = view_with_agents(vec![
        agent_row("zeta", 4, Some(AgentBadge::Blocked), false),
        agent_row("alpha", 5, Some(AgentBadge::Blocked), false),
        notes,
        orphan,
    ]);
    let mut layout = two_squad_layout(1);
    layout.agents = v.layout.agents.clone();
    layout.backlog = vec![bcard("x-ready", CardState::Ready)];
    v.set_layout(layout);
    v.section_view.insert(
        SectionKey::Squad("/code/notes".into()),
        SectionView::Expanded,
    );
    v.section_view
        .insert(SectionKey::Elsewhere, SectionView::Expanded);
    v.section_view
        .insert(SectionKey::WorkQueue, SectionView::Expanded);
    v.agent_sort = AgentSort::Squad;
    set_density(&mut v, Density::Extended);

    let rows = v.display_rows();
    let first_squad = rows
        .iter()
        .position(|r| {
            matches!(
                r,
                DisplayRow::Sel(SelRow {
                    squad: 1,
                    tab: None
                })
            )
        })
        .unwrap();
    let second_squad = rows
        .iter()
        .position(|r| {
            matches!(
                r,
                DisplayRow::Sel(SelRow {
                    squad: 2,
                    tab: None
                })
            )
        })
        .unwrap();
    let elsewhere = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { label, .. } if *label == "~ elsewhere"))
        .unwrap();
    let backlog = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { label, .. } if *label == "~ backlog"))
        .unwrap();
    assert!(first_squad < second_squad && second_squad < elsewhere && elsewhere < backlog);
    let squad_names: Vec<_> = rows[first_squad..second_squad]
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(squad_names, ["alpha", "zeta"]);
    assert!(rows[second_squad..elsewhere]
        .iter()
        .any(|r| matches!(r, DisplayRow::Agent(a) if a.name == "notes-agent")));
    assert!(rows[elsewhere..backlog]
        .iter()
        .any(|r| matches!(r, DisplayRow::Agent(a) if a.name == "orphan-agent")));
    assert!(rows[backlog..]
        .iter()
        .any(|r| matches!(r, DisplayRow::Card(c) if c.id == "x-ready")));
}

#[test]
fn extended_table_preserves_lineage_depth_in_rendered_agent_names() {
    let mut v = wide_view(vec![
        lineage_row("parent", 4, None),
        lineage_row("child", 5, Some("sid-parent")),
    ]);
    set_density(&mut v, Density::Extended);
    let rendered = frame_text(&v.compose());
    let child_line = rendered
        .lines()
        .find(|line| line.contains("child"))
        .unwrap();
    assert!(
        child_line.contains("  child"),
        "child keeps lineage indent: {child_line:?}"
    );
}

#[test]
fn table_header_click_sets_one_column_and_toggles_direction() {
    let mut v = wide_view(vec![agent_row(
        "agent",
        4,
        Some(AgentBadge::Working),
        false,
    )]);
    set_density(&mut v, Density::Extended);
    let layout = TableLayout::fitting(v.panel_w() - 1).unwrap();
    assert!(matches!(
        v.chrome_hit(0, layout.agent.start),
        Some(ChromeHit::SortColumn(AgentSortColumn::Agent))
    ));
    v.set_agent_sort_column(AgentSortColumn::Agent);
    assert_eq!(v.agent_sort.column, AgentSortColumn::Agent);
    assert_eq!(v.agent_sort.direction, SortDirection::Ascending);
    v.set_agent_sort_column(AgentSortColumn::Agent);
    assert_eq!(v.agent_sort.direction, SortDirection::Descending);
}

// AC5-EDGE at startup: a persisted Extended restored onto a now-narrow
// terminal degrades through the same clamp instead of corrupting the layout.
#[test]
fn persisted_extended_on_a_narrow_terminal_degrades_not_corrupts() {
    let mut v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    set_density(&mut v, Density::Extended);
    for cols in [200u16, 120, 90, 70, 50, 30, 10] {
        v.term = (24, cols);
        let w = v.panel_w();
        assert!(
            w == 0 || cols - w >= MIN_CONTENT_COLS,
            "cols {cols} panel {w}"
        );
        // The paint must not panic at any of these widths.
        let _ = v.compose();
    }
}

// The x-260a invariant in ALL THREE densities: every painted sideline line is
// exactly one display row, which is what keeps hit-testing honest.
#[test]
fn painted_lines_match_display_rows_in_every_density() {
    let mut v = wide_view(vec![
        agent_row("a", 4, Some(AgentBadge::Working), false),
        agent_row("b", 5, Some(AgentBadge::Blocked), false),
    ]);
    for d in [Density::Slim, Density::Regular, Density::Extended] {
        v.density = d;
        let n = v.display_rows().len();
        let panel_w = v.panel_w();
        assert!(panel_w > 0, "{d:?} should render at this width");
        // Every row index in range hit-tests back to ITSELF at its painted
        // row - the property `sideline_row_at` needs to stay correct.
        for i in 0..n.min(v.term.0 as usize) {
            assert_eq!(
                v.sideline_row_at(i as u16, 0),
                Some(i),
                "{d:?}: painted row {i} must resolve to display row {i}"
            );
        }
    }
}

// The selector follows the AGENT across a re-sort, not the row index.
#[test]
fn selector_follows_the_agent_across_a_resort() {
    let mut v = wide_view(vec![
        agent_row("idle", 4, None, false),
        agent_row("blocked", 5, Some(AgentBadge::Blocked), false),
    ]);
    set_density(&mut v, Density::Extended);
    let idle_at = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == "idle"))
        .unwrap();
    v.selector = Some(idle_at);
    v.toggle_agent_sort();
    let now_at = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == "idle"))
        .unwrap();
    assert_ne!(now_at, idle_at, "the re-sort moved this agent");
    assert_eq!(
        v.selector,
        Some(now_at),
        "the cursor followed the agent, not the index"
    );
}

// The sort toggle must stay VISIBLE at every width the table renders at.
// Appending the label past the last column silently overflowed the panel by
// 12 columns once the tail dropped, and the painter cut it - so the toggle
// looked like a dead control exactly where the table is hardest to read.
#[test]
fn sort_label_survives_every_column_configuration() {
    for text_w in [EXTENDED_PANEL_W - 1, MIN_EXTENDED_PANEL_W - 1] {
        let layout = TableLayout::fitting(text_w).unwrap();
        let head = table_head_text(layout, AgentSort::Squad);
        assert_eq!(head.chars().map(glyph_cols).sum::<usize>(), text_w as usize);
    }
}

// (codex P2) Slim is the explicitly NON-hidden density, so a narrow terminal
// must clamp it, not make it vanish. Its admit floor is MIN_SLIM, so it
// renders squished where a Regular tree - whose admit floor is PANEL_W -
// instead auto-hides (x-2e86 preserves that per-density asymmetry).
#[test]
fn slim_clamps_on_a_narrow_terminal_instead_of_hiding() {
    let mut v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    set_density(&mut v, Density::Slim);
    // Narrower than the rail wants, but wide enough to still show one.
    v.term = (24, MIN_CONTENT_COLS + SLIM_PANEL_W - 4);
    let w = v.panel_w();
    assert!(w > 0, "slim must not hide here");
    assert!(w < SLIM_PANEL_W, "and it clamped: {w}");
    assert!(v.term.1 - w >= MIN_CONTENT_COLS, "pane keeps its minimum");
    let _ = v.compose(); // must not panic at the clamped width
                         // Regular at the same width still auto-hides: its
                         // admit floor (PANEL_W) is above this room, so the
                         // tree yields to the panes rather than squishing.
    v.density = Density::Regular;
    assert_eq!(v.panel_w(), 0);
}

// (codex P2) A column header with nothing under it reads as a stalled table.
#[test]
fn extended_zero_agents_states_the_empty_case() {
    let mut v = wide_view(vec![]);
    set_density(&mut v, Density::Extended);
    let rows = v.display_rows();
    assert!(matches!(rows.first(), Some(DisplayRow::TableHead)));
    assert!(
        rows.iter().any(|r| matches!(r, DisplayRow::TableEmpty)),
        "zero agents renders an explicit empty-state line"
    );
    assert!(frame_text(&v.compose()).contains("no agents"));
}

// (codex P2) The painter advances by DISPLAY columns (`glyph_cols`), so a
// cell padded by scalar count occupies more columns than it reserved and
// shoves every following cell out of alignment.
//
// Uses the trigram block, which is what `glyph_cols` actually treats as
// wide. A CJK name does NOT reproduce this today: `glyph_cols` reports 1 for
// it, so the painter and the padding agree - the sideline's width model is
// trigram-only, which is a pre-existing gap this table neither introduced
// nor fixes.
#[test]
fn table_cells_align_with_double_width_glyphs() {
    let mut wide_name = agent_row("☰☰☰ menu", 4, Some(AgentBadge::Working), false);
    wide_name.pr = Some(7);
    let plain = agent_row("ascii", 5, Some(AgentBadge::Working), false);
    let layout = TableLayout::fitting(EXTENDED_PANEL_W - 1).unwrap();
    let a = table_row_text(&wide_name, layout, 0, 0);
    let b = table_row_text(&plain, layout, 0, 0);
    let width = |s: &str| s.chars().map(glyph_cols).sum::<usize>();
    assert_eq!(
        width(&a),
        width(&b),
        "rows must occupy equal display width:\n{a:?}\n{b:?}"
    );
}

// (codex P1) A scrape tick that flips one badge RE-ORDERS a status-sorted
// table. Preserving only the numeric index would slide the cursor onto a
// different agent, so the next Enter or lifecycle key hits the wrong worker.
#[test]
fn status_sorted_selector_follows_its_agent_across_a_layout_push() {
    let mut v = wide_view(vec![
        agent_row("idle", 4, None, false),
        agent_row("busy", 5, Some(AgentBadge::Working), false),
    ]);
    set_density(&mut v, Density::Extended);
    v.agent_sort = AgentSort::Attention;
    let at = |v: &View, name: &str| {
        v.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == name))
            .unwrap()
    };
    let idle_at = at(&v, "idle");
    v.selector = Some(idle_at);

    // The SELECTED agent becomes blocked, so it outranks the working row
    // and jumps up a band - the row under the cursor is the one that moves.
    let mut next = v.layout.clone();
    next.agents[0].badge = Some(AgentBadge::Blocked);
    v.set_layout(next);

    let now = at(&v, "idle");
    assert_ne!(now, idle_at, "precondition: the re-sort moved this agent");
    assert_eq!(
        v.selector,
        Some(now),
        "the cursor must follow the agent, not the index"
    );
}

// (codex P2) Extended puts an inert column header at index 0, so opening the
// selector there paints no cursor and Enter only rings.
#[test]
fn open_selector_skips_the_inert_table_header() {
    let mut v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    set_density(&mut v, Density::Extended);
    assert!(matches!(
        v.display_rows().first(),
        Some(DisplayRow::TableHead)
    ));
    let anchored = v.selector_anchor(0).unwrap();
    assert!(
        !row_is_inert(&v.display_rows()[anchored]),
        "the selector opens on an actionable row"
    );
    assert!(v.row_action(anchored).is_some(), "and Enter does something");
}

// (codex P2) Slim renders on terminals narrower than Regular needs. Gating
// the selector on the regular width left that rail clickable but not
// keyboard-reachable - the mouse-only trap this feature forbids.
#[test]
fn slim_width_that_renders_is_also_keyboard_selectable() {
    let mut v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    set_density(&mut v, Density::Slim);
    v.term = (24, MIN_CONTENT_COLS + MIN_SLIM_PANEL_W + 1);
    assert!(v.panel_w() > 0, "the rail renders at this width");
    assert!(
        v.term.1 < PANEL_W + MIN_CONTENT_COLS,
        "and it is below the old regular-width selector gate"
    );
}

// (codex P2) A re-order can push the selected agent out of the scroll
// window; a cursor with no visible row still takes contextual keys.
#[test]
fn resort_scrolls_the_selection_back_into_view() {
    let agents: Vec<AgentRow> = (0..40)
        .map(|i| {
            agent_row(
                &format!("a{i:02}"),
                100 + i,
                // Only the LAST row is blocked, so a status sort yanks it to
                // the top from far down the list.
                if i == 39 {
                    Some(AgentBadge::Blocked)
                } else {
                    None
                },
                false,
            )
        })
        .collect();
    let mut v = wide_view(agents);
    v.term = (12, EXTENDED_PANEL_W + MIN_CONTENT_COLS + 10); // short: scrolls
    set_density(&mut v, Density::Extended);
    let at = |v: &View, n: &str| {
        v.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == n))
            .unwrap()
    };
    v.selector = Some(at(&v, "a39"));
    v.clamp_sideline_offset();
    v.toggle_agent_sort();

    let cur = v.selector.unwrap();
    let visible = v.sideline_visible_rows();
    assert!(
        cur >= v.sideline_offset && cur < v.sideline_offset + visible,
        "selection {cur} must stay inside the window [{}, {})",
        v.sideline_offset,
        v.sideline_offset + visible
    );
}

// The density button is a real click target, routed to the SAME mutation the
// keybind runs, and it never becomes the only way in.
#[test]
fn density_button_click_routes_to_the_cycle() {
    let v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    let range = v.density_button_range(v.panel_w() as usize).unwrap();
    assert!(matches!(
        v.chrome_hit(0, range.start as u16 + 1),
        Some(ChromeHit::CycleDensity)
    ));
    // One row down is an ordinary sideline row again - the button is chrome
    // pinned to row 0, not a column.
    assert!(!matches!(
        v.chrome_hit(1, range.start as u16 + 1),
        Some(ChromeHit::CycleDensity)
    ));
    // Keybind parity (Locked 5): the gesture exists without the mouse.
    assert_eq!(
        crate::keys::resolve_chord(b'B'),
        crate::keys::Event::CycleDensity
    );
    assert_eq!(
        crate::keys::resolve_chord(b'o'),
        crate::keys::Event::ToggleAgentSort
    );
}

// The button must not eat the header rollup it sits beside (the regression
// the reserve-don't-overlay approach exists to prevent).
#[test]
fn density_button_preserves_the_top_header_rollup() {
    let mut v = wide_view(vec![agent_row("b", 5, Some(AgentBadge::Blocked), false)]);
    v.density = Density::Regular;
    let top = frame_text(&v.compose()).lines().next().unwrap().to_string();
    assert!(
        top.contains('▲'),
        "rollup survives beside the button: {top:?}"
    );
    assert!(
        top.contains(density_glyph(Density::Regular)),
        "and the button is there too: {top:?}"
    );
}

#[test]
fn density_button_glyph_sits_one_column_off_the_divider() {
    // AC4-UI (x-2e86): the glyph leads the button and the divider-adjacent
    // cell is a plain (non-inverse) pad, so the glyph reads one column in
    // from the border. The pad costs the header band NOTHING (range.start is
    // unchanged), which is why the tight-slim rollup test still holds.
    let v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    let pw = v.panel_w() as usize;
    let range = v.density_button_range(pw).unwrap();
    let frame = v.compose();
    // Row 0, so the cell index is the column.
    let glyph_cell = &frame.cells[range.start];
    let pad_cell = &frame.cells[range.end - 1]; // the cell before the divider
    assert_eq!(
        glyph_cell.c,
        density_glyph(v.density),
        "glyph leads the button"
    );
    assert_eq!(
        glyph_cell.flags,
        cell_flags::INVERSE,
        "glyph cell is the button"
    );
    assert_eq!(pad_cell.c, ' ', "the divider-adjacent cell is a pad");
    assert_eq!(
        pad_cell.flags, 0,
        "the pad is plain, giving real breathing room"
    );
    // The divider itself is the very next column.
    assert_eq!(
        range.end,
        pw - 1,
        "the button ends right before the divider"
    );
}

pub(super) fn view_with_agents(agents: Vec<AgentRow>) -> View {
    let mut v = two_pane_view();
    v.layout.agents = agents;
    v
}

// ---- US9: hierarchy-ordered sideline from mesh crown metadata ----

/// A live squad-1 row carrying a crown (or none). Not blocked - a plain live
/// coordinator/leaf, the shape the sideline orders.
fn crowned_row(name: &str, pane: u64, level: Option<u32>, scope: Option<&str>) -> AgentRow {
    let mut r = blocked_row(name, pane, None);
    r.badge = None;
    r.crown_level = level;
    r.crown_scope = scope.map(str::to_string);
    r
}

/// A crowned_row carrying a lineage edge: `parent` names another row's
/// harness_session_id (None = a root). The row's own id is "sid-<name>".
fn lineage_row(name: &str, pane: u64, parent: Option<&str>) -> AgentRow {
    let mut r = crowned_row(name, pane, None, None);
    r.harness_session_id = Some(format!("sid-{name}"));
    r.spawned_by_session = parent.map(str::to_string);
    r
}

/// The rendered order of agent-row names (post lineage layout).
fn agent_order(v: &View) -> Vec<String> {
    v.display_rows()
        .iter()
        .filter_map(|r| match r {
            DisplayRow::Agent(a) => Some(a.name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn lineage_child_sorts_beneath_its_parent_within_squad() {
    let v = view_with_agents(vec![
        lineage_row("worker-a", 2, Some("sid-king")),
        lineage_row("king", 3, None),
        lineage_row("worker-b", 4, Some("sid-king")),
    ]);
    // Pre-order: the parent first, its children beneath it keeping input
    // order among siblings. Authority rank (crown_level) no longer moves a
    // row; lineage does.
    assert_eq!(agent_order(&v), vec!["king", "worker-a", "worker-b"]);
}

#[test]
fn lineage_grandchild_renders_between_parent_and_later_sibling() {
    let v = view_with_agents(vec![
        lineage_row("king", 2, None),
        lineage_row("child-a", 3, Some("sid-king")),
        lineage_row("child-b", 4, Some("sid-king")),
        lineage_row("grandchild", 5, Some("sid-child-a")),
    ]);
    // Pre-order nests the grandchild under ITS parent, ahead of the
    // parent's later sibling.
    assert_eq!(
        agent_order(&v),
        vec!["king", "child-a", "grandchild", "child-b"]
    );
}

#[test]
fn crown_all_uncrowned_squad_keeps_order_and_paints_no_badge_or_indent() {
    // The common case (Operator Intent): a stable sort of equal ranks is
    // the identity, so the squad's order is unchanged and no crown ceremony
    // reaches the paint - the no-regression path.
    let v = view_with_agents(vec![
        crowned_row("zeta", 2, None, None),
        crowned_row("alpha", 3, None, None),
    ]);
    assert_eq!(agent_order(&v), vec!["zeta", "alpha"]);
    let text = frame_text(&v.compose());
    for line in text
        .lines()
        .filter(|l| l.contains("zeta") || l.contains("alpha"))
    {
        assert!(
            !line.contains('['),
            "no crown badge on un-crowned row: {line:?}"
        );
    }
}

/// (x-132c) The rendered lineage depth of the agent row named `name`,
/// read from the compose-pass depth vec (the painter's own source).
fn rendered_depth(v: &View, name: &str) -> usize {
    let (rows, depths) = v.display_rows_with_depths();
    rows.iter()
        .position(|r| match r {
            DisplayRow::Agent(a) => a.name == name,
            _ => false,
        })
        .map(|i| depths[i])
        .unwrap_or(0)
}

#[test]
fn lineage_indent_is_depth_within_squad() {
    let v = view_with_agents(vec![
        lineage_row("king", 2, None),
        lineage_row("dir", 3, Some("sid-king")),
        lineage_row("ic", 4, Some("sid-dir")),
    ]);
    let steps = |name: &str| rendered_depth(&v, name);
    assert_eq!(steps("king"), 0);
    assert_eq!(steps("dir"), 1);
    assert_eq!(steps("ic"), 2);

    // A parent and a stranger leaf: the leaf is a ROOT (absent parent),
    // never nested under a row it has no edge to.
    let v2 = view_with_agents(vec![
        lineage_row("king", 2, None),
        lineage_row("stranger", 3, None),
    ]);
    let steps2 = |name: &str| rendered_depth(&v2, name);
    assert_eq!(steps2("king"), 0);
    assert_eq!(steps2("stranger"), 0);
}

#[test]
fn lineage_indent_ignores_exited_parent_hidden_by_liveonly() {
    // The indent must reference only rows that render: an exited parent
    // dropped by a LiveOnly squad is ABSENT from the set, so its child
    // roots rather than indenting under a phantom.
    let mut parent = lineage_row("parent", 2, None);
    parent.exited = true;
    let child = lineage_row("child", 3, Some("sid-parent"));
    let mut v = view_with_agents(vec![parent, child]);
    let indent = |v: &View, name: &str| rendered_depth(v, name);
    // Expanded: the exited parent still renders, so the child indents.
    assert_eq!(indent(&v, "child"), 1);
    // LiveOnly hides the exited parent -> absent from the rendered set ->
    // the child is a root.
    v.cycle_squad(1);
    assert_eq!(v.squad_view(1), SectionView::LiveOnly);
    assert_eq!(
        indent(&v, "child"),
        0,
        "no phantom indent under a hidden parent"
    );
}

#[test]
fn lineage_nests_within_elsewhere_and_roots_strangers() {
    // `~ elsewhere` now carries the same lineage join as the squads: an
    // orphan spawned by another orphan nests beneath it, while an unrelated
    // orphan stays flat (absent parent = root, never nested under a
    // stranger).
    let mut parent = lineage_row("orphan-parent", 2, None);
    parent.squad = None;
    let mut child = lineage_row("orphan-child", 3, Some("sid-orphan-parent"));
    child.squad = None;
    let mut stranger = lineage_row("orphan-stranger", 4, None);
    stranger.squad = None;
    // `~ elsewhere` defaults to Collapsed; open it so the nesting this
    // test asserts actually renders (the depth vec only covers rows that
    // paint - that is the point of the compose-pass design).
    let mut v = view_with_agents(vec![parent, child, stranger]);
    v.section_view
        .insert(SectionKey::Elsewhere, SectionView::Expanded);
    let indent = |name: &str| rendered_depth(&v, name);
    assert_eq!(indent("orphan-parent"), 0);
    assert_eq!(indent("orphan-child"), 1, "a child nests under its parent");
    assert_eq!(
        indent("orphan-stranger"),
        0,
        "no edge to either row: a root, not nested under a stranger"
    );
}

#[test]
fn crown_malformed_scope_orders_by_level_and_badges_question_mark() {
    // A partial crown (level set, scope None) must never panic: it orders at
    // its altitude and its badge scope degrades to `?`.
    let v = view_with_agents(vec![
        crowned_row("dir", 2, Some(1), None),
        crowned_row("leaf", 3, None, None),
    ]);
    assert_eq!(agent_order(&v).first().map(String::as_str), Some("dir"));
    let text = frame_text(&v.compose());
    let dir_line = text.lines().find(|l| l.contains("dir")).unwrap();
    assert!(
        dir_line.contains("[L1 ?]"),
        "malformed scope badges ?: {dir_line:?}"
    );
}

// A roster row with an arbitrary badge/seen (x-feec): a join target for a
// fold item, or a done-unseen leg-1 row.
fn agent_row(name: &str, pane: u64, badge: Option<AgentBadge>, seen: bool) -> AgentRow {
    let mut r = blocked_row(name, pane, None);
    r.badge = badge;
    r.seen = seen;
    r
}

/// (x-d401) The top-K fold's target set after this branch split the old
/// blind `Idle` three ways. Every non-attention state must still fold: on
/// `origin/main` a badgeless row was `Idle` and folded, so folding only
/// `Idle` would strand both a pristine shell AND every badgeless bg
/// worker (`server.rs` hard-codes `pane_activity: None` on watch-only
/// paneless rows), driving `idle_budget` to zero on any real fleet. The
/// `?` glyph, not the fold, is what keeps a no-reading row honest.
#[test]
fn idle_fold_takes_every_non_attention_state() {
    let with = |activity: Option<ShellActivity>| {
        let mut r = agent_row("w", 1, None, true);
        r.pane_activity = activity;
        r
    };
    assert!(
        is_idle_row(&with(Some(ShellActivity::Empty))),
        "a pristine shell folds, else the fold cap dies on a shell-heavy squad"
    );
    assert!(is_idle_row(&with(Some(ShellActivity::Idle))), "idle folds");
    assert!(
        is_idle_row(&with(Some(ShellActivity::Unmeasured))),
        "an unmeasured row folds: the `?` glyph carries the honesty, not the cap"
    );
    assert!(
        is_idle_row(&with(None)),
        "a badgeless bg worker (pane_activity None) folds as it did before the split"
    );
    assert!(
        !is_idle_row(&with(Some(ShellActivity::Running))),
        "a running pane is attention, not fold"
    );
    let mut dead = with(Some(ShellActivity::Empty));
    dead.exited = true;
    assert!(
        !is_idle_row(&dead),
        "dead rows are the section view's business"
    );
}

fn fold_item(kind: &str, name: &str, live: bool) -> crate::needs_overlay::FoldItem {
    crate::needs_overlay::FoldItem {
        kind: kind.into(),
        session_id: format!("sess-{name}"),
        node: Some(name.into()),
        name: Some(name.into()),
        title: None,
        ts: "2026-07-03T02:00:00Z".into(),
        evidence: format!("{kind} evidence"),
        live,
    }
}

fn mine_item(n: usize, text: &str, done: bool) -> crate::needs_overlay::MineItem {
    crate::needs_overlay::MineItem {
        n,
        text: text.into(),
        done,
        node: None,
    }
}

fn question_item(
    id: &str,
    options: &[&str],
    live: Option<bool>,
) -> crate::needs_overlay::QuestionItem {
    crate::needs_overlay::QuestionItem {
        id: id.into(),
        question: format!("prose for {id}"),
        ask: Some(format!("ask for {id}")),
        asker: Some("fno-peer".into()),
        node: None,
        options: options.iter().map(|s| s.to_string()).collect(),
        live,
        rank: None,
    }
}

// (x-6851 US3) AC3-HP: a squad-matched agent whose cwd is FOREIGN to the
// squad's project gets a dim, inert Sub row carrying the foreign cwd_base
// alone (no branch); the selector skips it; and line 1 carries no
// ` (basename)` suffix (that is orphan-only now).
#[test]
fn foreign_cwd_agent_gets_dim_inert_subline() {
    let mut agent = blocked_row("worker", 4, None);
    // squad 1 is "footnote" (/code/footnote); a "regready" cwd is foreign.
    agent.cwd_base = Some("regready".into());
    agent.subline = Some("main · regready".into()); // server subline is ignored now
    let mut v = view_with_agents(vec![agent]);
    let rows = v.display_rows();
    let ai = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == "worker"))
        .unwrap();
    assert!(
        matches!(rows[ai + 1], DisplayRow::Sub(_)),
        "foreign-cwd agent gets a sub row"
    );
    // AC1-UI: inert - no row action, and the selector steps over it.
    assert!(v.row_action(ai + 1).is_none(), "sub row is not actionable");
    assert!(v.selector_down(ai) > ai + 1, "selector skips the sub row");
    assert_eq!(v.selector_anchor(ai + 1), v.selector_anchor(ai + 2));

    let frame = v.compose();
    let cols = frame.cols as usize;
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[ai].contains("worker"));
    assert!(
        !lines[ai].contains("(regready)"),
        "no line-1 basename suffix on a squad-matched row: {:?}",
        lines[ai]
    );
    // The subline is the foreign cwd_base alone - no branch, no ` · `.
    assert!(
        lines[ai + 1].contains("regready"),
        "foreign cwd on line 2: {:?}",
        lines[ai + 1]
    );
    assert!(
        !lines[ai + 1].contains('\u{b7}'),
        "no branch on the subline: {:?}",
        lines[ai + 1]
    );
    // The sub row paints DIM.
    let sub_cell = frame.cells[(ai + 1) * cols + 4];
    assert_eq!(sub_cell.flags & cell_flags::DIM, cell_flags::DIM);
    // AC1-UI: hover on the sub index paints no INVERSE bar.
    v.hover_row = Some(ai + 1);
    let frame = v.compose();
    assert_eq!(
        frame.cells[(ai + 1) * cols + 4].flags & cell_flags::INVERSE,
        0,
        "an inert sub row is never highlighted"
    );
}

// (x-6851 US3) AC3-HP count: squad "footnote" with a same-project agent A and
// a foreign agent B - A is one clean row, B gets exactly one Sub row.
#[test]
fn exception_subline_only_for_foreign_agent() {
    let mut a = blocked_row("A", 4, None);
    a.cwd_base = Some("footnote".into()); // same project as squad 1
    let mut b = blocked_row("B", 5, None);
    b.cwd_base = Some("regready".into()); // foreign
    let v = view_with_agents(vec![a, b]);
    let rows = v.display_rows();
    let subs = rows
        .iter()
        .filter(|r| matches!(r, DisplayRow::Sub(_)))
        .count();
    assert_eq!(subs, 1, "exactly one subline (the foreign agent's)");
    let bi = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(x) if x.name == "B"))
        .unwrap();
    assert!(
        matches!(rows[bi + 1], DisplayRow::Sub(_)),
        "the sub row follows the foreign agent B"
    );
    let ai = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(x) if x.name == "A"))
        .unwrap();
    assert!(
        !matches!(rows[ai + 1], DisplayRow::Sub(_)),
        "same-project agent A has no sub row"
    );
}

// (x-6851 US3, AC4-EDGE) A pathologically narrow panel (text_w < 3) must
// truncate a foreign-cwd sub row without underflow or panic.
#[test]
fn draw_sideline_narrow_panel_truncates_subline_without_panic() {
    let mut agent = blocked_row("worker", 4, None);
    agent.cwd_base = Some("regready".into()); // foreign -> a Sub row exists to truncate
    let v = view_with_agents(vec![agent]);
    let (rows, cols, panel_w) = (10usize, 40usize, 2usize); // text_w = 1
    let mut cells = vec![Cell::default(); rows * cols];
    v.draw_sideline(&mut cells, rows, cols, panel_w); // must not panic
                                                      // The divider still lands at panel_w - 1 on every drawn row.
    assert_eq!(cells[panel_w - 1].c, '│');
    assert_eq!(cells[cols + (panel_w - 1)].c, '│');
}

// (x-6851 US3) AC3-HP negative + AC4-EDGE: a same-project agent (cwd matches
// the squad basename) and a cwd-less agent both emit NO Sub row.
#[test]
fn same_project_or_absent_cwd_emits_no_sub_row() {
    let bare = blocked_row("worker", 4, None); // cwd_base None (AC4-EDGE)
    assert!(
        !view_with_agents(vec![bare])
            .display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Sub(_))),
        "absent cwd -> no sub row"
    );
    let mut same = blocked_row("worker", 4, None);
    same.cwd_base = Some("footnote".into()); // matches squad 1 "footnote"
    assert!(
        !view_with_agents(vec![same])
            .display_rows()
            .iter()
            .any(|r| matches!(r, DisplayRow::Sub(_))),
        "same-project cwd -> no sub row"
    );
}

#[test]
fn xc929_pad_to_truncates_and_pads() {
    assert_eq!(pad_to("hi", 5), "hi   ");
    assert_eq!(pad_to("hello", 5), "hello");
    assert_eq!(pad_to("hello world", 5), "hell…");
}

// AC1-HP + AC1-EDGE (x-feec): the selected row is marked, an answerable row
// lists its numbered options, a focus-only row is tagged; the answerable
// kind sorts ahead of focus-only (severity order).
#[test]
fn needs_overlay_lines_mark_selection_and_tag_focus_only() {
    let mut v = view_with_agents(vec![
        blocked_row("peer", 4, Some(answerable(&[("1", "Yes"), ("2", "No")], 7))),
        blocked_row("other", 5, None),
    ]);
    v.mine_fold = Some(Vec::new());
    let projection = v.needs_projection();
    let lines = needs_overlay_lines(&projection, 0, NeedsFooter::AsOf, NeedsFooter::AsOf);
    let selected = lines.iter().find(|l| l.contains('▸')).unwrap();
    assert!(selected.contains("peer"), "{selected:?}");
    assert!(lines
        .iter()
        .any(|l| l.contains("other") && l.contains("⚠ focus")));
    assert!(lines.iter().any(|l| l.contains("1. Yes")));
    assert!(lines.iter().any(|l| l.contains("2. No")));
    // Selecting the focus-only row shows no answer options.
    let lines = needs_overlay_lines(&projection, 1, NeedsFooter::AsOf, NeedsFooter::AsOf);
    assert!(!lines.iter().any(|l| l.contains("1. Yes")));
}

// The empty union renders the "nothing needs you" state, never a blank
// overlay (AC4-EDGE), and states the true total when the cap trims (footer).
#[test]
fn needs_overlay_lines_empty_and_capped_footers() {
    let empty_projection = NeedsProjection {
        rows: Vec::new(),
        mine_shown: 0,
        mine_total: 0,
        need_shown: 0,
        need_total: 0,
    };
    let empty = needs_overlay_lines(&empty_projection, 0, NeedsFooter::AsOf, NeedsFooter::AsOf);
    assert!(empty.iter().any(|l| l.contains("nothing needs you")));
    let one_row = NeedRow {
        kind: NeedKind::BudgetStop,
        name: "x".into(),
        reason: "stopped".into(),
        ts: String::new(),
        id_key: "x".into(),
        answerable: None,
        pane_id: Some(1),
        attach_id: None,
        squad: Some(1),
        tab: None,
    };
    let one_projection = NeedsProjection {
        rows: vec![NeedsOverlayRow::Need(one_row.clone())],
        mine_shown: 0,
        mine_total: 0,
        need_shown: 1,
        need_total: 1,
    };
    let degraded =
        needs_overlay_lines(&one_projection, 0, NeedsFooter::AsOf, NeedsFooter::Degraded);
    assert!(degraded
        .iter()
        .any(|l| l.contains("events fold unavailable")));
    let capped_projection = NeedsProjection {
        rows: vec![NeedsOverlayRow::Need(one_row)],
        mine_shown: 0,
        mine_total: 0,
        need_shown: 1,
        need_total: 8,
    };
    let capped = needs_overlay_lines(&capped_projection, 0, NeedsFooter::AsOf, NeedsFooter::AsOf);
    assert!(capped.iter().any(|l| l.contains("1 of 8 shown")));
}

#[test]
fn mine_renders_before_they_need_you_and_preserves_grouped_file_order() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(vec![
        mine_item(1, "first done", true),
        mine_item(2, "first open", false),
        mine_item(3, "second open", false),
    ]);
    v.needs_fold = Some(vec![fold_item("carveout_stale", "pile", true)]);

    let projection = v.needs_projection();
    assert_eq!(projection.rows.len(), 4);
    assert_eq!(projection.rows[0].label(), "first open");
    assert_eq!(projection.rows[1].label(), "second open");
    assert_eq!(projection.rows[2].label(), "first done");
    assert_eq!(projection.rows[3].label(), "pile");
    let lines = needs_overlay_lines(&projection, 0, NeedsFooter::AsOf, NeedsFooter::AsOf);
    let mine = lines.iter().position(|l| l.contains("MINE")).unwrap();
    let they = lines
        .iter()
        .position(|l| l.contains("THEY NEED YOU"))
        .unwrap();
    assert!(mine < they);
    assert!(lines[mine + 1].contains("first open"));
}

#[test]
fn needs_lane_caps_at_ten_and_footer_names_true_total() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(
        (0..11)
            .map(|i| fold_item("carveout_stale", &format!("pile-{i:02}"), true))
            .collect(),
    );
    let projection = v.needs_projection();
    assert_eq!(projection.need_total, 11);
    assert_eq!(projection.need_shown, 10);
    let lines = needs_overlay_lines(&projection, 0, NeedsFooter::AsOf, NeedsFooter::AsOf);
    assert!(lines.iter().any(|l| l.contains("10 of 11 shown")));
}

#[test]
fn mine_failure_is_visible_while_needs_lane_remains() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.mine_degraded = true;
    v.needs_fold = Some(vec![fold_item("carveout_stale", "pile", true)]);
    let projection = v.needs_projection();
    let lines = needs_overlay_lines(&projection, 0, NeedsFooter::Degraded, NeedsFooter::AsOf);
    assert!(lines.iter().any(|l| l.contains("MINE unavailable")));
    assert!(lines.iter().any(|l| l.contains("pile")));
}

#[test]
fn operator_overlay_filters_mail_traffic_and_bare_question_events_keeps_decision_piles() {
    let mut v = view_with_agents(vec![]);
    v.needs_fold = Some(vec![
        fold_item("mail_question", "mail-q", true),
        fold_item("mail_delivery_miss", "mail-miss", true),
        // operator_question is dropped here too (x-f730): the overlay
        // renders it from the richer QuestionItem leg instead, never
        // this bare event - see needs_operator_queue's doc comment.
        fold_item("operator_question", "operator-q", true),
        fold_item("carveout_stale", "pile", true),
    ]);
    // needs_queue() itself still carries mail_question and Question (a
    // live badge still needs both - see
    // mail_question_fold_item_renders_squadless_not_dropped and
    // operator_question_folds_to_question_and_sorts_first); the operator
    // PANEL's own accessor is what filters them.
    let q = v.needs_operator_queue();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].name, "pile");
}

#[test]
fn rendered_cursor_indexes_the_same_projection_row() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(vec![mine_item(1, "mine-one", false)]);
    v.needs_fold = Some(vec![fold_item("carveout_stale", "pile", true)]);
    let projection = v.needs_projection();
    let selected = 1;
    let lines = needs_overlay_lines(&projection, selected, NeedsFooter::AsOf, NeedsFooter::AsOf);
    let selected_line = lines.iter().find(|line| line.contains('▸')).unwrap();
    assert!(selected_line.contains(projection.rows[selected].label()));
}

// x-f730 task 2.2 AC1-HP: x on a MINE row queues its toggle by file index,
// never a stray keystroke to any pane.
#[tokio::test]
async fn answer_keys_x_on_mine_row_queues_toggle() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(vec![mine_item(3, "ship it", false)]);
    v.needs_fold = Some(Vec::new());
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert_eq!(
        v.mine_action,
        Some(crate::needs_overlay::MineMutation::Toggle(3))
    );
    assert!(v.mine_acting, "single-flight guard set while queued");
    assert!(buf.is_empty(), "a MINE toggle never sends a pane keystroke");
}

// x-f730 task 2.2: d on a MINE row queues its drop by file index.
#[tokio::test]
async fn answer_keys_d_on_mine_row_queues_drop() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(vec![mine_item(7, "cut it", false)]);
    v.needs_fold = Some(Vec::new());
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"d", &mut buf).await.unwrap();
    assert_eq!(
        v.mine_action,
        Some(crate::needs_overlay::MineMutation::Drop(7))
    );
}

// x/d on a NEED row (not MINE) are a silent no-op - only a MINE row is
// addressable by either key.
#[tokio::test]
async fn answer_keys_x_and_d_on_need_row_are_inert() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(vec![fold_item("carveout_stale", "pile", true)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert_eq!(v.mine_action, None);
    answer_keys(&mut v, b"d", &mut buf).await.unwrap();
    assert_eq!(v.mine_action, None);
    assert_eq!(v.answers, Some(0), "overlay stays open, row unaffected");
}

// x-f730 task 2.2 AC2-HP: a opens the add entry - even over an empty
// projection, the one case where "any key dismisses" must not apply -
// and typing text then Enter queues an Add with the trimmed text.
#[tokio::test]
async fn answer_keys_a_then_type_then_enter_queues_add() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(Vec::new());
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"a", &mut buf).await.unwrap();
    assert!(
        v.mine_adding.is_some(),
        "a opens the add entry even on an empty lane"
    );
    assert_eq!(
        v.answers,
        Some(0),
        "the empty-dismisses-all rule never fires for a"
    );
    answer_keys(&mut v, b"buy milk\r", &mut buf).await.unwrap();
    assert_eq!(
        v.mine_action,
        Some(crate::needs_overlay::MineMutation::Add("buy milk".into()))
    );
    assert!(v.mine_adding.is_none(), "submit closes the text entry");
}

// x-f730 task 2.2: Esc while adding cancels the text entry without
// queuing a mutation, and without closing the whole overlay.
#[tokio::test]
async fn answer_keys_esc_while_adding_cancels_without_closing_overlay() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(vec![mine_item(1, "existing", false)]);
    v.needs_fold = Some(Vec::new());
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"a", &mut buf).await.unwrap();
    answer_keys(&mut v, b"partial", &mut buf).await.unwrap();
    // A lone Esc byte pends in fold_selector_keys until a disambiguating
    // byte arrives (shared arrow-vs-bare-Esc fold); the trailing space
    // flushes it to a bare 0x1b and is itself swallowed as the
    // disambiguator, never reaching mine_adding as its own keystroke.
    answer_keys(&mut v, b"\x1b ", &mut buf).await.unwrap();
    assert!(v.mine_adding.is_none());
    assert_eq!(v.mine_action, None);
    assert!(
        v.answers.is_some(),
        "Esc cancels the add box, not the overlay"
    );
}

// x-f730 task 2.2 AC3-ERR: a failed mutation shows the failure and
// leaves state untouched (mine_fold, and never a re-fold) - never a
// silent no-op.
#[test]
fn apply_mine_action_result_shows_failure_never_silent() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(vec![mine_item(1, "existing", false)]);
    v.needs_want = false;
    v.apply_mine_action_result(Err(
        "mine: failed: item text must be one non-empty line".into()
    ));
    assert!(!v.mine_acting);
    assert!(!v.needs_want, "a failure never triggers a re-fold");
    let notice = v.notice.as_ref().expect("failure surfaces a notice");
    assert!(notice.0.contains("item text must be one non-empty line"));
    // The (untouched) prior MINE state is exactly what was there before.
    assert_eq!(v.mine_fold.as_ref().unwrap()[0].text, "existing");
}

#[test]
fn apply_mine_action_result_success_requests_refold() {
    let mut v = view_with_agents(vec![]);
    v.needs_want = false;
    v.apply_mine_action_result(Ok(()));
    assert!(!v.mine_acting);
    assert!(
        v.needs_want,
        "success re-folds so the render reflects the file"
    );
}

// x-f730 task 2.3 AC1-HP: a digit on a question with options queues the
// matching option text against the question id, single-flight set, no
// stray keystroke to any pane.
#[tokio::test]
async fn answer_keys_digit_on_question_with_options_queues_answer() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(Vec::new());
    v.questions_fold = Some(vec![question_item("q-1", &["oauth", "apikey"], Some(true))]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"2", &mut buf).await.unwrap();
    assert_eq!(
        v.question_action,
        Some(("q-1".to_string(), "apikey".to_string()))
    );
    assert!(v.question_acting);
    assert!(
        buf.is_empty(),
        "a question answer never sends a pane keystroke"
    );
}

// Refolding after the answer is what actually drops the row - proven
// directly against apply_question_action_result, mirroring the MINE
// pair above.
#[test]
fn apply_question_action_result_success_requests_refold() {
    let mut v = view_with_agents(vec![]);
    v.needs_want = false;
    v.apply_question_action_result(Ok(()));
    assert!(!v.question_acting);
    assert!(v.needs_want, "success re-folds so the row leaves on refold");
}

#[test]
fn apply_question_action_result_failure_shows_notice_never_silent() {
    let mut v = view_with_agents(vec![]);
    v.needs_want = false;
    v.apply_question_action_result(Err("failed to close q-1: locked".into()));
    assert!(!v.question_acting);
    assert!(!v.needs_want, "a failure never triggers a re-fold");
    let notice = v.notice.as_ref().expect("failure surfaces a notice");
    assert!(notice.0.contains("failed to close q-1: locked"));
}

// x-f730 task 2.3 AC2-HP: Enter on a no-options question opens the
// free-text entry; typing then Enter queues the typed answer.
#[tokio::test]
async fn answer_keys_enter_on_no_options_question_opens_free_text_then_sends_it() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(Vec::new());
    v.questions_fold = Some(vec![question_item("q-2", &[], None)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        v.question_answering,
        Some(("q-2".to_string(), String::new()))
    );
    assert_eq!(v.answers, Some(0), "the overlay stays open under the entry");
    answer_keys(&mut v, b"go with oauth\r", &mut buf)
        .await
        .unwrap();
    assert_eq!(
        v.question_action,
        Some(("q-2".to_string(), "go with oauth".to_string()))
    );
    assert!(v.question_answering.is_none());
    assert!(buf.is_empty());
}

// x-f730 task 2.3 AC4-ERR: a digit with no matching option on a
// with-options question is a local BEL, same invariant as a
// non-answerable NEED row - never a stray keystroke, never queued.
#[tokio::test]
async fn answer_keys_digit_with_no_matching_question_option_bels() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(Vec::new());
    v.questions_fold = Some(vec![question_item("q-3", &["oauth"], Some(true))]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"9", &mut buf).await.unwrap();
    assert_eq!(v.question_action, None);
    assert!(!v.question_acting);
    assert!(buf.is_empty());
}

// x-f730 task 2.3 AC3: a STALE question (asker no longer resolves) still
// renders its options and the "recorded but reaches no session" note -
// the operator can still answer it, just knows upfront it will not be
// seen live.
#[test]
fn needs_overlay_lines_renders_stale_question_with_explanation() {
    let mut v = view_with_agents(vec![]);
    v.mine_fold = Some(Vec::new());
    v.needs_fold = Some(Vec::new());
    v.questions_fold = Some(vec![question_item("q-4", &["a", "b"], Some(false))]);
    let projection = v.needs_projection();
    let lines = needs_overlay_lines(&projection, 0, NeedsFooter::AsOf, NeedsFooter::AsOf);
    assert!(lines.iter().any(|l| l.contains("STALE")));
    assert!(lines
        .iter()
        .any(|l| l.contains("recorded but reaches no session")));
    assert!(lines.iter().any(|l| l.contains("1. a")));
    assert!(lines.iter().any(|l| l.contains("2. b")));
}

// ---- x-b2bf: the yard ----

#[test]
fn yard_eye_binds_the_rows_own_reading() {
    use crate::sprites::Eye;
    // Gift: an open PR outranks every mood.
    let mut g = blocked_row("g", 1, None);
    g.badge = Some(AgentBadge::Working);
    g.pr = Some(883);
    assert_eq!(yard_eye(&g, None), Eye::Gift);
    // Working, nothing owed: the default body eye.
    let mut w = blocked_row("w", 2, None);
    w.badge = Some(AgentBadge::Working);
    assert_eq!(yard_eye(&w, None), Eye::Working);
    // Attention: every human-owing need kind.
    for kind in [
        NeedKind::Question,
        NeedKind::Decision,
        NeedKind::MailQuestion,
        NeedKind::BlockedAnswerable,
        NeedKind::BlockedFocusOnly,
        NeedKind::DoneUnseen,
    ] {
        assert_eq!(yard_eye(&w, Some(kind)), Eye::Attention, "{kind:?}");
    }
    // Faded: wedged review, budget stop.
    assert_eq!(yard_eye(&w, Some(NeedKind::ReviewWedged)), Eye::Faded);
    assert_eq!(yard_eye(&w, Some(NeedKind::BudgetStop)), Eye::Faded);
    // No badge, no need: NO reading - reserved, never content.
    let mut dark = blocked_row("dark", 3, None);
    dark.badge = None;
    assert_eq!(yard_eye(&dark, None), Eye::Reserved);
    let mut gone = blocked_row("gone", 4, None);
    gone.exited = true;
    assert_eq!(yard_eye(&gone, None), Eye::Reserved);
}

#[test]
fn yard_crowd_reads_rows_and_joins_needs() {
    let mut working = blocked_row("working", 2, None);
    working.badge = Some(AgentBadge::Working);
    let mut blocked_focus = blocked_row("stuck", 3, None);
    blocked_focus.badge = Some(AgentBadge::Blocked);
    let v = view_with_agents(vec![working, blocked_focus]);
    let crowd = v.yard_crowd();
    assert_eq!(crowd[0].0, "working");
    assert_eq!(crowd[0].1, crate::sprites::Eye::Working);
    // A blocked row joins the needs queue -> attention eye.
    assert_eq!(crowd[1].1, crate::sprites::Eye::Attention);
    assert_eq!(crowd[1].2, 0); // crown defaults to 0, no hat
}

#[test]
fn yard_selection_re_anchors_by_citizen_across_a_scrape() {
    // sel indexes layout.agents order; a scrape that drops the row BEFORE
    // the cursor would slide the spotlight onto another citizen. The
    // selection must follow the citizen, not the slot.
    let a = blocked_row("aa", 2, None);
    let b = blocked_row("bb", 3, None);
    let mut v = view_with_agents(vec![a, b]);
    v.yard = Some(YardSel {
        sel: 1,
        opened_at: std::time::Instant::now(),
    });
    assert_eq!(v.yard_selected_name().as_deref(), Some("bb"));
    let mut next = v.layout.clone();
    next.agents = vec![next.agents[1].clone()]; // aa exited
    v.set_layout(next);
    assert_eq!(
        v.yard_selected_name().as_deref(),
        Some("bb"),
        "spotlight must stay on bb, not clamp onto a wrong citizen"
    );
    // Crowd empty (all rows gone): the name capture degrades to None; the
    // render's own clamp keeps the index in range.
    let mut none = v.layout.clone();
    none.agents.clear();
    v.set_layout(none);
    assert!(v.yard_selected_name().is_none());
}

fn yard_item(
    name: &str,
    species: usize,
    rarity: &str,
    crown: u32,
    first: bool,
) -> crate::yard_overlay::YardItem {
    crate::yard_overlay::YardItem {
        id: format!("{name}-id"),
        name: name.into(),
        harness: Some("claude".into()),
        species,
        rarity: rarity.into(),
        crown_level: crown,
        first_sighting: first,
    }
}

#[test]
fn yard_overlay_renders_one_spotlight_sprite() {
    let crowd = vec![
        ("a", crate::sprites::Eye::Working, 0u32),
        ("b", crate::sprites::Eye::Attention, 0),
    ];
    let id = yard_item("b", 0, "common", 0, false);
    let lines = yard_overlay_lines(&crowd, 1, Some(&id), 0, NeedsFooter::AsOf);
    // Crowd row: exactly the two eye glyphs.
    assert!(lines
        .iter()
        .any(|l| { l.trim_start() == "\u{b7}@" || l.trim_start().starts_with("\u{b7}@") }));
    // Caption carries identity outcome fields, never a rank or boundary.
    let caption = lines.iter().find(|l| l.contains('▸')).expect("caption");
    assert!(caption.contains("b") && caption.contains("cat") && caption.contains("common"));
    assert!(!caption.contains("60") && !caption.contains("boundary"));
    // The sprite: each of the cat's rendered rows WITH CONTENT appears
    // exactly once (padding trails, so match on the prefix; the sprite's
    // own blank top row is indistinguishable from padding by design),
    // and NO hat row (crown 0) - one sprite, no second block.
    for row in crate::sprites::render_frame(0, 0, crate::sprites::Eye::Attention) {
        if row.trim().is_empty() {
            continue;
        }
        assert!(
            lines
                .iter()
                .filter(|l| l.starts_with(&format!("  {row}")))
                .count()
                == 1,
            "sprite row {row:?} once"
        );
    }
    assert!(!lines.iter().any(|l| l.contains("\\^^^/")));
    assert!(lines.iter().any(|l| l.contains("2 citizens")));
}

#[test]
fn yard_overlay_crown_hat_only_when_grounded() {
    let crowd = vec![("king", crate::sprites::Eye::Working, 2u32)];
    let id = yard_item("king", 0, "rare", 0, true);
    let lines = yard_overlay_lines(&crowd, 0, Some(&id), 0, NeedsFooter::AsOf);
    assert!(lines.iter().any(|l| l.contains("\\^^^/")), "crown hat row");
    let caption = lines.iter().find(|l| l.contains('▸')).unwrap();
    assert!(caption.contains("crown 2"));
    assert!(caption.contains("NEW"));
}

#[test]
fn yard_overlay_without_identity_never_guesses_a_species() {
    let crowd = vec![("x", crate::sprites::Eye::Working, 0u32)];
    let lines = yard_overlay_lines(&crowd, 0, None, 0, NeedsFooter::Folding);
    assert!(lines.iter().any(|l| l.contains("identity fold pending")));
    // No sprite rows: nothing 12-wide renders.
    assert!(!lines
        .iter()
        .any(|l| l.trim_start().chars().count() == crate::sprites::SPRITE_W));
    assert!(lines.iter().any(|l| l.contains("folding identities")));
    let degraded = yard_overlay_lines(&crowd, 0, None, 0, NeedsFooter::Degraded);
    assert!(degraded
        .iter()
        .any(|l| l.contains("identity fold unavailable")));
    // Fold landed but this row has no registry citizen behind it (a bare
    // shell, a tombstone): say so, never "pending" forever, never a guess.
    let landed = yard_overlay_lines(&crowd, 0, None, 0, NeedsFooter::AsOf);
    assert!(landed
        .iter()
        .any(|l| l.contains("no yard identity (not a registry citizen)")));
    assert!(!landed.iter().any(|l| l.contains("pending")));
}

#[test]
fn yard_overlay_empty_yard_is_the_failure_state() {
    let lines = yard_overlay_lines(&[], 0, None, 0, NeedsFooter::AsOf);
    assert!(lines
        .iter()
        .any(|l| l.contains("the yard is empty - nothing was dispatched")));
}

#[test]
fn needs_queue_filters_to_live_blocked_rows() {
    let mut working = blocked_row("working", 2, None);
    working.badge = Some(AgentBadge::Working);
    let mut dead = blocked_row("dead", 3, None);
    dead.exited = true;
    let v = view_with_agents(vec![
        blocked_row("a", 1, None),
        working,
        dead,
        blocked_row("b", 4, None),
    ]);
    assert_eq!(
        v.needs_queue()
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>(),
        vec!["a", "b"],
        "only live blocked rows"
    );
}

// AC3-UI (x-feec): a scrape tick that drops the selected pane re-anchors the
// cursor by identity; an emptied queue keeps the overlay open in its
// "nothing needs you" state (does NOT close under the user).
#[test]
fn needs_reanchor_keeps_cursor_and_stays_open_when_empty() {
    let mut v = view_with_agents(vec![blocked_row("a", 1, None), blocked_row("b", 2, None)]);
    v.answers = Some(1);
    let with = |v: &View, agents: Vec<AgentRow>| LayoutView {
        squads: v.layout.squads.clone(),
        active_squad: 1,
        panes: v.layout.panes.clone(),
        focus: v.layout.focus,
        area: v.layout.area,
        agents,
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    };
    // "b" drops -> its identity is gone, so the cursor clamps to the new last.
    let l1 = with(&v, vec![blocked_row("a", 1, None)]);
    v.set_layout(l1);
    assert_eq!(v.answers, Some(0));
    // Queue empties -> overlay stays open (AC4-EDGE empty state), cursor at 0.
    let l2 = with(&v, vec![]);
    v.set_layout(l2);
    assert_eq!(v.answers, Some(0), "empty keeps the overlay open");
}

// INV (x-feec): NeedKind declaration order IS the severity contract.
#[test]
fn needs_kind_ord_is_declaration_order() {
    use NeedKind::*;
    let mut ks = vec![
        DoneUnseen,
        BudgetStop,
        ReviewWedged,
        BlockedFocusOnly,
        BlockedAnswerable,
        Decision,
    ];
    ks.sort();
    assert_eq!(
        ks,
        vec![
            Decision,
            BlockedAnswerable,
            BlockedFocusOnly,
            ReviewWedged,
            BudgetStop,
            DoneUnseen
        ]
    );
}

// AC1-HP (x-feec): the live badge leg + the event-fold leg merge into one
// worst-first queue: answerable, focus-only, review-wedged, budget-stopped,
// done-unseen.
#[test]
fn needs_queue_merges_and_ranks_five_kinds() {
    let mut v = view_with_agents(vec![
        blocked_row("ans", 1, Some(answerable(&[("1", "Y")], 3))),
        blocked_row("foc", 2, None),
        agent_row("dn", 3, Some(AgentBadge::Done), false),
        agent_row("rw", 4, Some(AgentBadge::Working), false),
        agent_row("bs", 5, Some(AgentBadge::Working), false),
    ]);
    v.needs_fold = Some(vec![
        fold_item("budget_stop", "bs", false),
        fold_item("review_wedged", "rw", false),
    ]);
    assert_eq!(
        v.needs_queue()
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>(),
        vec!["ans", "foc", "rw", "bs", "dn"]
    );
}

// Locked 5 (x-feec): an unjoined fold item renders only when live (squadless
// with no pane), else it is dropped (a dead session's stale stop never nags).
#[test]
fn needs_fold_drops_dead_and_renders_live_squadless() {
    let mut v = view_with_agents(vec![]);
    v.needs_fold = Some(vec![
        fold_item("budget_stop", "gone", false),
        fold_item("review_wedged", "alive", true),
    ]);
    let q = v.needs_queue();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].name, "alive");
    assert_eq!(q[0].kind, NeedKind::ReviewWedged);
    assert!(q[0].pane_id.is_none(), "squadless row has no pane");
}

// x-e3be: NeedKind::Decision is constructed by two needs.rs fold kinds and
// leads the severity order; x-f730 split `operator_question` into its own
// `NeedKind::Question` (still built here, for the roster badge), which
// sorts even ahead of Decision - the richer per-question overlay leg is
// what actually answers it (see needs_operator_queue).
#[test]
fn operator_question_folds_to_question_and_sorts_first() {
    let mut v = view_with_agents(vec![agent_row("bs", 5, Some(AgentBadge::Working), false)]);
    v.needs_fold = Some(vec![
        fold_item("budget_stop", "bs", false),
        fold_item("operator_question", "decision-row", true),
    ]);
    let q = v.needs_queue();
    assert_eq!(q.len(), 2);
    assert_eq!(q[0].kind, NeedKind::Question);
    assert_eq!(q[0].name, "decision-row");
    assert_eq!(q[1].kind, NeedKind::BudgetStop);
}

#[test]
fn carveout_stale_and_stale_claims_also_map_to_decision() {
    for kind in ["carveout_stale", "stale_claims"] {
        let mut v = view_with_agents(vec![]);
        v.needs_fold = Some(vec![fold_item(kind, "pile", true)]);
        let q = v.needs_queue();
        assert_eq!(q.len(), 1, "kind={kind}");
        assert_eq!(q[0].kind, NeedKind::Decision, "kind={kind}");
    }
}

// AC5-FR (x-feec): Enter on a joined fold row focuses its pane (goto).
#[tokio::test]
async fn needs_enter_goto_focuses_joined_fold_row() {
    let mut v = view_with_agents(vec![agent_row("bs", 5, Some(AgentBadge::Working), false)]);
    v.needs_fold = Some(vec![fold_item("budget_stop", "bs", false)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert!(
        matches!(msg, ClientMsg::Command(Command::FocusPane(5))),
        "{msg:?}"
    );
    assert_eq!(v.answers, None);
}

// Invariant (x-feec): a squadless live row has no reachable pane, so Enter
// degrades to a notice and sends nothing.
#[tokio::test]
async fn needs_enter_squadless_row_notices_no_send() {
    let mut v = view_with_agents(vec![]);
    v.needs_fold = Some(vec![fold_item("review_wedged", "alive", true)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "squadless row sends nothing");
    assert_eq!(v.answers, None);
    assert!(v.notice.is_some(), "shows a focus-manually notice");
}

// Invariant (x-feec): a digit on a non-answerable fold row is a local BEL,
// never a stray keystroke to a pane.
#[tokio::test]
async fn needs_digit_on_non_answerable_sends_nothing() {
    let mut v = view_with_agents(vec![agent_row("rw", 4, Some(AgentBadge::Working), false)]);
    v.needs_fold = Some(vec![fold_item("review_wedged", "rw", false)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"1", &mut buf).await.unwrap();
    assert!(buf.is_empty());
}

// AC6-FR (x-feec): closing bumps the generation token so a fold result that
// lands after the overlay closed is discarded by the recv guard.
#[tokio::test]
async fn needs_close_bumps_generation() {
    let mut v = view_with_agents(vec![blocked_row("a", 1, None)]);
    v.answers = Some(0);
    let g = v.needs_gen;
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"q", &mut buf).await.unwrap();
    assert_eq!(
        v.needs_gen,
        g + 1,
        "close bumps gen (in-flight fold discarded)"
    );
}

// codex P2 (x-feec): a fold row's cursor re-anchor survives a squadless ->
// joined transition, because identity is the stable session id, not the
// display name (which flips from session id to the roster row's name).
#[test]
fn needs_fold_row_id_is_stable_across_join_flip() {
    // Squadless first: no roster row, item is live -> name is the session id.
    let mut v = view_with_agents(vec![]);
    v.needs_fold = Some(vec![fold_item("budget_stop", "wkr", true)]);
    let squadless_id = v.needs_queue()[0].id();
    // Now the roster row appears: the item joins and its name flips to "wkr"
    // (already the same here, so use a distinct roster name to prove it).
    let mut joined_row = agent_row("wkr-pane", 5, Some(AgentBadge::Working), false);
    joined_row.cwd_base = Some("wkr".into()); // join by cwd_base, name differs
    v.layout.agents = vec![joined_row];
    let joined = &v.needs_queue()[0];
    assert_eq!(
        joined.name, "wkr-pane",
        "display name flips to the roster row"
    );
    assert_eq!(
        joined.id(),
        squadless_id,
        "but the re-anchor identity (session id) is unchanged"
    );
}

// AC1-HP: a digit on an answerable pane sends PaneAnswer with the exact
// daemon-pinned keystroke/fingerprint/region_lines; the overlay stays open.
#[tokio::test]
async fn xc929_answer_keys_digit_sends_pinned_paneanswer() {
    let mut v = view_with_agents(vec![blocked_row(
        "peer",
        4,
        Some(answerable(&[("1", "Yes"), ("2", "No")], 9)),
    )]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"1", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    match msg {
        ClientMsg::PaneAnswer {
            pane,
            fingerprint,
            region_lines,
            keystroke,
        } => {
            assert_eq!(pane, 4);
            assert_eq!(fingerprint, [9u8; 32]);
            assert_eq!(region_lines, 8);
            assert_eq!(keystroke, b"1");
        }
        other => panic!("expected PaneAnswer, got {other:?}"),
    }
    assert_eq!(v.answers, Some(0), "overlay stays open to cycle onward");
}

// AC1-ERR: a digit with no matching option never sends a keystroke.
#[tokio::test]
async fn xc929_answer_keys_no_matching_option_sends_nothing() {
    let mut v = view_with_agents(vec![blocked_row(
        "peer",
        4,
        Some(answerable(&[("1", "Yes"), ("2", "No")], 9)),
    )]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"7", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "no-such-option sends nothing (AC1-ERR)");
}

// AC2-HP + AC2-UI: n/N cycle the blocked queue and wrap deterministically;
// Esc closes.
#[tokio::test]
async fn xc929_answer_keys_cycle_wraps_and_esc_closes() {
    let mut v = view_with_agents(vec![blocked_row("a", 1, None), blocked_row("b", 2, None)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"n", &mut buf).await.unwrap();
    assert_eq!(v.answers, Some(1));
    answer_keys(&mut v, b"n", &mut buf).await.unwrap();
    assert_eq!(v.answers, Some(0), "n wraps to the first");
    answer_keys(&mut v, b"N", &mut buf).await.unwrap();
    assert_eq!(v.answers, Some(1), "N wraps backward to the last");
    // `q` closes instantly (a lone Esc pends until the next byte, the shared
    // fold_selector_keys behavior; `q` is the unambiguous close).
    answer_keys(&mut v, b"q", &mut buf).await.unwrap();
    assert_eq!(v.answers, None, "q closes");
    assert!(
        buf.is_empty(),
        "cycling and closing send nothing to the pane"
    );
}

// x-dcff AC (happy): Enter on a blocked row focuses that exact pane, not just
// its squad; the overlay closes.
#[tokio::test]
async fn xdcff_answer_keys_enter_focuses_the_exact_pane() {
    let mut v = view_with_agents(vec![blocked_row("peer", 4, None)]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    match msg {
        ClientMsg::Command(Command::FocusPane(pane)) => assert_eq!(pane, 4),
        other => panic!("expected FocusPane(4), got {other:?}"),
    }
    assert_eq!(v.answers, None, "Enter closes the overlay");
}

// x-dcff AC (edge): a blocked row with no pane_id sends nothing on Enter and
// still closes.
#[tokio::test]
async fn xdcff_answer_keys_enter_no_pane_id_sends_nothing() {
    let mut row = blocked_row("peer", 4, None);
    row.pane_id = None;
    let mut v = view_with_agents(vec![row]);
    v.answers = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "no pane_id -> nothing sent (AC1-EDGE)");
    assert_eq!(v.answers, None, "Enter still closes the overlay");
}

#[tokio::test]
async fn rename_keys_enter_sends_the_typed_name_for_the_captured_tab() {
    // AC2-HP (client half): type + Enter -> one RenameTab for the tab id
    // captured at open time.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"debug\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::RenameTab {
            tab: 7,
            name: "debug".into()
        })
    );
    assert_eq!(v.rename, None, "submit closes the overlay");
}

#[tokio::test]
async fn move_to_keys_enter_sends_one_reorder_with_the_computed_delta() {
    // (x-cf97) The typed ordinal is 1-based, the delta is computed against
    // the tab's current index, and ONE ReorderTab carries it.
    let mut v = two_pane_view();
    v.open_move_to(1); // squad 1's second tab: visible ordinal 2
    let mut buf: Vec<u8> = Vec::new();
    move_to_keys(&mut v, b"1\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::ReorderTab {
            squad: 1,
            tab: 1,
            delta: -1
        })
    );
    assert_eq!(v.move_to, None, "submit closes the prompt");
}

#[tokio::test]
async fn move_to_keys_out_of_range_keeps_the_prompt_open_with_a_notice() {
    // The prompt never sends a clamped guess: ordinal 3 in a two-tab
    // squad re-opens with the typed text intact and says so.
    let mut v = two_pane_view();
    v.open_move_to(1);
    let mut buf: Vec<u8> = Vec::new();
    move_to_keys(&mut v, b"3\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "nothing is sent");
    let (tab, text) = v.move_to.expect("the prompt stays open");
    assert_eq!((tab, text.as_str()), (1, "3"));
    assert!(v.notice.is_some(), "the refusal names the range");
}

#[tokio::test]
async fn move_to_keys_esc_cancels_and_swallows_the_tail() {
    let mut v = two_pane_view();
    v.open_move_to(0);
    let mut buf: Vec<u8> = Vec::new();
    move_to_keys(&mut v, b"2\x1b9", &mut buf).await.unwrap();
    assert_eq!(v.move_to, None, "Esc cancels the prompt");
    assert!(buf.is_empty(), "no tail byte reaches the pane");
}

#[tokio::test]
async fn jump_to_a_number_that_names_no_tab_sets_a_notice_and_sends_nothing() {
    // (x-cf97) A digit that names no tab answers with a notice naming the
    // miss - a silent BEL reads as a dead keybind - and never a wire
    // message the server would refuse. The event now carries the 1-based
    // ordinal the operator reads off the strip, so 1 is the FIRST tab,
    // and 0 (an unparseable hold) is refused like any other miss.
    let mut v = two_pane_view();
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::SelectTabIdx(99), &mut buf)
        .await
        .unwrap();
    assert!(buf.is_empty(), "nothing is sent for a missing tab");
    let (text, _) = v.notice.expect("the refusal names the miss");
    assert!(text.contains("no tab 99"), "{text}");
    assert!(
        text.contains("2 open"),
        "the notice names the count: {text}"
    );

    // Ordinal 1 is the FIRST tab (1-based), not the second.
    let mut v = two_pane_view();
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::SelectTabIdx(1), &mut buf)
        .await
        .unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(msg, ClientMsg::Command(Command::SelectTab(0)));
}

#[tokio::test]
async fn xf331_hover_armed_x_acts_on_the_row_not_the_pane() {
    // x-f331 AC1-HP: with the pointer over a squad header, x opens the
    // close-workspace confirm on THAT row and no `x` leaks to the focused
    // pane's PTY (the old bare-key leak this node closes).
    let mut v = two_pane_view();
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    v.on_hover(0, 5, Instant::now()); // hover-arm the footnote squad header
    assert_eq!(v.selector, Some(0));
    assert!(v.sel_hover_armed);
    handle_stdin(&mut v, &mut scanner, &mut carry, b"x", &mut buf)
        .await
        .unwrap();
    assert!(
        matches!(
            v.confirm.as_ref().map(|c| &c.action),
            Some(ConfirmKind::RemoveSquad { .. })
        ),
        "x on the hovered squad header opens the close-workspace confirm"
    );
    assert!(buf.is_empty(), "no x reaches the focused pane's PTY");
    let anchor = v.confirm_anchor_row(v.term.0 as usize, v.confirm.as_ref().unwrap());
    assert_eq!(anchor, 0, "the confirm anchors at the hovered squad's row");
}

#[tokio::test]
async fn xf331_hover_armed_non_verb_key_disarms_and_forwards() {
    // x-f331 AC2-EDGE: a pointer parked over the sideline hover-arms the
    // selector, but the first NON-verb key disarms the arm and forwards to the
    // focused pane - typing into the shell is never swallowed.
    let mut v = two_pane_view();
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    v.on_hover(0, 5, Instant::now()); // hover-arm the header row
    assert_eq!(v.selector, Some(0));
    assert!(v.sel_hover_armed);
    handle_stdin(&mut v, &mut scanner, &mut carry, b"l", &mut buf)
        .await
        .unwrap();
    assert_eq!(v.selector, None, "a non-verb key disarms the hover-arm");
    assert!(!v.sel_hover_armed);
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Input(bytes) => {
            assert_eq!(bytes, b"l", "the key forwards to the focused pane")
        }
        other => panic!("a non-verb key should forward to the pane, got {other:?}"),
    }
}

#[tokio::test]
async fn xf331_selector_r_on_a_non_squad_row_notices_not_beeps() {
    // x-f331 US3/AC1-ERR: a refused sideline action prints a notice naming
    // why, never a bare beep. `r` (rename workspace) on an agent row refuses.
    let mut v = unified_rows_view();
    let mut buf: Vec<u8> = Vec::new();
    let idx = agent_row_at(&v, |a| a.pane_id == Some(10));
    v.selector = Some(idx);
    selector_keys(&mut v, b"r", &mut buf).await.unwrap();
    assert!(v.rename.is_none(), "r on an agent row opens no rename");
    assert!(
        v.notice.is_some(),
        "the refusal is a visible notice, not a bare beep"
    );
}

#[tokio::test]
async fn prefix_reorder_sends_the_active_tab_id_and_delta() {
    let mut v = two_pane_view();
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();

    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x02>leak", &mut buf)
        .await
        .unwrap();

    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::ReorderTab {
            squad: 1,
            tab: 1,
            delta: 1
        })
    );
    assert_eq!(
        cur.position() as usize,
        cur.get_ref().len(),
        "same-chunk bytes after the chord are swallowed"
    );
}

#[tokio::test]
async fn keys_modal_executed_resize_arms_the_repeat_window() {
    // codex P2 parity: a resize run from the which-key modal arms the repeat
    // window, so a following bare H repeats without prefix - exactly as a
    // typed prefix+H would (the scanner never saw the modal keystroke).
    let mut v = two_pane_view();
    v.term = (40, 80);
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    v.open_keys_modal();
    keys_modal_keys(&mut v, &mut scanner, b"H", &mut buf)
        .await
        .unwrap();
    buf.clear();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"H", &mut buf)
        .await
        .unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::ResizeDir(crate::tree::Dir::Left)) => {}
        other => panic!("bare H after a modal resize should repeat-resize, got {other:?}"),
    }
}

#[tokio::test]
async fn keys_modal_executed_pane_ids_arms_the_repeat_window() {
    let mut v = two_pane_view();
    v.term = (40, 80);
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    v.open_keys_modal();
    keys_modal_keys(&mut v, &mut scanner, b"\\", &mut buf)
        .await
        .unwrap();
    buf.clear();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\\", &mut buf)
        .await
        .unwrap();
    assert!(
        buf.is_empty(),
        "a modal pane-id repeat must stay client-local"
    );
    assert!(v.pane_ids_until.is_some(), "the repeat reopened the reveal");
}

#[tokio::test]
async fn mouse_scroll_disarms_the_resize_repeat_window() {
    // codex P2: a scroll (like a click) is "other input" - it disarms the
    // window in the mouse pre-pass, before the report is stripped and the
    // scanner runs, so a following bare H forwards to the pane instead of
    // silently resizing. (A wheel, unlike a chrome click, opens no overlay
    // that would swallow the next key, so it isolates the disarm wiring.)
    let mut v = two_pane_view();
    v.term = (40, 80);
    let mut scanner = Scanner::default();
    let mut carry = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    // Arm via a typed prefix+H.
    handle_stdin(&mut v, &mut scanner, &mut carry, b"\x02H", &mut buf)
        .await
        .unwrap();
    // A wheel-down SGR report (button 65) disarms.
    buf.clear();
    handle_stdin(
        &mut v,
        &mut scanner,
        &mut carry,
        b"\x1b[<65;10;5M",
        &mut buf,
    )
    .await
    .unwrap();
    // A bare H now forwards to the pane, not a resize.
    buf.clear();
    handle_stdin(&mut v, &mut scanner, &mut carry, b"H", &mut buf)
        .await
        .unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Input(bytes) => {
            assert_eq!(
                bytes, b"H",
                "bare H forwards after a scroll disarms the window"
            )
        }
        other => panic!("bare H after a scroll should forward, not resize, got {other:?}"),
    }
}

#[tokio::test]
async fn rename_keys_empty_enter_still_sends_the_clear() {
    // Locked 2 / AC3-HP: Enter on an EMPTY buffer sends (blank = reset to
    // auto) - the one deliberate divergence from create_keys.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::RenameTab {
            tab: 7,
            name: String::new()
        })
    );
    assert_eq!(v.rename, None);
}

#[tokio::test]
async fn rename_keys_esc_cancels_without_sending_and_swallows_the_tail() {
    // AC1-UI: Esc closes, sends nothing; same-chunk bytes after the Esc
    // die with the overlay instead of leaking into the pane.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"deb\x1bx", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "cancel sends no command");
    assert_eq!(v.rename, None);
}

#[tokio::test]
async fn rename_keys_caps_the_buffer_at_max_tab_name() {
    // The TUI affordance half of AC2-ERR: the operator sees exactly what
    // the server will store (the server cap stays authoritative).
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    let long = "a".repeat(MAX_TAB_NAME + 8);
    rename_keys(&mut v, long.as_bytes(), &mut buf)
        .await
        .unwrap();
    assert_eq!(v.rename.as_ref().unwrap().1.len(), MAX_TAB_NAME);
}

// ---- x-96e8: squad-management selector context keys ----

#[tokio::test]
async fn selector_r_opens_squad_rename_overlay() {
    // AC1-HP (client half): `r` on a squad row opens the rename overlay for
    // that squad, closing the selector, without sending anything.
    let mut v = two_pane_view(); // rows: [squad1, squad2, +footer]
    v.selector = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "opening the overlay sends nothing");
    assert_eq!(v.selector, None, "the selector closes");
    assert_eq!(v.rename.map(|(t, _)| t), Some(RenameTarget::Squad(1)));
}

#[tokio::test]
async fn rename_keys_squad_target_sends_rename_squad() {
    // AC1-HP: Enter on a squad rename sends RenameSquad for the captured id.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Squad(2));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"oss\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        decoded,
        ClientMsg::Command(Command::RenameSquad {
            squad: 2,
            name: "oss".into()
        })
    );
}

#[tokio::test]
async fn selector_j_sends_move_squad_and_tracks_the_squad() {
    // AC3-HP (client half): `J` reorders the squad down and arms sel_follow
    // so the next Layout re-points the cursor at the moved workspace; the
    // selector stays open for repeated presses.
    let mut v = two_pane_view();
    v.selector = Some(0); // squad 1
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"J", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        decoded,
        ClientMsg::Command(Command::MoveSquad { squad: 1, delta: 1 })
    );
    assert_eq!(v.sel_follow, Some(1), "the cursor tracks the moved squad");
    assert_eq!(v.selector, Some(0), "the selector stays open");
}

#[test]
fn set_layout_follows_the_reordered_squad() {
    // AC3-HP: after a J/K reorder, the next Layout re-points the cursor onto
    // the moved squad's new row rather than clamping the old index.
    let mut v = two_pane_view(); // rows: [squad1@0, Blank@1, squad2@2, footer]
    v.selector = Some(0);
    v.sel_follow = Some(1); // tracking squad 1
                            // The reorder landed: squad 1 is now second, so its row moves to index 2
                            // (a x-cd67 US3 Blank spacer sits at index 1 between the groups).
    v.set_layout(LayoutView {
        squads: vec![meta(2, "notes", 1, 0), meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![],
        focus: 11,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    assert_eq!(v.selector, Some(2), "cursor follows squad 1 to its new row");
}

#[tokio::test]
async fn selector_x_arms_remove_confirm_and_degrades_on_short_terminal() {
    // AC2-UI: `x` on a squad row arms the remove confirm carrying the blast
    // radius; a too-short terminal refuses instead of arming an invisible
    // confirm (x-260a row_action rule).
    let mut v = two_pane_view(); // squad1 (footnote) has 2 panes; 2 squads
    v.selector = Some(0);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"x", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert_eq!(v.selector, None);
    match v.confirm.as_ref().map(|c| (&c.action, c.label.as_str())) {
        Some((ConfirmKind::RemoveSquad { squad, panes, last }, label)) => {
            assert_eq!((*squad, *panes, *last), (1, 2, false));
            assert_eq!(label, "footnote");
        }
        _ => panic!("expected a RemoveSquad confirm"),
    }

    // Too short: refuse with a notice, arm nothing.
    let mut v = two_pane_view();
    v.term.0 = MIN_ROWS_FOR_STATUS - 1;
    v.selector = Some(0);
    selector_keys(&mut v, b"x", &mut Vec::new()).await.unwrap();
    assert!(
        v.confirm.is_none(),
        "no invisible confirm on a short terminal"
    );
    assert!(v.notice.is_some(), "the refusal is surfaced");
}

#[tokio::test]
async fn confirm_keys_enter_sends_remove_squad() {
    // AC2-HP: Enter on an armed remove confirm sends RemoveSquad.
    let mut v = two_pane_view();
    v.confirm = Some(ConfirmAction {
        action: ConfirmKind::RemoveSquad {
            squad: 2,
            panes: 1,
            last: false,
        },
        label: "notes".into(),
    });
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(decoded, ClientMsg::Command(Command::RemoveSquad(2)));
}

#[tokio::test]
async fn selector_m_opens_move_picker_on_a_squad_row() {
    // x-0090: tab rows left the sideline, so `m` on a SQUAD row opens the
    // picker over the OTHER squads, targeting that squad's ACTIVE tab (the
    // one shown in the tab bar). A squad itself still moves with J/K, not m.
    let mut v = two_pane_view(); // rows: [squad1@0, squad2@1, footer@2]
    v.selector = Some(0); // squad 1 (fixture active_tab 1 -> tab id 1)
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"m", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "opening the picker sends nothing");
    assert_eq!(
        v.move_pick,
        Some(MovePick::new(MoveSrc::Tab(1), vec![2])),
        "picker captures the squad's active tab id and the non-source squads"
    );

    // With only one squad there is nowhere to move to: `m` BELs, no picker.
    let mut v = two_pane_view();
    v.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![],
        focus: 0,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    v.selector = Some(0);
    selector_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
    assert!(v.move_pick.is_none(), "no destination squad -> no picker");
}

#[tokio::test]
async fn selector_m_picker_excludes_mission_squads() {
    // A mission id resolves to no server-side squad, so MoveTab into one is
    // refused; the tab-move destination list must exclude mission sentinels
    // (the other destination sites already do).
    let mut v = two_pane_view(); // squads 1 (footnote) + 2 (notes)
    v.layout.squads.push(mission_meta(5, "epic  0/4"));
    v.selector = Some(0); // squad 1
    selector_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
    let dsts = v
        .move_pick
        .expect("picker opens with squad 2 available")
        .squads;
    assert!(
        !dsts.iter().any(|&id| is_mission_squad(id)),
        "no mission sentinel in the destinations"
    );
    assert!(dsts.contains(&2), "the other real workspace is listed");
}

#[tokio::test]
async fn move_pick_keys_digit_sends_move_tab_and_stale_id_bels() {
    // A digit sends MoveTab for the numbered squad; a captured id that
    // vanished is refused locally (no wire message).
    let mut v = two_pane_view();
    v.move_pick = Some(MovePick::new(MoveSrc::Tab(7), vec![2])); // move tab 7 to squad 2 (digit 1)
    let mut buf: Vec<u8> = Vec::new();
    move_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        decoded,
        ClientMsg::Command(Command::MoveTab { tab: 7, squad: 2 })
    );
    assert_eq!(v.move_pick, None, "the picker is single-shot");

    // A stale captured id (not in the current catalog) sends nothing.
    let mut v = two_pane_view();
    v.move_pick = Some(MovePick::new(MoveSrc::Tab(7), vec![999]));
    let mut buf: Vec<u8> = Vec::new();
    move_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
    assert!(
        buf.is_empty(),
        "a stale destination id never reaches the wire"
    );
    assert_eq!(v.move_pick, None);
}

#[test]
fn row_menu_pane_row_offers_move_to_workspace_with_another_squad() {
    // A pane-hosted row gets a Move-to-workspace entry when another real
    // workspace exists - the cross-workspace affordance it lacked. Built in
    // open_row_menu (layout-aware), so this goes through open_row_menu, not
    // build_row_menu directly.
    let mut v = unified_rows_view(); // squads 1 (footnote) + 2 (notes)
    let idx = agent_row_at(&v, |a| a.name == "worker" && a.pane_id.is_some());
    assert!(v.open_row_menu(idx, Anchor::Center));
    assert!(
        v.row_menu
            .as_ref()
            .unwrap()
            .actions
            .contains(&MenuAction::MoveToWorkspace),
        "pane row with a second workspace offers Move to workspace"
    );
}

#[test]
fn row_menu_pane_row_omits_move_to_workspace_with_one_squad() {
    // No destination -> no entry. A dead Move-to-workspace item that bels on
    // open would be worse than none.
    let mut v = view_with_agents(vec![]);
    v.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(
            10,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 35,
            },
        )],
        focus: 10,
        area: (29, 72),
        agents: vec![pane_hosted_row("only", 10)],
        focus_node: None,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
    });
    let idx = agent_row_at(&v, |a| a.name == "only");
    assert!(v.open_row_menu(idx, Anchor::Center));
    assert!(
        !v.row_menu
            .as_ref()
            .unwrap()
            .actions
            .contains(&MenuAction::MoveToWorkspace),
        "a lone workspace offers no move"
    );
}

#[tokio::test]
async fn move_pick_keys_pane_sends_cross_squad_move_pane() {
    // A pane source relocates the live pane into the chosen workspace beside
    // its active-tab focus pane (the MovePane anchor), the cross-squad path
    // the row drag already uses. With no anchor pane in the destination,
    // nothing goes on the wire and the operator is told.
    let mut v = two_pane_view();
    // Give squad 2's active tab a leaf pane to anchor against (200).
    v.layout.squads[1].tabs[0].panes.push(PaneMeta {
        id: 200,
        label: "dst".into(),
    });
    v.move_pick = Some(MovePick::new(MoveSrc::Pane(10), vec![2])); // move pane 10 to squad 2
    let mut buf: Vec<u8> = Vec::new();
    move_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![Command::MovePane {
            mover: Some(10),
            target: Some(200),
            dir: Dir::Right,
        }],
        "pane move targets the destination's anchor pane"
    );
    assert_eq!(v.move_pick, None);

    // Destination with no leaf pane: anchor is None -> a notice, no command.
    let mut v = two_pane_view(); // squad 2's tabs carry no PaneMeta here
    v.move_pick = Some(MovePick::new(MoveSrc::Pane(10), vec![2]));
    let mut buf: Vec<u8> = Vec::new();
    move_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "no anchor -> nothing on the wire");
    assert!(v.notice.is_some(), "and the operator is told why");
}

#[tokio::test]
async fn selector_non_reorder_key_clears_sel_follow() {
    // sel_follow only survives across J/K; any other key drops it so a later
    // Layout re-anchors normally.
    let mut v = two_pane_view();
    v.selector = Some(0);
    v.sel_follow = Some(1);
    selector_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
    assert_eq!(v.sel_follow, None);
}

// -- pane relocation (x-aa95) -------------------------------------------

/// The first cell of the seam flanked by exactly `a` and `b`, found the way
/// the dispatch does rather than hardcoded, so a layout tweak cannot
/// silently point these tests at a cell that is no longer that divider.
fn seam_cell_between(view: &View, a: u64, b: u64) -> (u16, u16) {
    let (rows, cols) = view.term;
    (TAB_BAR_ROWS..rows)
        .flat_map(|r| (0..cols).map(move |c| (r, c)))
        .find(|(r, c)| view.seam_at(*r, *c).is_some_and(|s| s.a == a && s.b == b))
        .unwrap_or_else(|| panic!("no seam between {a} and {b}"))
}

#[test]
fn grip_is_hidden_on_a_single_pane_tab() {
    // Locked Decision 5: nowhere to relocate to, so no handle is offered.
    let mut view = two_pane_view();
    view.layout.panes.truncate(1);
    let rect = view.layout.panes[0].1;
    let (row, cols) = view.grip_span(rect).expect("a wide pane has room");
    assert_eq!(
        view.grip_at(row, cols.start),
        None,
        "a lone pane must not offer a grip"
    );
}

#[test]
fn grip_hit_test_matches_where_the_grip_draws() {
    // The renderer and the hit test share grip_span precisely so a press
    // cannot miss a handle the operator can see.
    let view = two_pane_view();
    let rect = view.pane_rect(10).expect("pane 10 exists");
    let (row, gcols) = view.grip_span(rect).expect("pane 10 has room");
    let frame = view.compose();
    let cols = view.term.1 as usize;
    for (i, ch) in GRIP.chars().enumerate() {
        let cell = frame.cells[row as usize * cols + gcols.start as usize + i];
        assert_eq!(cell.c, ch, "the grip draws at its own span");
    }
    for c in gcols.clone() {
        assert_eq!(
            view.grip_at(row, c),
            Some(10),
            "every grip cell is pressable"
        );
    }
    assert_eq!(view.grip_at(row, gcols.end), None, "and only those cells");
}

#[test]
fn a_seam_zone_targets_the_pane_it_sits_after() {
    let view = three_pane_view();
    let (r, c) = seam_cell_between(&view, 11, 12);
    let seam = view.seam_at(r, c).expect("cell chosen for being a seam");
    assert_eq!(
        view.drop_zone_at(r, c),
        Some(DropZone {
            target: seam.a,
            dir: Dir::Right
        }),
        "landing between a side-by-side pair is landing right of the left one"
    );
}

#[test]
fn a_drag_lights_a_candidate_and_keeps_the_origin_marked() {
    // AC3-UI: the zone under the pointer accents, and the pane being moved
    // stays visibly marked even though the pointer has left it.
    let mut view = three_pane_view();
    // Pane 10 dragged to the 11|12 seam: a divider it does not already
    // abut, so this is a real destination rather than an origin drop.
    view.begin_pane_drag(10, Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    assert!(
        view.pane_drag_to(r, c, Instant::now()),
        "entering a zone redraws"
    );
    assert!(view.pane_drag.and_then(|d| d.zone).is_some());
    assert!(
        !view.pane_drag_to(r, c, Instant::now()),
        "staying inside the same zone costs no redraw"
    );

    let frame = view.compose();
    let cols = view.term.1 as usize;
    let lit = frame.cells[r as usize * cols + c as usize];
    assert_eq!(
        lit.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the candidate zone reads as the drop target"
    );
    let rect = view.pane_rect(10).expect("the origin still exists");
    let (grow, gcols) = view.grip_span(rect).expect("pane 10 has room");
    assert_eq!(
        frame.cells[grow as usize * cols + gcols.start as usize].flags & cell_flags::BOLD,
        cell_flags::BOLD,
        "the origin grip stays marked while the pointer is elsewhere"
    );
}

#[test]
fn a_drop_commits_the_candidate_zone() {
    let mut view = three_pane_view();
    view.begin_pane_drag(10, Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.pane_drag_to(r, c, Instant::now());
    assert_eq!(
        view.commit_pane_drag(),
        Some(Command::MovePane {
            mover: Some(10),
            target: Some(11),
            dir: Dir::Right
        })
    );
    assert!(view.pane_drag.is_none(), "committing ends the drag");
}

// ---- (v43, x-d6a8) US9 interactive drag commit functions ---------------

#[test]
fn g1_pane_drag_onto_the_strip_commits_a_break() {
    // AC1-HP (commit half): a pane dragged onto the tab strip breaks into its
    // own tab (BreakPane), not a within-tab MovePane. The strip cell also
    // clears the content zone: a cell is strip XOR content.
    let mut view = three_pane_view();
    view.begin_pane_drag(10, Instant::now());
    // First over a real content seam: a MovePane candidate.
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.pane_drag_to(r, c, Instant::now());
    assert!(view.pane_drag.and_then(|d| d.zone).is_some());
    // Then up onto the strip (row 0, right of the sideline panel).
    let changed = view.pane_drag_to(0, view.panel_w(), Instant::now());
    assert!(changed, "moving onto the strip redraws");
    let d = view.pane_drag.expect("still dragging");
    assert!(d.on_strip, "the pointer is over the strip");
    assert!(d.zone.is_none(), "the content zone is cleared on the strip");
    assert_eq!(
        view.commit_pane_drag(),
        Some(Command::BreakPane { pane: 10 }),
        "a strip drop breaks the pane"
    );
    assert!(view.pane_drag.is_none(), "committing ends the drag");
}

#[test]
fn g1_pane_drag_off_the_strip_still_moves_within_the_tab() {
    // The strip branch must not perturb the ordinary drag: leaving the strip
    // for a content seam commits MovePane exactly as before.
    let mut view = three_pane_view();
    view.begin_pane_drag(10, Instant::now());
    view.pane_drag_to(0, view.panel_w(), Instant::now()); // over strip
    let (r, c) = seam_cell_between(&view, 11, 12); // back to content
    view.pane_drag_to(r, c, Instant::now());
    assert!(!view.pane_drag.unwrap().on_strip, "left the strip");
    assert_eq!(
        view.commit_pane_drag(),
        Some(Command::MovePane {
            mover: Some(10),
            target: Some(11),
            dir: Dir::Right
        })
    );
}

#[test]
fn g2_tab_drag_onto_a_content_edge_commits_a_join() {
    // AC2-HP: dragging a NON-current tab (id 0; the current tab is 1) onto a
    // content edge of the current tab joins it there.
    let mut view = three_pane_view();
    view.begin_tab_drag(0, Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    assert!(
        view.tab_drag_to(r, c, Instant::now()),
        "entering a zone redraws"
    );
    assert!(view.tab_drag.and_then(|d| d.zone).is_some());
    assert_eq!(
        view.commit_tab_drag(),
        Some(Command::JoinTab {
            src_tab: 0,
            anchor_pane: 11,
            dir: Dir::Right
        })
    );
    assert!(view.tab_drag.is_none(), "committing ends the drag");
}

#[test]
fn g2_tab_drag_of_the_current_tab_is_a_suppressed_self_join() {
    // AC2-EDGE: the current tab (id 1) dropped on its own content lights
    // nothing and commits nothing - the client suppresses the no-op (the
    // server would also refuse BAD_REQUEST).
    let mut view = three_pane_view();
    view.begin_tab_drag(1, Instant::now()); // 1 IS the current tab
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.tab_drag_to(r, c, Instant::now());
    assert!(
        view.tab_drag.and_then(|d| d.zone).is_none(),
        "a self-join lights no zone"
    );
    assert_eq!(view.commit_tab_drag(), None, "and commits nothing");
}

#[test]
fn g2_tab_drag_off_zone_or_cancelled_sends_nothing() {
    // AC1-EDGE / AC1-FR: an off-zone release, and a cancelled drag (the
    // timeout/esc path clears the struct internally), both send nothing.
    let mut view = three_pane_view();
    view.begin_tab_drag(0, Instant::now());
    // Released with no zone ever set.
    assert_eq!(view.commit_tab_drag(), None, "off-zone -> nothing");
    // Cancel clears the struct internally, so a late commit finds nothing.
    view.begin_tab_drag(0, Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.tab_drag_to(r, c, Instant::now());
    assert!(view.cancel_tab_drag(), "a live drag cancels");
    assert!(
        view.tab_drag.is_none(),
        "the struct is cleared, not just the zone"
    );
    assert_eq!(
        view.commit_tab_drag(),
        None,
        "a late release commits nothing"
    );
}

#[test]
fn g3_pane_hosted_row_drop_moves_the_off_layout_pane_cross_tab() {
    // AC3-HP (commit half): a pane-hosted row names a pane (99) that is NOT
    // in the current layout - off-layout by design (Domain Pitfall: the row
    // drop must NOT gate on pane_rect(mover)). It still commits a MovePane.
    let mut view = three_pane_view();
    assert!(
        view.pane_rect(99).is_none(),
        "precondition: 99 is off-layout"
    );
    view.begin_row_drag(RowSource::Pane(99), Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    assert!(
        view.row_drag_to(r, c, Instant::now()),
        "entering a zone redraws"
    );
    assert_eq!(
        view.commit_row_drag(),
        Some(Command::MovePane {
            mover: Some(99),
            target: Some(11),
            dir: Dir::Right
        })
    );
}

#[test]
fn row_drag_of_an_onscreen_pane_suppresses_an_origin_zone() {
    // The server discards an origin move in SILENCE (it reads the drop as a
    // deliberate cancel), so a zone that lights up and then moves nothing is
    // the defect. `pane_drag_to` already suppresses both origin shapes; the
    // row drag of an on-layout pane must too, or the same gesture means two
    // things depending on which grip you grabbed.
    let mut view = three_pane_view();
    assert!(
        view.pane_rect(11).is_some(),
        "precondition: 11 is ON this layout"
    );
    view.begin_row_drag(RowSource::Pane(11), Instant::now());
    // The seam between 11 and 12 is one 11 already abuts: dropping there
    // rebuilds the identical tree, which move_leaf reports as Origin.
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.row_drag_to(r, c, Instant::now());
    assert_eq!(
        view.commit_row_drag(),
        None,
        "an origin drop commits nothing rather than a command the server eats"
    );

    // The off-layout case is unchanged: that mover is in another tree, so it
    // can never be an origin and must keep every zone.
    let mut view = three_pane_view();
    view.begin_row_drag(RowSource::Pane(99), Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    assert!(view.row_drag_to(r, c, Instant::now()));
    assert!(
        view.commit_row_drag().is_some(),
        "an off-layout row still commits a cross-tab move"
    );
}

#[test]
fn g3_paneless_row_drop_attaches_at_the_slot() {
    // AC3 (paneless half): a paneless bg row attaches at the drop slot with a
    // placement anchored to the dropped-on pane; `tab` is left unset (the
    // server resolves the tab from the anchor's live location).
    let mut view = three_pane_view();
    view.begin_row_drag(RowSource::Attach("c19cd2c3".into()), Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.row_drag_to(r, c, Instant::now());
    assert_eq!(
        view.commit_row_drag(),
        Some(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                at: Some(11),
                split: Some(Dir::Right),
                here: false,
                ..PanePlacement::default()
            },
        })
    );
}

#[test]
fn g3_row_drop_off_zone_or_cancelled_sends_nothing() {
    // AC1-EDGE / AC1-FR for the row drag: off-zone and cancelled both quiet.
    let mut view = three_pane_view();
    view.begin_row_drag(RowSource::Pane(99), Instant::now());
    assert_eq!(view.commit_row_drag(), None, "off-zone -> nothing");
    view.begin_row_drag(RowSource::Attach("dead".into()), Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.row_drag_to(r, c, Instant::now());
    assert!(view.cancel_row_drag(), "a live drag cancels");
    assert!(view.row_drag.is_none(), "the struct is cleared internally");
    assert_eq!(
        view.commit_row_drag(),
        None,
        "a late release commits nothing"
    );
}

#[test]
fn tab_cell_at_resolves_a_strip_cell_to_its_tab() {
    // The drag-source hit-test for G2: a strip cell names its tab id, a
    // content cell and the sideline column do not, and neither does the `+`.
    let view = three_pane_view(); // squad 1, tabs [0,1], active 1
    let pw = view.panel_w();
    // The strip leads with a squad-name span (not a drag source), so scan for
    // the first cell that names a tab; it must be tab 0.
    let first_tab = (pw..view.term.1).find_map(|c| view.tab_cell_at(0, c));
    assert_eq!(first_tab, Some(0), "the first tab span names tab 0");
    assert_eq!(
        view.tab_cell_at(5, pw),
        None,
        "a content-row cell is not a tab"
    );
    if pw > 0 {
        assert_eq!(
            view.tab_cell_at(0, 0),
            None,
            "the sideline column is not the strip"
        );
    }
}

#[test]
fn row_drag_source_at_resolves_pane_hosted_and_paneless_rows() {
    // The drag-source hit-test for G3, reusing the exact fixture + row
    // coordinates of chrome_hit_agent_rows_focus_or_hint: worker (pane 10) at
    // row 1, bg-claude (attach) at row 8, bg-other (neither) at row 9.
    let hosted = focus_agent(10);
    let mut bg_attach = focus_agent(0);
    bg_attach.squad = None;
    bg_attach.name = "bg-claude".into();
    bg_attach.pane_id = None;
    bg_attach.attach_id = Some("c19cd2c3".into());
    let mut bg_plain = focus_agent(0);
    bg_plain.squad = None;
    bg_plain.name = "bg-other".into();
    bg_plain.pane_id = None;
    bg_plain.attach_id = None;
    let mut view = view_with_agents(vec![hosted, bg_attach, bg_plain]);
    view.expand_pull_sections(); // (x-c5ee) ~ elsewhere now defaults Collapsed
    assert_eq!(
        view.row_drag_source_at(1, 4),
        Some(RowSource::Pane(10)),
        "a pane-hosted row drags its pane"
    );
    assert_eq!(
        view.row_drag_source_at(8, 4),
        Some(RowSource::Attach("c19cd2c3".into())),
        "a paneless bg row drags its attach id"
    );
    assert_eq!(
        view.row_drag_source_at(9, 4),
        None,
        "a row with neither pane nor attach is not a drag source"
    );
}

#[test]
fn row_drag_source_at_skips_the_density_button_over_an_agent_row() {
    // (x-d6a8, codex P2) The row-0 density button overlays a scrolled agent
    // row; a press on the button must cycle density (chrome_hit), not start a
    // row drag on the agent underneath.
    let mut view = view_with_agents(vec![focus_agent(10)]);
    view.sideline_offset = 1; // scroll so an agent row paints at row 0
    let pw = view.panel_w() as usize;
    let Some(range) = view.density_button_range(pw) else {
        return; // panel too narrow for the button; the guard is moot
    };
    let btn = range.start as u16;
    // Precondition: a real agent row sits under the button cell.
    let i = view
        .sideline_row_at(0, btn)
        .expect("the button col is a sideline row");
    assert!(
        matches!(view.display_rows().get(i), Some(DisplayRow::Agent(_))),
        "an agent row is under the density button"
    );
    // Yet the button cell is NOT a drag source - it cycles density instead.
    assert_eq!(
        view.row_drag_source_at(0, btn),
        None,
        "the density button is not a drag source"
    );
}

#[test]
fn g1_strip_lights_inverse_while_a_pane_is_dragged_over_it() {
    // AC1-UI (headless slice): a pane dragged onto the strip lights the strip
    // INVERSE - the "drop here to break" affordance. The visual is otherwise a
    // manual-checklist item, but that a strip cell changes is assertable.
    let mut view = three_pane_view();
    view.begin_pane_drag(10, Instant::now());
    view.pane_drag_to(0, view.panel_w(), Instant::now()); // over the strip
    let frame = view.compose();
    let cols = view.term.1 as usize;
    let cell = frame.cells[view.panel_w() as usize]; // row 0, first strip cell
    assert_eq!(
        cell.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the strip reads as an insert target while a pane hovers it"
    );
    let _ = cols;
}

#[test]
fn g2_tab_drag_lights_the_destination_zone() {
    // AC1-UI (headless slice): a tab-cell drag reuses the content-edge zone
    // highlight, so the destination seam lights INVERSE just like a pane drag.
    let mut view = three_pane_view();
    view.begin_tab_drag(0, Instant::now()); // a non-current tab
    let (r, c) = seam_cell_between(&view, 11, 12);
    assert!(
        view.tab_drag_to(r, c, Instant::now()),
        "entering a zone redraws"
    );
    let frame = view.compose();
    let cols = view.term.1 as usize;
    let lit = frame.cells[r as usize * cols + c as usize];
    assert_eq!(
        lit.flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the tab-drag destination zone lights the same as a pane drag"
    );
}

#[test]
fn a_release_resolves_against_the_layout_it_lands_on() {
    // The cached zone can go stale: a co-viewer moves the targeted seam
    // between the last motion and the release, and set_layout only clears
    // the cache when the target DISAPPEARS, not when it merely moves. So
    // the release re-hit-tests its own coordinates.
    let mut view = three_pane_view();
    view.begin_pane_drag(10, Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.pane_drag_to(r, c, Instant::now());
    assert_eq!(
        view.pane_drag.and_then(|d| d.zone),
        Some(DropZone {
            target: 11,
            dir: Dir::Right
        })
    );

    // A co-viewer widens 11, sliding the 11|12 seam right so this cell now
    // sits INSIDE pane 11 rather than on any seam.
    let mut next = view.layout.clone();
    next.panes = vec![
        (
            10,
            Rect {
                x: 0,
                y: 0,
                rows: 29,
                cols: 23,
            },
        ),
        (
            11,
            Rect {
                x: 24,
                y: 0,
                rows: 29,
                cols: 40,
            },
        ),
        (
            12,
            Rect {
                x: 65,
                y: 0,
                rows: 29,
                cols: 7,
            },
        ),
    ];
    view.set_layout(next);

    // Whatever that cell means NOW is what the release must commit - never
    // the stale 11|12 seam, which has slid out from under the pointer.
    let fresh = view.drop_zone_at(r, c);
    assert_ne!(
        fresh,
        Some(DropZone {
            target: 11,
            dir: Dir::Right
        }),
        "precondition: the cell must no longer mean what it did"
    );
    view.pane_drag_to(r, c, Instant::now());
    assert_eq!(
        view.commit_pane_drag(),
        fresh.map(|z| Command::MovePane {
            mover: Some(10),
            target: Some(z.target),
            dir: z.dir,
        }),
        "the release follows the layout it landed on"
    );
}

#[test]
fn a_t_junction_seam_previews_exactly_what_it_does() {
    // Pane 10 spans the left; 11 and 12 stack on the right. The 10|11 and
    // 10|12 segments are ONE divider - x-d807 addresses a seam by branch
    // child pair, so both resolve to the same zone. That is only acceptable
    // if the highlight says so: the whole divider must light, and the drop
    // must then land full-height, matching what lit.
    let mut view = two_pane_view();
    view.set_layout(LayoutView {
        panes: vec![
            (
                10,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 29,
                    cols: 35,
                },
            ),
            (
                11,
                Rect {
                    x: 36,
                    y: 0,
                    rows: 14,
                    cols: 36,
                },
            ),
            (
                12,
                Rect {
                    x: 36,
                    y: 15,
                    rows: 14,
                    cols: 36,
                },
            ),
        ],
        focus: 10,
        ..view.layout.clone()
    });
    view.frames.insert(12, text_frame(14, 36, 'c'));

    let upper = seam_cell_between(&view, 10, 11);
    let lower = seam_cell_between(&view, 10, 12);
    assert_ne!(upper.0, lower.0, "the two segments are different rows");
    assert_eq!(
        view.drop_zone_at(upper.0, upper.1),
        view.drop_zone_at(lower.0, lower.1),
        "one divider, one zone - both segments mean the same drop"
    );

    // The band spans pane 10's FULL height, so the preview shows the
    // full-height insert the drop actually performs. No mismatch between
    // what lights and what happens.
    let zone = view.drop_zone_at(lower.0, lower.1).expect("a seam zone");
    let (band_rows, _) = view.drop_band(zone).expect("10 has a rect");
    let r10 = view.pane_rect(10).expect("10 exists");
    assert_eq!(
        (band_rows.end - band_rows.start),
        r10.rows,
        "the highlight must cover the whole divider it will insert along"
    );
}

#[test]
fn every_cancel_path_sends_nothing() {
    // AC5-EDGE: Esc, a drop off any zone, and a drop on the pane's own
    // origin all end the gesture without putting a command on the wire.
    let mut view = two_pane_view();

    view.begin_pane_drag(10, Instant::now());
    assert!(view.cancel_pane_drag(), "Esc ends a live drag");
    assert!(
        view.commit_pane_drag().is_none(),
        "and leaves nothing to send"
    );

    // Released over the pane's own middle: no zone was ever a candidate.
    view.begin_pane_drag(10, Instant::now());
    let rect = view.pane_rect(10).expect("pane 10 exists");
    let mid_r = TAB_BAR_ROWS + rect.y + rect.rows / 2;
    let mid_c = view.panel_w() + rect.x + rect.cols / 2;
    view.pane_drag_to(mid_r, mid_c, Instant::now());
    assert_eq!(view.commit_pane_drag(), None);
}

#[test]
fn a_zone_on_the_dragged_pane_never_becomes_a_candidate() {
    // An origin drop is a cancel, so it must not even light up - the
    // gesture should read as "this does nothing" before the release.
    let mut view = two_pane_view();
    view.begin_pane_drag(10, Instant::now());
    let rect = view.pane_rect(10).expect("pane 10 exists");
    // Pane 10 sits at the layout's left rim, so its own left edge is a zone
    // that would target it.
    let edge = view.edge_zone_at(TAB_BAR_ROWS + rect.y, view.panel_w() + rect.x);
    assert_eq!(
        edge.map(|z| z.target),
        Some(10),
        "precondition: that rim targets 10"
    );
    view.pane_drag_to(
        TAB_BAR_ROWS + rect.y,
        view.panel_w() + rect.x,
        Instant::now(),
    );
    assert_eq!(view.pane_drag.and_then(|d| d.zone), None);
    assert_eq!(view.commit_pane_drag(), None);
}

#[test]
fn every_rim_drop_zone_actually_lights_something() {
    // The right and bottom rims resolve to a cell one PAST the terminal.
    // Clamping only the low side left those bands empty once compose()
    // trimmed them to the buffer, so the zone rendered nothing while a
    // release there still relocated the pane - and an unlit zone means
    // "this does nothing" everywhere else in this UI.
    //
    // two_pane_view tiles the content area exactly (35 + divider + 36 =
    // 72); three_pane_view leaves its last column uncovered, so its right
    // rim is filler rather than a pane and resolves to no zone at all.
    let mut view = two_pane_view();
    let panel_w = view.panel_w();
    let (term_rows, term_cols) = (view.term.0, view.term.1);
    let (a_rows, a_cols) = view.layout.area;
    let mid_col = panel_w + a_cols / 2;
    let mid_row = TAB_BAR_ROWS + a_rows / 2;

    for (row, col, want) in [
        (mid_row, panel_w, Dir::Left),
        (mid_row, panel_w + a_cols - 1, Dir::Right),
        (TAB_BAR_ROWS, mid_col, Dir::Up),
        (TAB_BAR_ROWS + a_rows - 1, mid_col, Dir::Down),
    ] {
        // A cell the hit-test really resolves to a rim zone; a fabricated
        // zone would prove nothing, since an interior pane's outward band
        // belongs to a seam zone instead.
        let zone = view
            .edge_zone_at(row, col)
            .unwrap_or_else(|| panic!("{want:?} rim cell ({row},{col}) is not a zone"));
        assert_eq!(zone.dir, want, "rim cell ({row},{col}) resolved oddly");

        // The regression itself: after compose()'s clamp to the cell
        // buffer, the band must still cover at least one real cell.
        let (br, bc) = view.drop_band(zone).expect("the target has a rect");
        assert!(
            br.start < term_rows && br.end.min(term_rows) > br.start,
            "{want:?} band rows {br:?} clamp to nothing against {term_rows}"
        );
        assert!(
            bc.start < term_cols && bc.end.min(term_cols) > bc.start,
            "{want:?} band cols {bc:?} clamp to nothing against {term_cols}"
        );
    }

    // And end to end for the right rim, the half that regressed: dragging
    // there really does light more than the resting frame does.
    let baseline = view
        .compose()
        .cells
        .iter()
        .filter(|c| c.flags & cell_flags::INVERSE != 0)
        .count();
    let (row, col) = (mid_row, panel_w + a_cols - 1);
    let target = view.edge_zone_at(row, col).expect("right rim zone").target;
    let mover = view
        .layout
        .panes
        .iter()
        .map(|(p, _)| *p)
        .find(|p| *p != target)
        .expect("a second pane to drag");
    view.begin_pane_drag(mover, Instant::now());
    assert!(view.pane_drag_to(row, col, Instant::now()));
    let lit = view
        .compose()
        .cells
        .iter()
        .filter(|c| c.flags & cell_flags::INVERSE != 0)
        .count();
    assert!(
        lit > baseline,
        "right rim lit nothing (lit={lit} baseline={baseline})"
    );
}

#[test]
fn an_interior_cell_tracks_the_nearest_pane_edge() {
    // The fix for "no preview until I drag to the rim": a cell in a pane's
    // interior (NOT on the content-area rim) now resolves to the nearest
    // edge of the pane under the pointer, so the destination band tracks
    // the pointer live. Pane 10 is 35x29 at the layout origin.
    let view = two_pane_view();
    let (pw, tb) = (view.panel_w(), TAB_BAR_ROWS);
    // (cr, cc) within pane 10, none on the area rim (cr!=0/28, cc!=0/71).
    for (cr, cc, want) in [
        (20u16, 17u16, Dir::Down), // lower-centre -> below
        (5, 17, Dir::Up),          // upper-centre -> above
        (14, 30, Dir::Right),      // right of centre -> beside (right)
        (14, 4, Dir::Left),        // left of centre  -> beside (left)
    ] {
        let z = view
            .edge_zone_at(tb + cr, pw + cc)
            .unwrap_or_else(|| panic!("interior ({cr},{cc}) went dark"));
        assert_eq!(z.target, 10, "interior ({cr},{cc}) targets its own pane");
        assert_eq!(z.dir, want, "interior ({cr},{cc}) resolved oddly");
    }

    // The user's exact gesture: dragging pane 11 over pane 10's lower
    // middle lights a Down zone BEFORE the pointer reaches the bottom rim
    // (cr=20, not 28), which is precisely what rim-only zones never did.
    let mut view = two_pane_view();
    view.begin_pane_drag(11, Instant::now());
    assert!(view.pane_drag_to(tb + 20, pw + 17, Instant::now()));
    let zone = view.pane_drag.and_then(|d| d.zone).expect("interior lit");
    assert_eq!((zone.target, zone.dir), (10, Dir::Down));
}

#[test]
fn the_scroll_indicator_outranks_the_grip() {
    // They contend for the same top row on a narrow pane. The indicator is
    // state, the grip is an affordance that has a keyboard equivalent, so
    // the indicator wins.
    let mut view = three_pane_view();
    let rect = view.pane_rect(10).expect("pane 10 exists");
    let narrow = Rect { cols: 9, ..rect };
    view.layout.panes[0].1 = narrow;
    let f = view.frames.get_mut(&10).expect("pane 10 has a frame");
    f.scroll_offset = 7;

    let (grow, gcols) = view.grip_span(narrow).expect("9 cols still fits a grip");
    let cols = view.term.1 as usize;
    let out = view.compose();
    let row: String = (0..cols)
        .map(|c| out.cells[grow as usize * cols + c].c)
        .collect();
    assert!(
        row.contains("[+7]"),
        "the scroll indicator must survive the grip: {row:?}"
    );
    // Precondition: they really did overlap, or this proves nothing.
    let ind_start = view.panel_w() + narrow.x + narrow.cols - 4;
    assert!(
        gcols.contains(&ind_start) || (ind_start..ind_start + 4).contains(&gcols.start),
        "fixture no longer overlaps: grip={gcols:?} indicator at {ind_start}"
    );
}

/// Every surviving `WIDE_SPACER` still has its double-width lead, and every
/// wide lead still owns its spacer. Break either and the row emits the wrong
/// number of columns, shifting or wrapping everything after it.
fn assert_pairs_intact(row: &[Cell], what: &str) {
    for (i, cell) in row.iter().enumerate() {
        if cell.flags & cell_flags::WIDE_SPACER != 0 {
            assert!(
                i > 0 && glyph_cols(row[i - 1].c) == 2,
                "{what}: orphaned spacer at column {i}: the row would shift left"
            );
        }
        if glyph_cols(cell.c) == 2 {
            assert!(
                i + 1 < row.len() && row[i + 1].flags & cell_flags::WIDE_SPACER != 0,
                "{what}: wide glyph at column {i} lost its spacer: the row would shift right"
            );
        }
    }
}

#[test]
fn the_name_modal_never_bisects_a_double_width_glyph() {
    // The one-row modal this replaced cleared whole rows and could not
    // strand a half. The block stamps a SUB-RANGE of three rows, so pane
    // content - arbitrary program output - can straddle either edge of any
    // of them.
    let view = shot_view(
        (24, 80),
        vec![named_meta(1, "footnote", &["main"], 0)],
        vec![],
    );
    let (rows, cols) = (24usize, 80usize);

    // Read the block's extent off a probe rather than recomputing its
    // format here: a test that restates the format stops testing it. Since
    // x-b465 the block is the SHARED frame, so its height and origin come
    // from the chrome - discover which rows it paints instead of naming
    // three.
    let mut probe = vec![Cell::default(); rows * cols];
    view.draw_name_modal(&mut probe, rows, cols, "rename tab", "release", None);
    let painted: Vec<(usize, usize, usize)> = (0..rows)
        .filter_map(|r| {
            let lit: Vec<usize> = (0..cols)
                .filter(|&c| probe[r * cols + c] != Cell::default())
                .collect();
            (!lit.is_empty()).then(|| (r, lit[0], lit[lit.len() - 1] + 1))
        })
        .collect();
    assert!(!painted.is_empty(), "the modal painted nothing");
    assert!(
        painted.iter().all(|&(_, c0, end)| c0 > 0 && end < cols),
        "fixture needs margins on both sides: {painted:?}"
    );

    let mut cells = vec![Cell::default(); rows * cols];
    let lead = |c: char| Cell {
        c,
        fg: Color::Default,
        bg: Color::Default,
        flags: 0,
    };
    let spacer = Cell {
        c: ' ',
        fg: Color::Default,
        bg: Color::Default,
        flags: cell_flags::WIDE_SPACER,
    };
    // Straddle BOTH edges of EVERY row the block touches: a pair whose
    // spacer is the block's first cell, and one whose lead is its last.
    for &(row, c0, end) in &painted {
        cells[row * cols + c0 - 1] = lead('\u{4f60}');
        cells[row * cols + c0] = spacer;
        cells[row * cols + end - 1] = lead('\u{597d}');
        cells[row * cols + end] = spacer;
    }

    view.draw_name_modal(&mut cells, rows, cols, "rename tab", "release", None);
    for &(row, _, _) in &painted {
        assert_pairs_intact(
            &cells[row * cols..(row + 1) * cols],
            &format!("modal row {row}"),
        );
    }
    // And the prompt still drew: the target in the chrome, the name in the
    // body.
    let screen: String = (0..rows)
        .map(|r| {
            cells[r * cols..(r + 1) * cols]
                .iter()
                .map(|c| c.c)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("rename tab"), "target missing: {screen}");
    assert!(screen.contains("release_"), "prompt missing: {screen}");
}

#[test]
fn a_long_name_scrolls_so_the_cursor_stays_visible() {
    // Stamping the head of the prompt cut the `_` off the right edge, so on
    // a narrow terminal the operator typed a name they could not see. Same
    // shape as the clipped notice and the clipped action id: the payload
    // sits at the end, and the end is what a narrow render drops.
    let view = shot_view(
        (24, 40),
        vec![named_meta(1, "footnote", &["main"], 0)],
        vec![],
    );
    let (rows, cols) = (24usize, 40usize);
    let mut cells = vec![Cell::default(); rows * cols];
    let long = "a-really-quite-long-release-branch-name";
    view.draw_name_modal(&mut cells, rows, cols, "rename tab", long, None);

    // (x-b465) The name is the framed body, so read the whole block: the
    // title sits on the top border, the name a row below it.
    let screen = |cells: &[Cell]| -> String {
        (0..rows)
            .map(|r| {
                cells[r * cols..(r + 1) * cols]
                    .iter()
                    .map(|c| c.c)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let shot = screen(&cells);
    assert!(
        shot.contains("name_"),
        "the cursor and the tail of what was typed must be visible: {shot}"
    );
    assert!(
        shot.contains('…'),
        "and the scroll has to be visible as a scroll: {shot}"
    );
    // A name that fits is untouched: no ellipsis, target still named.
    let mut cells = vec![Cell::default(); rows * cols];
    view.draw_name_modal(&mut cells, rows, cols, "rename tab", "short", None);
    let shot = screen(&cells);
    assert!(shot.contains("rename tab"), "got {shot}");
    assert!(shot.contains("short_"), "got {shot}");
    assert!(!shot.contains('…'), "nothing to scroll: {shot}");
}

#[test]
fn a_clipped_notice_reads_as_clipped() {
    // The keymap warnings this PR added are long, and the strip clips from
    // the right. Silently, a cut notice reads as a whole sentence, which is
    // how "$FNO_CONFIG points at /long/path.yaml, which the mux reads as
    // TOML" rendered on a 40-column strip as a path fragment and nothing
    // else: technically a warning, practically the silence it replaced.
    let mut view = shot_view(
        (24, 200),
        vec![named_meta(1, "footnote", &["main"], 0)],
        vec![],
    );
    let long = "keys on defaults: $FNO_CONFIG not TOML (/a/very/long/path/settings.yaml)";
    view.set_notice(long.to_string());

    let (_, wide) = view.notice_overlay(200).expect("a notice is set");
    assert_eq!(wide, long, "with room to spare nothing is cut");

    let (start, narrow) = view.notice_overlay(40).expect("a notice is set");
    assert!(
        narrow.ends_with('…'),
        "a cut notice must say it was cut: {narrow:?}"
    );
    assert!(
        narrow.chars().count() + start < 40,
        "and still fit the strip: {narrow:?} at {start}"
    );
    // What survives is the part that tells the operator what happened.
    assert!(
        narrow.contains("defaults") && narrow.contains("not TOML"),
        "the meaning has to survive the clip, not the path: {narrow:?}"
    );
}

#[test]
fn the_key_modal_shows_exact_action_ids_on_a_narrow_terminal() {
    // The modal advertises the id an operator types into `config.mux.keys`,
    // so a clipped one is worse than none: `grab-…` still looks like an id.
    // The generic row renderer clipped the whole line from the RIGHT, which
    // is exactly where the id sits, so this only showed at the narrow end -
    // the wide case the id was added for looked fine.
    let modal = build_keys_modal();
    // Tall enough that the popup does not scroll: the subject here is
    // WIDTH, and a scrolled-off row would read as a clipped id.
    for cols in [40u16, 60, 100] {
        let out = modal.popup.render((80, cols));
        let screen: Vec<String> = out.lines.iter().map(|l| l.text.clone()).collect();
        for kb in crate::keys::key_bindings() {
            assert!(
                screen.iter().any(|l| l.contains(kb.action)),
                "at {cols} cols the modal must show `{}` in full, not clipped; \
                     rendered:\n{}",
                kb.action,
                screen.join("\n")
            );
        }
    }
}

#[test]
fn condensing_a_narrow_strip_never_shortens_an_overflow_counter() {
    // A counter carries a Tab hit so that clicking it walks the strip, which
    // is exactly why its role cannot be read off `hit`. Shortened, `‹13 `
    // becomes `‹1 ` - a wrong count stated as confidently as a right one -
    // and dropped, its click target goes with it.
    let label = TabSpan {
        text: " a-very-long-workspace-name ".to_string(),
        flags: cell_flags::BOLD,
        fg: Color::Default,
        hit: None,
        role: SpanRole::Squad,
    };
    let counter = |text: &str, role: SpanRole| TabSpan {
        text: text.to_string(),
        flags: cell_flags::DIM,
        fg: Color::Default,
        hit: Some(TabHit::Tab(7)),
        role,
    };
    let strip = || {
        vec![
            label.clone(),
            counter("\u{2039}13 ", SpanRole::OverflowLeft(13)),
            TabSpan {
                text: "[a-tab-label]".to_string(),
                flags: 0,
                fg: Color::Default,
                hit: Some(TabHit::Tab(3)),
                role: SpanRole::Tab,
            },
            counter(" 4\u{203a}", SpanRole::OverflowRight(4)),
            TabSpan {
                text: " + ".to_string(),
                flags: cell_flags::DIM,
                fg: Color::Default,
                hit: Some(TabHit::NewTab),
                role: SpanRole::NewTab,
            },
        ]
    };
    // 40 columns is a supported strip width. 30 forces the squad label to
    // give. Neither may cost a counter its count or the `+` its click.
    for width in [40usize, 30] {
        let mut spans = strip();
        condense_to_width(&mut spans, width);
        let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(
            texts.contains(&"\u{2039}13 ") && texts.contains(&" 4\u{203a}"),
            "at {width}: both counters must survive intact: {texts:?}"
        );
        assert!(
            texts.contains(&" + "),
            "at {width}: the `+` is the only mouse route to a new tab: {texts:?}"
        );
        assert!(
            spans.iter().any(|s| s.role == SpanRole::Tab),
            "at {width}: the active tab must still be on the strip: {texts:?}"
        );
        assert!(
            spans.iter().map(|s| s.text.chars().count()).sum::<usize>() <= width,
            "at {width}: still has to fit: {texts:?}"
        );
    }
    let mut narrow = strip();
    condense_to_width(&mut narrow, 30);
    assert!(
        narrow[0].text.chars().count() < label.text.chars().count(),
        "at 30 the squad label is what is left to give: {:?}",
        narrow.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn a_tab_that_stops_being_shown_starts_being_counted() {
    // Hiding a tab without touching the counter leaves the strip stating a
    // number that was computed before the hiding: the same
    // confidently-wrong shape as a counter squeezed down to `‹1 `.
    let squad = TabSpan {
        text: " ws ".to_string(),
        flags: cell_flags::BOLD,
        fg: Color::Default,
        hit: None,
        role: SpanRole::Squad,
    };
    let tab = |id: TabId| TabSpan {
        text: format!("[tab-{id}]"),
        flags: 0,
        fg: Color::Default,
        hit: Some(TabHit::Tab(id)),
        role: SpanRole::Tab,
    };
    let plus = TabSpan {
        text: " + ".to_string(),
        flags: cell_flags::DIM,
        fg: Color::Default,
        hit: Some(TabHit::NewTab),
        role: SpanRole::NewTab,
    };

    // An existing right counter absorbs the newly hidden tab, and points at
    // it: it is now the nearest one hidden.
    let mut spans = vec![
        squad.clone(),
        tab(1),
        tab(2),
        TabSpan {
            text: " 4\u{203a}".to_string(),
            flags: cell_flags::DIM,
            fg: Color::Default,
            hit: Some(TabHit::Tab(9)),
            role: SpanRole::OverflowRight(4),
        },
        plus.clone(),
    ];
    condense_to_width(&mut spans, 14);
    let counter = spans
        .iter()
        .find(|s| matches!(s.role, SpanRole::OverflowRight(_)))
        .expect("the counter survives");
    assert_eq!(
        counter.role,
        SpanRole::OverflowRight(5),
        "4 hidden + 1 more"
    );
    assert_eq!(counter.text, " 5\u{203a}");
    assert!(
        matches!(counter.hit, Some(TabHit::Tab(2))),
        "clicking it should walk back to the tab just hidden"
    );

    // And with no counter yet, the hidden tab becomes one rather than
    // vanishing off the strip unaccounted for. Note that this particular
    // step buys no columns - a three-column tab becomes a three-column
    // counter - so it happens for truthfulness, not for width.
    let mut spans = vec![squad, tab(2), plus];
    condense_to_width(&mut spans, 8);
    let counter = spans
        .iter()
        .find(|s| matches!(s.role, SpanRole::OverflowRight(_)))
        .expect("a counter appears for the tab that stopped being shown");
    assert_eq!(counter.role, SpanRole::OverflowRight(1));
    assert!(matches!(counter.hit, Some(TabHit::Tab(2))));
}

#[test]
fn the_grip_never_bisects_a_double_width_glyph() {
    // The compositor SKIPS a WIDE_SPACER cell, so a lead left without its
    // spacer (or vice versa) makes the row emit the wrong column count and
    // shifts everything after it. Pane content is arbitrary program output,
    // so a CJK glyph can sit anywhere - including straddling the grip.
    let mut view = two_pane_view();
    let rect = view.pane_rect(10).expect("pane 10 exists");
    let (grow, gcols) = view.grip_span(rect).expect("pane 10 has room");

    // Straddle BOTH edges: a wide pair whose spacer is the grip's first
    // cell, and another whose lead is the grip's last cell.
    let panel_w = view.panel_w();
    let frow = (grow - TAB_BAR_ROWS - rect.y) as usize;
    let local = |outer: u16| (outer - panel_w - rect.x) as usize;
    let (lead_before, last_cell) = (local(gcols.start) - 1, local(gcols.end) - 1);
    let frame = view.frames.get_mut(&10).expect("pane 10 has a frame");
    let fcols = frame.cols as usize;
    for (at, ch, spacer) in [
        (lead_before, '\u{4f60}', true),
        (last_cell, '\u{597d}', true),
    ] {
        frame.cells[frow * fcols + at] = Cell {
            c: ch,
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        };
        if spacer {
            frame.cells[frow * fcols + at + 1] = Cell {
                c: ' ',
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::WIDE_SPACER,
            };
        }
    }

    let out = view.compose();
    let cols = view.term.1 as usize;
    let row = &out.cells[grow as usize * cols..(grow as usize + 1) * cols];
    assert_pairs_intact(row, "grip row");
    // And the grip still drew.
    for (i, ch) in GRIP.chars().enumerate() {
        assert_eq!(row[gcols.start as usize + i].c, ch);
    }
}

#[test]
fn a_drag_ends_when_the_pane_it_moves_disappears() {
    // AC7-FR: the dragged pane's process exits mid-gesture.
    let mut view = three_pane_view();
    view.begin_pane_drag(10, Instant::now());
    let (r, c) = seam_cell_between(&view, 11, 12);
    view.pane_drag_to(r, c, Instant::now());

    let mut next = view.layout.clone();
    next.panes.retain(|(id, _)| *id != 10);
    next.focus = 11;
    view.set_layout(next);

    assert!(view.pane_drag.is_none(), "the drag cannot outlive its pane");
    assert!(
        view.notice.is_some(),
        "and it says why rather than dying silently"
    );
}
#[test]
fn probe_right_rim_band() {
    let mut view = two_pane_view();
    view.begin_pane_drag(10, Instant::now());
    let rect = view.pane_rect(11).unwrap();
    let r = TAB_BAR_ROWS + rect.y + 5;
    let c = view.panel_w() + rect.x + rect.cols - 1; // rightmost content col
    let z = view.edge_zone_at(r, c).expect("rim zone");
    eprintln!("zone={:?}", z);
    let band = view.drop_band(z).expect("band");
    eprintln!(
        "band rows={:?} cols={:?} termcols={}",
        band.0, band.1, view.term.1
    );
    assert!(view.pane_drag_to(r, c, Instant::now()));
    let frame = view.compose();
    let cols = view.term.1 as usize;
    let lit = (0..view.term.0 as usize)
        .flat_map(|rr| (0..cols).map(move |cc| (rr, cc)))
        .filter(|(rr, cc)| frame.cells[rr * cols + cc].flags & cell_flags::INVERSE != 0)
        .count();
    eprintln!("inverse cells = {}", lit);
    assert!(lit > 0, "right-rim drop zone lit NOTHING");
}

#[test]
fn probe_bottom_rim_band() {
    let mut view = two_pane_view();
    view.begin_pane_drag(10, Instant::now());
    let rect = view.pane_rect(11).unwrap();
    let r = TAB_BAR_ROWS + rect.y + rect.rows - 1;
    let c = view.panel_w() + rect.x + 5;
    let z = view.edge_zone_at(r, c).expect("rim zone");
    eprintln!("zone={:?}", z);
    eprintln!("band={:?}", view.drop_band(z));
    assert!(view.pane_drag_to(r, c, Instant::now()));
    let frame = view.compose();
    let cols = view.term.1 as usize;
    let lit = (0..view.term.0 as usize)
        .flat_map(|rr| (0..cols).map(move |cc| (rr, cc)))
        .filter(|(rr, cc)| frame.cells[rr * cols + cc].flags & cell_flags::INVERSE != 0)
        .count();
    eprintln!("inverse cells = {}", lit);
    assert!(lit > 0, "bottom-rim drop zone lit NOTHING");
}

// ---------------------------------------------------------------------
// UX screenshots
//
// These compose REAL frames through the one composer the terminal is
// painted from, assert the property the operator reported, and (when
// `FNO_UX_SHOTS=<dir>` is set) write the frame out as HTML so the result
// can be LOOKED AT. The assertion is the gate; the HTML is the evidence.
//
//   FNO_UX_SHOTS=/tmp/shots cargo test -p fno ux_shot
// ---------------------------------------------------------------------

/// A squad whose tabs carry real names, so a strip fixture reads like a
/// working operator's rather than `1 2 3`.
fn named_meta(id: u64, name: &str, tabs: &[&str], active_tab: usize) -> SquadMeta {
    SquadMeta {
        id,
        name: name.into(),
        canonical_cwd: format!("/code/{name}"),
        tabs: tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabMeta {
                id: i as u64,
                name: (*t).to_string(),
                named: true,
                panes: Vec::new(),
            })
            .collect(),
        active_tab,
        panes: tabs.len(),
    }
}

fn shot_view(term: (u16, u16), squads: Vec<SquadMeta>, agents: Vec<AgentRow>) -> View {
    let (rows, cols) = term;
    let active = squads[0].id;
    let mut view = View::new(
        term,
        "main".into(),
        LayoutView {
            squads,
            active_squad: active,
            panes: vec![(
                10,
                Rect {
                    x: 0,
                    y: 0,
                    rows: rows - 1,
                    cols: cols - 28,
                },
            )],
            focus: 10,
            area: (rows - 1, cols - 28),
            agents,
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    view.frames
        .insert(10, text_frame(rows - 1, cols - 28, '\u{b7}'));
    view
}

fn shot_agent(squad: u64, name: &str, badge: Option<AgentBadge>) -> AgentRow {
    let mut r = blocked_row(name, 0, None);
    r.squad = Some(squad);
    r.pane_id = None;
    r.badge = badge;
    r
}

/// Item 1. The rename prompt was `INVERSE | BOLD` over default colours, so
/// how legible it came out was decided by the reader's theme rather than by
/// us. The gate is the WORST case across themes, not the flag bits - the
/// flag bits looked fine, which is exactly how this shipped.
///
/// Covers all three name modals: they share one painter, so the create and
/// recruit prompts are the same cells.
#[test]
fn ux_shot_rename_prompt_is_legible() {
    use crate::frame_html::{self, write_shot};
    // (x-b465) The modal wears the shared frame, so the typed name no longer
    // sits on the terminal's middle row - the border, title and footer share
    // the block. Find the BODY row by its content: that is the row whose
    // legibility this test is about.
    let modal_at = |view: &View| -> (Frame, usize) {
        let frame = view.compose();
        let cols = frame.cols as usize;
        let row = (0..frame.rows as usize)
            .find(|r| {
                (0..cols)
                    .map(|c| frame.cells[r * cols + c].c)
                    .collect::<String>()
                    .contains('_')
            })
            .unwrap_or(frame.rows as usize / 2);
        (frame, row)
    };
    let mut view = shot_view(
        (34, 150),
        vec![named_meta(1, "footnote", &["main", "review"], 0)],
        vec![],
    );
    view.rename = Some((RenameTarget::Tab(0), "release-notes".into()));
    let (frame, row) = modal_at(&view);
    let cols = frame.cols as usize;
    // The modal is the contiguous inverse run through the centre column.
    // Scoped rather than "every inverse cell on the row": the sideline's
    // focused-row band and the divider accents are inverse too, and they
    // are different rules with different styles (one of them BOLD), so a
    // bare flag test would judge them against the modal's contract.
    // Anchor the span walk on a column the modal certainly owns - the cursor
    // it just drew. The block is centered on the CONTENT viewport, which the
    // sideline offsets, so the frame's own middle column can sit outside it.
    let modal_cols =
        |f: &Frame, row: usize, at: usize| -> Option<std::ops::RangeInclusive<usize>> {
            let inv = |c: usize| f.cells[row * cols + c].flags & cell_flags::INVERSE != 0;
            if !inv(at) {
                return None;
            }
            let lo = (0..=at).rev().take_while(|&c| inv(c)).last().unwrap_or(at);
            let hi = (at..cols).take_while(|&c| inv(c)).last().unwrap_or(at);
            Some(lo..=hi)
        };
    let cursor_col = (0..cols)
        .find(|&c| frame.cells[row * cols + c].c == '_')
        .unwrap_or(cols / 2);
    let span = modal_cols(&frame, row, cursor_col).expect("the prompt row painted nothing");
    let prompt: Vec<&Cell> = span.clone().map(|c| &frame.cells[row * cols + c]).collect();
    for cell in &prompt {
        assert_eq!(
            cell.flags & cell_flags::BOLD,
            0,
            "bold on top of the inversion makes the pair the reader's \
                 terminal settings decide"
        );
        // While the modal inherits, this holds by construction (see
        // `inverting_the_default_pair_costs_a_scheme_nothing`), so it can
        // only fail if someone stops inheriting - which is exactly the
        // regression it exists to catch, and did catch when Indexed(0) on
        // Indexed(15) was tried. Not a legibility measurement.
        //
        // The bar is the THEME'S OWN body text, not an absolute ratio. An
        // absolute floor asks the modal to beat the scheme its reader chose
        // (Solarized Light is 4.1:1 by design), and the only way to meet it
        // is to override their colours - which is exactly what failed here:
        // Indexed(0) on Indexed(15) measured 21:1 against an idealised
        // palette and 4.6:1 in the Macchiato the reporter actually runs.
        for theme in frame_html::THEMES {
            let (ratio, body) = (
                frame_html::contrast_ratio(cell, theme),
                frame_html::body_contrast(theme),
            );
            assert!(
                ratio + 0.01 >= body,
                "prompt cell {:?} paints at {ratio:.2}:1 on {}, below that \
                     scheme's own body text at {body:.2}:1",
                cell.c,
                theme.name
            );
        }
    }
    // The block, not a text-hugging stripe: the frame's own rows above and
    // below the body.
    for r in [row - 1, row + 1] {
        assert!(
            span.clone()
                .any(|c| frame.cells[r * cols + c].flags & cell_flags::INVERSE != 0),
            "row {r} carries no margin, so the prompt is a stripe not a block"
        );
    }
    // The hint has to be as readable as the prompt it qualifies; it is the
    // half the operator called out. Since x-b465 the target is the chrome
    // title and the hint is the footer, so both live in the block on rows of
    // their own rather than sharing one line.
    let text = frame_text(&frame);
    assert!(
        text.contains("rename tab"),
        "the modal lost the target it names: {text}"
    );
    assert!(
        text.contains("release-notes_"),
        "the modal lost the name being typed: {text}"
    );
    assert!(
        text.contains("empty resets to auto"),
        "the modal lost its hint: {text}"
    );
    // Same painter, same guarantee, for the sibling prompts.
    let mut sib = shot_view(
        (34, 150),
        vec![named_meta(1, "footnote", &["main"], 0)],
        vec![],
    );
    sib.create = Some("growth".into());
    let (sib_frame, sib_row) = modal_at(&sib);
    let sib_col = (0..cols)
        .find(|&c| sib_frame.cells[sib_row * cols + c].c == '_')
        .unwrap_or(cols / 2);
    assert!(
        modal_cols(&sib_frame, sib_row, sib_col).is_some(),
        "the new-workspace prompt does not share the modal treatment"
    );
    write_shot(&frame, "01-rename-prompt", "rename prompt (after)");
}

/// Chrome never hardcodes a colour.
///
/// The mux paints into the user's terminal, and terminal themes are a
/// solved, user-owned space with thousands of options. So chrome uses
/// `Color::Default` plus attributes, or at most an `Indexed` palette slot,
/// which is still the reader's colour - `Indexed(3)` means "whatever your
/// scheme calls yellow", not a yellow we picked. `Color::Rgb` would be us
/// overriding a choice that is not ours to make.
///
/// This is the rule the name-entry modal broke and had to be walked back:
/// it named an explicit pair, and on the reporter's own scheme that measured
/// worse than inheriting would have. A structural test rather than a review
/// habit, because the tempting version of that mistake looks like care.
#[test]
fn chrome_paints_in_the_readers_colours_not_ours() {
    use crate::frame_html::{body_contrast, contrast_ratio, THEMES};
    let mut view = shot_view(
        (34, 150),
        vec![named_meta(1, "footnote", &["main", "review"], 0)],
        vec![shot_agent(1, "reviewer", Some(AgentBadge::Blocked))],
    );
    view.rename = Some((RenameTarget::Tab(0), "release-notes".into()));
    let frame = view.compose();
    for (i, cell) in frame.cells.iter().enumerate() {
        assert!(
            !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..)),
            "cell {i} ({:?}) hardcodes an RGB colour; chrome uses the \
                 reader's palette",
            cell.c
        );
    }
    // Informational, not a bound: how the one palette slot mux does use
    // lands on a few real schemes. mux does not choose these numbers - they
    // are the scheme's own yellow against the scheme's own background - so
    // there is nothing here to assert, only something to know when deciding
    // whether a signal may rest on colour alone.
    let accent = Cell {
        c: '\u{25b2}',
        fg: LATTICE_ACCENT,
        bg: Color::Default,
        flags: cell_flags::BOLD,
    };
    for theme in THEMES {
        eprintln!(
            "accent on {:22} {:5.2}:1   (that scheme's body text {:.2}:1)",
            theme.name,
            contrast_ratio(&accent, theme),
            body_contrast(theme)
        );
    }
}

/// Item 2. A workspace header has to separate from its own contents.
///
/// It cannot do it by SIZE (one cell grid, one font) and it cannot do it by
/// weight alone either, which is the part that is easy to get wrong: a
/// working, blocked or done agent row is already BOLD (`lattice_style`), so
/// a bold header sits at exactly the weight of a busy workspace's rows. The
/// rule (`section_rule`) is what actually carries the separation; the
/// all-headers-BOLD change only buys the idle case. Both are asserted, and
/// the bold-agent-row fact is asserted too, so deleting the rule in favour
/// of "just make headers bold" fails here rather than in a screenshot.
#[test]
fn ux_shot_workspace_headers_outweigh_their_rows() {
    use crate::frame_html::write_shot;
    let squads = vec![
        named_meta(1, "main", &["1"], 0),
        named_meta(2, "c3po", &["1"], 0),
        named_meta(3, "fno-platform", &["1"], 0),
        named_meta(4, "readyrule/readyrule-web", &["1"], 0),
    ];
    let mut view = shot_view(
        (34, 150),
        squads.clone(),
        vec![
            shot_agent(1, "archer", Some(AgentBadge::Working)),
            shot_agent(1, "sigma", None),
            shot_agent(2, "scribe", None),
            shot_agent(2, "curator", Some(AgentBadge::Working)),
            shot_agent(3, "reviewer", Some(AgentBadge::Blocked)),
            shot_agent(4, "impeccable", Some(AgentBadge::Working)),
            shot_agent(4, "vercel-deploy", None),
        ],
    );
    // Expand every workspace: an inactive squad folds by default, and a
    // folded section trivially "reads as a section". The reported case is
    // the open one, where header and rows sit next to each other.
    for s in &squads {
        view.section_view
            .insert(section_key(s), SectionView::Expanded);
    }
    let frame = view.compose();
    let cols = frame.cols as usize;
    let panel_w = view.panel_w() as usize;
    // Find each squad-name row by its rendered label, then compare its
    // weight against the row below it (an agent row of the same squad).
    let text = frame_text(&frame);
    let lines: Vec<&str> = text.lines().collect();
    let head_of = |row: usize| -> String { lines[row].chars().take(panel_w).collect() };
    let bold_in = |row: usize| -> bool {
        (0..panel_w).any(|c| frame.cells[row * cols + c].flags & cell_flags::BOLD != 0)
    };
    let mut checked = 0;
    // Every workspace header, active or not, is bold AND ruled. `main` is the
    // active one; the other three used to render plain.
    for name in ["main", "c3po", "fno-platform"] {
        let Some(r) = lines
            .iter()
            .position(|l| l.chars().take(panel_w).collect::<String>().contains(name))
        else {
            panic!("workspace header {name:?} never painted");
        };
        assert!(bold_in(r), "header {name:?} is not bold");
        assert!(
            head_of(r).contains('\u{2500}'),
            "header {name:?} carries no rule, so it does not separate: {:?}",
            head_of(r)
        );
        checked += 1;
    }
    assert_eq!(checked, 3);
    // A workspace whose name fills the rail keeps its width instead of
    // squeezing in a stub rule.
    let long = lines
        .iter()
        .position(|l| {
            l.chars()
                .take(panel_w)
                .collect::<String>()
                .contains("readyrule/readyrule-web")
        })
        .expect("the long-named workspace never painted");
    assert!(
        !head_of(long).contains('\u{2500}'),
        "a header with no room should stay unruled, not print a stub"
    );
    // The fact that makes the rule necessary: a working agent row is bold
    // too, so weight cannot be what separates a header from its contents.
    let working_row = lines
        .iter()
        .position(|l| l.contains("archer"))
        .expect("the working agent row never painted");
    assert!(
        bold_in(working_row),
        "fixture drift: this test's whole point is that a working agent row \
             is ALSO bold, so a bold header cannot out-weigh it"
    );
    write_shot(
        &frame,
        "02-workspace-sections",
        "workspace sections (after)",
    );
}

/// Item 3. Twenty tabs. Every tab must be REACHABLE: the active one is
/// always painted, the counters name what is hidden, and every painted span
/// hit-tests back to a tab (which is what makes a click work).
#[test]
fn ux_shot_twenty_tabs_stay_reachable() {
    use crate::frame_html::write_shot;
    let names: Vec<String> = (1..=20).map(|i| format!("task-{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for active in [0usize, 6, 13, 19] {
        let view = shot_view(
            (34, 150),
            vec![named_meta(1, "footnote", &refs, active)],
            vec![],
        );
        let window = view.tab_bar_window();
        let painted: usize = window.iter().map(|s| s.text.chars().count()).sum();
        assert!(
            painted + view.panel_w() as usize <= view.term.1 as usize,
            "the strip overflows at active={active}: {painted} cols of tabs"
        );
        let active_name = &names[active];
        assert!(
            window
                .iter()
                .any(|s| s.text.contains(active_name.as_str()) && s.text.starts_with('[')),
            "active tab {active_name} is not on the strip"
        );
        // Reachability: the hidden ends carry a counter whose click target
        // is the nearest tab it hides, so no tab is more than a few clicks
        // away and none is unreachable.
        let hidden_left = window.iter().any(|s| s.text.starts_with('\u{2039}'));
        let hidden_right = window.iter().any(|s| s.text.ends_with('\u{203a}'));
        assert!(
            hidden_left || hidden_right,
            "twenty tabs must not all fit; the fixture is not testing overflow"
        );
        for span in &window {
            if span.text.starts_with('\u{2039}') || span.text.ends_with('\u{203a}') {
                assert!(
                    matches!(span.hit, Some(TabHit::Tab(_))),
                    "an overflow counter that cannot be clicked is decoration"
                );
            }
        }
        // The `+` affordance is pinned: it is the only mouse route to a new
        // tab and must never scroll away.
        assert!(
            window.iter().any(|s| matches!(s.hit, Some(TabHit::NewTab))),
            "the + affordance scrolled off at active={active}"
        );
        if active == 13 {
            let frame = view.compose();
            write_shot(&frame, "03-twenty-tabs", "twenty tabs, active 14 (after)");
        }
    }
}

/// The narrow end of item 3. At the 40-column content minimum with a long
/// workspace name and long tab names, not one whole tab fits beside the
/// pinned chrome. The strip must condense rather than overflow: clipping is
/// what took the `+` away, and the `+` is the only mouse route to a new tab.
#[test]
fn tab_strip_condenses_instead_of_overflowing_a_narrow_terminal() {
    let names: Vec<String> = (1..=20)
        .map(|i| format!("release-candidate-{i:02}"))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for cols in [68u16, 72, 80, 100] {
        let view = shot_view(
            (24, cols),
            vec![named_meta(1, "readyrule/readyrule-web", &refs, 13)],
            vec![],
        );
        let window = view.tab_bar_window();
        let painted: usize = window.iter().map(|s| s.text.chars().count()).sum();
        let avail = (cols as usize).saturating_sub(view.panel_w() as usize);
        assert!(
            painted <= avail,
            "strip overflows at {cols} cols: {painted} > {avail}"
        );
        assert!(
            window.iter().any(|s| matches!(s.hit, Some(TabHit::NewTab))),
            "the + must survive condensation at {cols} cols"
        );
        // Every painted span still hit-tests, so condensing never leaves a
        // decorative stub the operator cannot click.
        for span in &window {
            if span.text.chars().count() > 0 {
                assert!(
                    span.hit.is_some() || std::ptr::eq(span, &window[0]),
                    "a painted span lost its click target at {cols} cols"
                );
            }
        }
    }
}

/// (x-b465) The group marker costs three columns on every grouped tab, and
/// the strip is the surface with the least room to spare. Condensation has
/// to survive it: the `+` is still the only mouse route to a new tab, and an
/// overflow counter that scrolls away is worse than a missing marker.
#[test]
fn a_strip_full_of_grouped_tabs_still_condenses() {
    let names: Vec<String> = (1..=20)
        .map(|i| format!("release-candidate-{i:02}"))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for cols in [44u16, 52, 60, 68, 80, 100] {
        let mut squad = named_meta(1, "readyrule/readyrule-web", &refs, 13);
        // Every tab a four-pane group, the worst case for width.
        for (t, tab) in squad.tabs.iter_mut().enumerate() {
            tab.panes = (0..4)
                .map(|p| crate::proto::PaneMeta {
                    id: (t * 10 + p) as u64,
                    label: String::new(),
                })
                .collect();
        }
        let view = shot_view((24, cols), vec![squad], vec![]);
        let window = view.tab_bar_window();
        let painted: usize = window.iter().map(|s| s.text.chars().count()).sum();
        let avail = (cols as usize).saturating_sub(view.panel_w() as usize);
        assert!(
            painted <= avail,
            "a grouped strip overflows at {cols} cols: {painted} > {avail}"
        );
        assert!(
            window.iter().any(|s| matches!(s.hit, Some(TabHit::NewTab))),
            "the + must survive the markers at {cols} cols"
        );
        // Width and the `+` alone pass on the outcome that matters most:
        // a strip of identical nameless `▤` cells. Columns come off the
        // right, so the marker would otherwise outlive the name it marks.
        // No visible tab may be marker-only.
        for span in window.iter().filter(|s| s.role == SpanRole::Tab) {
            let bare: String = span
                .text
                .chars()
                .filter(|c| !matches!(c, ' ' | '[' | ']') && *c != TAB_GROUP_GLYPH)
                .collect();
            assert!(
                !bare.is_empty(),
                "a tab kept its marker and lost its whole name at {cols} cols: {:?}",
                span.text
            );
        }
    }
    // And the marker really is rendering, or the widths above prove nothing.
    let mut squad = named_meta(1, "footnote", &["build"], 0);
    squad.tabs[0].panes = (0..4)
        .map(|p| crate::proto::PaneMeta {
            id: p,
            label: String::new(),
        })
        .collect();
    let view = shot_view((24, 100), vec![squad], vec![]);
    let strip: String = view
        .tab_bar_window()
        .iter()
        .map(|s| s.text.clone())
        .collect();
    assert!(
        strip.contains("▤ build·4"),
        "a four-pane tab names its size in the strip: {strip:?}"
    );
}

#[test]
fn condensing_sheds_a_group_marker_before_the_last_of_its_name() {
    // Tested on `condense_to_width` directly, not through the strip. Under
    // real layouts the strip HIDES a tab into the overflow counter long
    // before shrinking one this far - measured across a dozen widths and
    // both tab counts - so an integration assertion here passes whatever
    // `shrink` does. That is the vacuous-test trap twice over in this PR
    // already; this pins the rule where the rule lives.
    let span = |text: &str| TabSpan {
        text: text.to_string(),
        flags: 0,
        fg: Color::Default,
        hit: Some(TabHit::Tab(1)),
        role: SpanRole::Tab,
    };
    let mut spans = vec![span(" ▤ build·4 ")];
    super::condense_to_width(&mut spans, 4);
    let text = spans.first().map(|s| s.text.clone()).unwrap_or_default();
    let name: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    assert!(
        !name.is_empty(),
        "the marker outlived the name it marks: {text:?}"
    );
}

/// The before/after pair the operator can compare: the same twenty tabs
/// under the old paint-until-the-edge rule.
#[test]
fn ux_shot_twenty_tabs_before() {
    use crate::frame_html::write_shot;
    let names: Vec<String> = (1..=20).map(|i| format!("task-{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let view = shot_view(
        (34, 150),
        vec![named_meta(1, "footnote", &refs, 13)],
        vec![],
    );
    // The pre-fix behaviour, reproduced from the unwindowed span list: paint
    // left to right and stop at the edge.
    let mut frame = view.compose();
    let cols = frame.cols as usize;
    for c in view.panel_w() as usize..cols {
        frame.cells[c] = Cell::default();
    }
    let mut c = view.panel_w() as usize;
    'spans: for span in view.tab_bar_spans() {
        for ch in span.text.chars() {
            if c >= cols {
                break 'spans;
            }
            frame.cells[c] = Cell {
                c: ch,
                fg: span.fg,
                bg: Color::Default,
                flags: span.flags,
            };
            c += 1;
        }
    }
    let painted = frame_text(&frame);
    let strip = painted.lines().next().unwrap_or_default();
    assert!(
        !strip.contains("task-20"),
        "the before fixture should CLIP: the old strip could not reach tab 20"
    );
    write_shot(
        &frame,
        "00-twenty-tabs-before",
        "twenty tabs (before: clipped)",
    );
}

// (x-aeab) The court block reserves the bottom rows of the sideline at the
// one subtraction point, and yields entirely when the terminal cannot hold
// it beside at least one row.
#[test]
fn the_court_block_shrinks_the_sideline_and_yields_when_too_short() {
    let mut view = View::new(
        (24, 100),
        "main".into(),
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    assert!(view.court.take_want());
    view.court.apply(Some(crate::court_overlay::Court {
        lane_count: None,
        per_lane_cpu_cores: None,
        per_lane_mem_gb: None,
        cost_source: String::new(),
        refused_reason: String::new(),
        census: Default::default(),
        arms: Vec::new(),
    }));

    assert_eq!(view.court_block_rows(), 3, "minimized is three lines");
    let full = view.sideline_visible_rows() + view.court_block_rows();

    view.court.toggle();
    let expanded = view.court.expanded_lines(&view.agent_ages()).len();
    assert!(
        expanded > 3,
        "the empty reading's expanded render still exceeds the glance"
    );
    assert_eq!(view.court_block_rows(), expanded);
    assert_eq!(view.sideline_visible_rows(), full - expanded);

    // Too short: the block drops, the rows never do.
    view.term = (3, 100);
    assert_eq!(view.court_block_rows(), 0);
    assert_eq!(
        view.sideline_visible_rows(),
        3 - view.bottom_row_is_chrome() as usize
    );
}
