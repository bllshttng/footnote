use super::*;

#[test]
fn nav_filter_text_is_case_insensitive_substring() {
    // AC2-HP + AC2-UI: typed text narrows to matching labels; case-folded.
    let v = two_pane_view();
    let nav = NavView {
        query: "NOTES".into(),
        state_filter: None,
        cursor: 0,
    };
    let rows = v.nav_filtered(&nav);
    assert_eq!(
        rows.len(),
        2,
        "notes squad + its one tab, footnote excluded"
    );
    assert!(rows
        .iter()
        .all(|r| r.label.to_lowercase().contains("notes")));
}

#[test]
fn nav_filter_state_composes_with_text() {
    // AC3-HP + AC3-EDGE: text AND state both apply. Squad/tab rows are Idle,
    // so a [blocked] chip leaves only the blocked agent.
    let mut v = two_pane_view();
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
        badge: Some(AgentBadge::Blocked),
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
    }];
    let composed = NavView {
        query: "notes".into(),
        state_filter: Some(PaneState::Blocked),
        cursor: 0,
    };
    let rows = v.nav_filtered(&composed);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "notes › stuck");
    assert_eq!(rows[0].state, PaneState::Blocked);
    let state_only = NavView {
        query: String::new(),
        state_filter: Some(PaneState::Blocked),
        cursor: 0,
    };
    assert_eq!(
        v.nav_filtered(&state_only).len(),
        1,
        "[blocked] excludes the Idle squad/tab rows"
    );
}

#[test]
fn nav_filter_matches_pane_id_node_id_and_slug() {
    // x-e10f AC1-HP/AC2-HP/AC3-HP: the query matches what the row IS, not
    // just its label. Pane 307 is the screenshot specimen (live, running
    // x-8a01, `find > 307` said `no matches`); x-6233 is the bound node;
    // the slug is the node's title-slug. All three find the pane's row.
    let mut v = two_pane_view();
    // A plain (non-agent) pane 307 in notes' first tab, plus an agent on
    // pane 11 working in-flight node x-6233 - both row classes must match.
    v.layout.squads[1].tabs[0].panes = vec![PaneMeta {
        id: 307,
        label: "shell".into(),
    }];
    v.layout.agents = vec![agent_row("claude", 11, None, false)];
    v.layout.backlog = vec![BacklogCard {
        id: "x-6233".into(),
        slug: "mux-navigator-matches-by-identity".into(),
        priority: "p1".into(),
        state: CardState::InFlight,
        pane_id: Some(11),
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    }];
    let rows = |q: &str| {
        let nav = NavView {
            query: q.into(),
            state_filter: None,
            cursor: 0,
        };
        v.nav_filtered(&nav)
    };
    let hits_pane = |r: &NavRow| {
        matches!(
            &r.hit,
            ChromeHit::Cmds(cs) if cs.iter().any(|c| matches!(c, Command::FocusPane(p) if *p == 307))
        )
    };
    assert!(
        rows("307").iter().any(hits_pane),
        "the plain pane row is found by its mux pane id, not its label"
    );
    assert!(
        rows("307").iter().all(|r| !r.label.contains("307")),
        "the hit is invisible in every matched label - it matched the key"
    );
    assert!(
        rows("x-6233").iter().any(|r| r.label.contains("claude")),
        "the agent row working x-6233 is found by node id via the pane join"
    );
    assert!(
        rows("matches-by-identity")
            .iter()
            .any(|r| r.label.contains("claude")),
        "the agent row is found by its bound node's title-slug, invisible in its label"
    );
    assert!(
        rows("notes").iter().any(|r| r.label == "notes"),
        "label substrings still match (the workspace half of the ask)"
    );
}

#[test]
fn nav_filter_matches_a_portal_index() {
    // x-0719 AC1-HP/AC2-UI/AC3-EDGE/AC4-REG: the portal index joins the
    // match key as a `portal:<n>` token, so an EXISTING portal is reachable
    // by number through the navigator that already exists. The token is
    // composed, never displayed (x-e10f): a hit renders the `·portal:2`
    // reason, a label hit renders bare, and a row shown through no portal
    // gains no token at all.
    let mut v = two_pane_view();
    let mut shown = agent_row("claude", 11, None, false);
    shown.portal = Some(2);
    v.layout.agents = vec![shown, agent_row("omega", 12, None, false)];
    let rows = v.nav_rows();
    let portal_row = rows.iter().find(|r| r.label.contains("claude")).unwrap();
    assert!(
        portal_row.match_key.contains("portal:2"),
        "the match key carries the portal index"
    );
    let plain_row = rows.iter().find(|r| r.label.contains("omega")).unwrap();
    assert!(
        !plain_row.match_key.contains("portal:"),
        "a row shown through no portal gains no stray token"
    );
    let q = |s: &str| NavView {
        query: s.into(),
        state_filter: None,
        cursor: 0,
    };
    let all = v.nav_filtered(&q(""));
    assert!(
        all.iter().any(|r| r.label.contains("omega")),
        "an empty query still lists the portal-less row"
    );
    let hits = v.nav_filtered(&q("portal:2"));
    assert_eq!(hits.len(), 1, "portal:2 finds exactly the portal-2 row");
    assert!(hits[0].label.contains("claude"));
    let lines = nav_overlay_lines(&hits, &q("portal:2"));
    assert!(
        lines.iter().any(|l| l.contains("·portal:2")),
        "an invisible-token hit names its reason"
    );
    let label_hits = v.nav_filtered(&q("claude"));
    let lines = nav_overlay_lines(&label_hits, &q("claude"));
    assert!(
        lines.iter().all(|l| !l.contains("·")),
        "a label hit renders bare, as x-e10f locked"
    );
}

#[test]
fn nav_rows_fold_done_through_the_seen_bit() {
    // AC1-HP/AC2-HP (x-4328), at the navigator seam: a seen Done row
    // folds to Idle (the unseen glyph clears); an unseen Done row stays
    // DoneUnseen (surfaced) - `nav_agent_state` must forward `a.seen`,
    // not hardcode it.
    let mut v = two_pane_view();
    v.layout.agents = vec![
        AgentRow {
            harness: None,
            model: None,
            route: None,
            reach: Reach::Locate,
            spawned_by_session: None,
            harness_session_id: None,
            squad: Some(2),
            name: "finished-seen".into(),
            pane_id: Some(9),
            portal: None,
            badge: Some(AgentBadge::Done),
            reason: None,
            exited: false,
            dnd: false,
            unmeasured: false,
            liveness_age_s: None,
            harness_title: None,
            answerable: None,
            attach_id: None,
            external: false,
            seen: true,
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
            squad: Some(2),
            name: "finished-unseen".into(),
            pane_id: Some(10),
            portal: None,
            badge: Some(AgentBadge::Done),
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
        },
    ];
    let rows = v.nav_rows();
    let seen_row = rows.iter().find(|r| r.label.ends_with("finished-seen"));
    let unseen_row = rows.iter().find(|r| r.label.ends_with("finished-unseen"));
    assert_eq!(seen_row.map(|r| r.state), Some(PaneState::Idle));
    assert_eq!(unseen_row.map(|r| r.state), Some(PaneState::DoneUnseen));
}

#[test]
fn nav_overlay_lines_show_query_chip_cursor_and_no_matches() {
    // AC1-UI: query line + [all] chip + cursor `▸` on row 0. AC2-ERR: an
    // empty filtered result renders `no matches`.
    let v = two_pane_view();
    let nav = NavView {
        query: String::new(),
        state_filter: None,
        cursor: 0,
    };
    let rows = v.nav_filtered(&nav);
    let lines = nav_overlay_lines(&rows, &nav);
    assert!(lines[0].contains("find ›") && lines[0].contains("[all]"));
    assert!(
        lines[1].trim_start().starts_with('▸'),
        "cursor on row 0: {:?}",
        lines[1]
    );
    let empty = NavView {
        query: "zzzz".into(),
        state_filter: None,
        cursor: 0,
    };
    let rows = v.nav_filtered(&empty);
    let lines = nav_overlay_lines(&rows, &empty);
    assert!(lines.iter().any(|l| l.contains("no matches")));
}

#[test]
fn nav_overlay_lines_show_the_matched_identity_token() {
    // x-e10f AC4-UI: a hit on an invisible identity token appends the
    // matched token as a `·<token>` suffix; a query that hits the visible
    // label appends nothing (the row renders exactly as before).
    let mut v = two_pane_view();
    v.layout.squads[1].tabs[0].panes = vec![PaneMeta {
        id: 307,
        label: "shell".into(),
    }];
    let unseen = NavView {
        query: "307".into(),
        state_filter: None,
        cursor: 0,
    };
    let rows = v.nav_filtered(&unseen);
    let lines = nav_overlay_lines(&rows, &unseen);
    let hit = lines
        .iter()
        .find(|l| l.contains("·307"))
        .expect("the pane-id hit names its reason");
    assert!(hit.contains("shell"), "the label still renders: {hit:?}");
    let visible = NavView {
        query: "shell".into(),
        state_filter: None,
        cursor: 0,
    };
    let rows = v.nav_filtered(&visible);
    let lines = nav_overlay_lines(&rows, &visible);
    assert!(
        lines.iter().all(|l| !l.contains("·")),
        "a label match appends no reason token"
    );
}
