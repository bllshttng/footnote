//! The feed overlay's client-side tests (x-4433): render order, selection
//! marker, and the deep-link contract. A joined row resolves exactly what
//! `agent_hit` yields for that sideline row; an unjoined row with a session
//! id attaches it on portal 0; a row with no session id is not selectable.
//! Parent items resolve through the glob.

use super::tests::view_with_agents;
use super::*;
use crate::proto::Reach;

fn feed_item(node: Option<&str>, sid: Option<&str>) -> crate::feed_overlay::FeedItem {
    crate::feed_overlay::FeedItem {
        ts: "2026-09-02T18:27:06Z".into(),
        kind: "pr_created".into(),
        node: node.map(str::to_string),
        session_id: sid.map(str::to_string),
        harness: None,
        title: "PR 1395".into(),
        r#ref: Some("1395".into()),
    }
}

fn overlay(items: Vec<crate::feed_overlay::FeedItem>) -> FeedOverlay {
    FeedOverlay {
        items,
        sel: 0,
        degraded: false,
        inflight: false,
        want: false,
        gen: 0,
    }
}

/// A pane-hosted row, so the joined case exercises agent_hit's FocusPane arm.
fn joined_row(name: &str, cwd_base: Option<&str>, pane: Option<u64>) -> AgentRow {
    AgentRow {
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        harness_session_id: None,
        squad: None,
        name: name.into(),
        pane_id: pane,
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
        cwd_base: cwd_base.map(str::to_string),
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

fn view_with_rows(rows: Vec<AgentRow>) -> View {
    view_with_agents(rows)
}

#[test]
fn lines_render_newest_first_with_selection_marker() {
    let o = overlay(vec![
        feed_item(Some("x-a"), Some("s-1")),
        feed_item(Some("x-b"), Some("s-2")),
        feed_item(Some("x-c"), Some("s-3")),
    ]);
    let lines = feed_overlay_lines(&o);
    // One instruction line + one per row + one footer.
    assert_eq!(lines.len(), 5);
    // Newest first: the LAST item renders at the top, cursor on it.
    assert!(lines[1].starts_with(" ▸"));
    assert!(lines[3].starts_with("   "));
    assert!(lines.last().unwrap().contains("3 events"));
}

#[test]
fn degraded_footer_states_the_failure() {
    let mut o = overlay(vec![]);
    o.degraded = true;
    let lines = feed_overlay_lines(&o);
    assert!(lines
        .iter()
        .any(|l| l.contains("feed unavailable - fno agents feed failed")));
}

#[test]
fn hit_on_a_joined_row_equals_agent_hit_for_that_row() {
    // The cwd basename is the node id: the join the sideline itself uses.
    let v = view_with_rows(vec![joined_row("worker-01", Some("x-9223"), Some(7))]);
    let joined = feed_hit(&v, &feed_item(Some("x-9223"), Some("s-ghost"))).unwrap();
    let expected = {
        let r = &v.layout.agents[0];
        agent_hit(r, v.layout.active_squad)
    };
    // ChromeHit carries no Debug/PartialEq; the two shapes that matter here.
    match (joined, expected) {
        (ChromeHit::Cmds(a), ChromeHit::Cmds(b)) => assert_eq!(a, b),
        _ => panic!("both hits must be Cmds"),
    }
}

#[test]
fn hit_on_an_unjoined_row_attaches_its_session() {
    let v = view_with_rows(vec![]);
    let hit = feed_hit(&v, &feed_item(Some("x-nope"), Some("s-ghost"))).unwrap();
    assert!(matches!(hit, ChromeHit::Cmds(c)
    if c == vec![Command::AttachAgent {
        id: "s-ghost".into(),
        placement: PanePlacement { portal: Some(0), ..Default::default() },
    }]));
}

#[test]
fn hit_on_a_row_without_session_id_is_none() {
    let v = view_with_rows(vec![]);
    assert!(feed_hit(&v, &feed_item(Some("x-nope"), None)).is_none());
}

#[test]
fn empty_overlay_renders_an_empty_notice_and_footer() {
    let o = overlay(vec![]);
    let lines = feed_overlay_lines(&o);
    assert!(lines.iter().any(|l| l.contains("no activity")));
    assert!(lines.last().unwrap().contains("0 events"));
}

#[test]
fn folding_first_open_claims_no_activity() {
    // While the fold is in flight the body claims nothing: "no activity" is
    // a statement only a settled fold has earned.
    let mut o = overlay(vec![]);
    o.inflight = true;
    let lines = feed_overlay_lines(&o);
    assert!(!lines.iter().any(|l| l.contains("no activity")));
    assert!(lines.iter().any(|l| l.contains("folding...")));
}
