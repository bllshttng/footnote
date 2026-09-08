//! The feed panel's client-side tests (x-4433, x-f089): render order, the
//! hover marker, the click mapping, and the deep-link contract. A joined row
//! resolves exactly what `agent_hit` yields for that sideline row; an
//! unjoined row with a session id attaches it on portal 0; a row with no
//! session id is not selectable. The panel is chrome: the click resolver and
//! the painter must invert each other exactly.

use super::tests::{two_pane_view, view_with_agents};
use super::*;
use crate::client::feed_view::FeedOverlay;
use crate::proto::Reach;

const W: usize = 40;
const ROWS: usize = 10;

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
        error: None,
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
fn lines_render_newest_first_with_marker() {
    let o = overlay(vec![
        feed_item(Some("x-a"), Some("s-1")),
        feed_item(Some("x-b"), Some("s-2")),
        feed_item(Some("x-c"), Some("s-3")),
    ]);
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    // The viewport is exact: header + ROWS-2 item rows + footer, so the
    // painter can blit 1:1 and short lists render blank below their last row.
    assert_eq!(lines.len(), ROWS);
    // Newest first: the LAST item renders at the top item row, marker on it.
    assert!(lines[1].starts_with(" ▸"));
    assert!(lines[2].starts_with("  "));
    assert!(lines[5].trim().is_empty(), "below the last item: blank");
    assert!(lines.last().unwrap().contains("3 events"));
}

#[test]
fn offset_windows_the_items() {
    let o = overlay(vec![
        feed_item(Some("x-a"), Some("s-1")),
        feed_item(Some("x-b"), Some("s-2")),
        feed_item(Some("x-c"), Some("s-3")),
    ]);
    // Display index 1 (the second newest) opens the window.
    let lines = feed_panel_lines(&o, W, ROWS, 1);
    assert!(lines[1].contains("x-b"));
    assert!(lines[2].contains("x-c"));
    assert!(lines[3].trim().is_empty());
}

#[test]
fn degraded_footer_renders_the_typed_reason() {
    // x-d15a: the failure line names the cause, never the old generic
    // "feed unavailable" sentence. Timeout names its budget; a malformed
    // body carries the projection's stderr so the cause leads the line.
    let mut o = overlay(vec![]);
    o.error = Some(crate::feed_overlay::FeedError::Timeout);
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    assert!(lines.iter().any(|l| l.contains("timed out after 10s")));

    let mut o = overlay(vec![]);
    o.error = Some(crate::feed_overlay::FeedError::Exit(
        "unreadable store: graph.json".into(),
    ));
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    // At the panel's 40 columns pad_to truncates the tail; the CAUSE still
    // leads the line (the x-d15a contract).
    assert!(lines.iter().any(|l| l.contains("feed exited non-zero")));
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
fn empty_panel_renders_an_earned_empty_notice_and_footer() {
    let o = overlay(vec![]);
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    assert!(lines.iter().any(|l| l.contains("no activity")));
    assert!(lines.last().unwrap().contains("0 events"));
}

#[test]
fn folding_first_open_claims_no_activity() {
    // While the fold is in flight the body claims nothing: "no activity" is
    // a statement only a settled fold has earned.
    let mut o = overlay(vec![]);
    o.inflight = true;
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    assert!(!lines.iter().any(|l| l.contains("no activity")));
    assert!(lines.iter().any(|l| l.contains("folding...")));
}

#[test]
fn click_resolver_inverts_the_painter() {
    // Row 0 is the header, the last row is the footer, and row n carries
    // display item n-1 exactly when the window opens on it.
    assert_eq!(feed_row_item(3, 0, ROWS, 0), None, "header");
    assert_eq!(feed_row_item(3, ROWS - 1, ROWS, 0), None, "footer");
    assert_eq!(feed_row_item(3, 1, ROWS, 0), Some(0));
    assert_eq!(
        feed_row_item(3, 4, ROWS, 0),
        None,
        "past the last item: blank"
    );
    assert_eq!(feed_row_item(3, 1, ROWS, 2), Some(2), "offset applies");
}

#[test]
fn a_click_on_a_feed_row_deep_links_it() {
    // The full client path: feed open, click in the panel's item rows at the
    // geometry the painter draws, hit = that row's deep link (the unjoined
    // attach, since the two-pane view has no agents).
    let mut v = view_with_rows(vec![]);
    v.feed = Some(overlay(vec![
        feed_item(Some("x-a"), Some("s-1")),
        feed_item(Some("x-b"), Some("s-2")),
        feed_item(Some("x-c"), Some("s-3")),
    ]));
    v.feed_offset = 2; // window opens on the OLDEST item (display index 2)
    let w = v.feed_panel_w() as u16;
    assert!(w > 0, "the panel must render at the 100-col view");
    let col = v.term.1 - w + 2; // inside the panel text area
    let hit = v.chrome_hit(1, col).unwrap();
    assert!(matches!(hit, ChromeHit::Cmds(c)
    if c == vec![Command::AttachAgent {
        id: "s-1".into(),
        placement: PanePlacement { portal: Some(0), ..Default::default() },
    }]));
    // Header and footer rows are chrome, not rows: they never deep-link.
    assert!(v.chrome_hit(0, col).is_none());
    assert!(v.chrome_hit((v.term.0 - 1) as u16, col).is_none());
}

#[test]
fn panel_width_is_transient_clamped_and_yields() {
    let mut v = two_pane_view(); // 30x100, Regular sideline at its canonical width
                                 // Closed: no panel, no columns.
    assert_eq!(v.feed_panel_w(), 0);
    v.feed = Some(overlay(vec![]));
    // 100 cols: sideline takes 28, the content floor (40) caps the feed at
    // 32 - the stored 40 is a TRANSIENT clamp, never mutated.
    let w = v.feed_panel_w();
    assert_eq!(w, 32);
    assert_eq!(v.feed_width, 40);
    // The content viewport subtracts both panels.
    assert_eq!(v.content_dims().1 as u16, 100 - v.panel_w() - w);
    // A tight terminal: the sideline auto-hides first (its own AC6-EDGE), then
    // the feed takes exactly what the content floor leaves it.
    v.term = (30, 50);
    assert_eq!(v.panel_w(), 0);
    assert_eq!(v.feed_panel_w(), 10);
    assert_eq!(v.content_dims().1, 40);
}

#[test]
fn drag_updates_width_and_release_persists() {
    let mut v = two_pane_view();
    v.feed = Some(overlay(vec![]));
    v.term = (30, 100);
    let now = Instant::now();
    // No drag in flight: no crossing.
    assert!(!v.drag_feed_to(60, now));
    v.feed_drag = Some(SidelineDrag {
        start_width: v.feed_width,
        last_at: now,
    });
    // Same column band: no crossing.
    assert!(!v.drag_feed_to(60, now));
    // Border to col 70: width 100-70 = 30, a real crossing.
    assert!(v.drag_feed_to(70, now));
    assert_eq!(v.feed_width, 30);
    // Esc reverts to the grab width and reports the change.
    assert!(v.revert_feed_drag());
    assert_eq!(v.feed_width, 40);
    assert!(!v.revert_feed_drag(), "no drag left to revert");
}

#[test]
fn wheel_scrolls_within_the_item_count() {
    let mut v = two_pane_view();
    v.feed = Some(overlay(vec![
        feed_item(Some("x-a"), Some("s-1")),
        feed_item(Some("x-b"), Some("s-2")),
    ]));
    let visible = v.term.0 as usize - 2;
    // Scrolling up from 0 stays at 0; down clamps at total - visible.
    v.scroll_feed(false);
    assert_eq!(v.feed_offset, 0);
    v.scroll_feed(true);
    assert_eq!(v.feed_offset, 0, "2 items never overflow the viewport");
    // Enough items to overflow: the offset clamps at the last full window.
    let many: Vec<_> = (0..visible + 10)
        .map(|i| {
            let mut it = feed_item(Some("x-n"), Some("s-n"));
            it.title = format!("event {i}");
            it
        })
        .collect();
    v.feed.as_mut().unwrap().items = many;
    for _ in 0..visible + 20 {
        v.scroll_feed(true);
    }
    assert_eq!(v.feed_offset, 10);
    v.scroll_feed(false);
    assert_eq!(v.feed_offset, 9);
}
