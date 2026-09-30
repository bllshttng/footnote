//! The feed panel's client-side tests (x-4433, x-f089): render order, the
//! hover marker, the click mapping, the focus mode, and the provenance view.
//! One `Reach` resolves a row, and the footer and the action both read it, so
//! a joined row yields exactly what `agent_hit` yields for that sideline row,
//! an unjoined row with a session id attaches on portal 0, and a row with no
//! session id offers nothing. The click resolver and the painter must invert
//! each other exactly.

use super::tests::{two_pane_view, view_with_agents};
use super::*;
use crate::client::feed_detail::{self, destination, Destination};
use crate::client::feed_view::{feed_panel_lines, feed_row_item, FeedOverlay};
use crate::proto::Reach;

const W: usize = 40;
const ROWS: usize = 10;

fn feed_item(node: Option<&str>, sid: Option<&str>) -> crate::feed_overlay::FeedItem {
    crate::feed_overlay::FeedItem {
        ts: "2026-09-02T18:27:06Z".into(),
        kind: "pr_created".into(),
        node: node.map(str::to_string),
        cwd: None,
        session_id: sid.map(str::to_string),
        harness: None,
        title: "PR 1395".into(),
        r#ref: Some("1395".into()),
        actor: None,
        model: None,
        effort: None,
        phase: None,
        detail: None,
        reason: None,
        crown: None,
        owner: None,
        parent: None,
        url: None,
    }
}

fn reaped_item(sid: &str, resume: &str) -> crate::feed_overlay::FeedItem {
    crate::feed_overlay::FeedItem {
        ts: "2026-09-06T10:00:00Z".into(),
        kind: "session_reaped".into(),
        node: None,
        cwd: None,
        session_id: Some(sid.into()),
        harness: Some("claude".into()),
        title: "t-d145 removed by reap".into(),
        r#ref: None,
        actor: None,
        model: None,
        effort: None,
        phase: None,
        detail: Some(resume.into()),
        reason: None,
        crown: None,
        owner: None,
        parent: None,
        url: None,
    }
}

fn overlay(items: Vec<crate::feed_overlay::FeedItem>) -> FeedOverlay {
    overlay_in(items, feed_view::FeedOrder::Grouped)
}

fn overlay_in(
    items: Vec<crate::feed_overlay::FeedItem>,
    order: feed_view::FeedOrder,
) -> FeedOverlay {
    let sel = feed_view::first_item_slot(&items, order);
    FeedOverlay {
        items,
        sel,
        error: None,
        inflight: false,
        want: false,
        gen: 0,
        focused: false,
        hpan: 0,
        last_fold: None,
        order,
    }
}

/// A pane-hosted row, so the joined case exercises agent_hit's FocusPane arm.
fn joined_row(name: &str, cwd_base: Option<&str>, pane: Option<u64>) -> AgentRow {
    AgentRow {
        spawned_by_name: None,
        lineage_reason: None,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        lineage_kind: None,
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
        liveness_measured_at: None,
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
        pr_session_short: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        crown_title: None,
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
    let o = {
        let mut a = feed_item(Some("x-a"), Some("s-1"));
        a.ts = "2026-09-02T16:27:06Z".into();
        let mut b = feed_item(Some("x-b"), Some("s-2"));
        b.ts = "2026-09-02T17:27:06Z".into();
        let mut c = feed_item(Some("x-c"), Some("s-3"));
        c.ts = "2026-09-02T18:27:06Z".into();
        overlay(vec![a, b, c])
    };
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    // The viewport is exact: header + ROWS-2 item rows + footer, so the
    // painter can blit 1:1 and short lists render blank below their last row.
    assert_eq!(lines.len(), ROWS);
    // Slot 0 is the `other` group header (no owners in this fixture); the
    // top ITEM row shows the newest event.
    assert!(
        lines[1].contains("other"),
        "group header first: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("x-c"),
        "top row shows the newest: {}",
        lines[2]
    );
    assert!(lines[2].starts_with(" ▸"));
    assert!(lines[3].contains("x-b"));
    assert!(lines[4].contains("x-a"));
    assert!(lines[5].trim().is_empty(), "below the last item: blank");
    assert!(lines.last().unwrap().contains("3 events"));
}

#[test]
fn offset_windows_the_items() {
    let o = {
        let mut a = feed_item(Some("x-a"), Some("s-1"));
        a.ts = "2026-09-02T16:27:06Z".into();
        let mut b = feed_item(Some("x-b"), Some("s-2"));
        b.ts = "2026-09-02T17:27:06Z".into();
        let mut c = feed_item(Some("x-c"), Some("s-3"));
        c.ts = "2026-09-02T18:27:06Z".into();
        overlay(vec![a, b, c])
    };
    // Slot offset 1 skips the group header, so the newest row (x-c) leads
    // the window now.
    let lines = feed_panel_lines(&o, W, ROWS, 1);
    assert!(lines[1].contains("x-c"));
    assert!(lines[2].contains("x-b"));
    assert!(lines[3].contains("x-a"));
    assert!(lines[4].trim().is_empty());
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
    // The modal's session row carries the action, resolved once at open.
    let row = joined_row("worker-01", Some("x-9223"), Some(7));
    let item = feed_item(Some("x-9223"), Some("s-ghost"));
    let (_, actions, _) = feed_detail::build(&[row], 0, &item);
    let expected = agent_hit(&joined_row("worker-01", Some("x-9223"), Some(7)), 0);
    let joined = actions.iter().find_map(|a| match a {
        feed_detail::FeedAction::Session(hit) => Some(hit.clone()),
        _ => None,
    });
    assert_eq!(actions.len(), 3, "node, session-id and pane are actions");
    // ChromeHit carries no Debug/PartialEq; the two shapes that matter here.
    match (joined, expected) {
        (Some(ChromeHit::Cmds(a)), ChromeHit::Cmds(b)) => assert_eq!(a, b),
        _ => panic!("both hits must be Cmds"),
    }
}

#[test]
fn hit_on_a_row_without_session_id_is_none() {
    let item = feed_item(Some("x-nope"), None);
    let (_, actions, _) = feed_detail::build(&[], 0, &item);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, feed_detail::FeedAction::Session(_))),
        "no session, no session action"
    );
}

#[tokio::test]
async fn a_created_row_without_node_offers_no_deep_link_or_blueprint_composer() {
    let v = view_with_rows(vec![]);
    let mut item = feed_item(None, None);
    item.kind = "node_created".into();
    let (_, actions, _) = feed_detail::build(&v.layout.agents, 0, &item);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, feed_detail::FeedAction::Node(_))),
        "a node-less created row offers no node action"
    );

    let mut v = v;
    v.feed_detail = Some(feed_detail::modal(&v, item));
    let (mut writer, _reader) = tokio::io::duplex(4096);
    feed_view::feed_keys(&mut v, b"b", &mut writer)
        .await
        .unwrap();
    assert!(
        v.launcher.is_none(),
        "missing node id cannot prefill the composer"
    );
    assert!(v.feed_detail.is_some(), "an ineligible detail stays open");
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
    // Row 0 is the panel header, the last row is the footer; painted row 1 is
    // the group HEADER (no detail), row 2 the first item. The resolver reads
    // the same slot list the painter drew.
    // Distinct timestamps, so the slot order is total: newest first.
    let items: Vec<_> = (0..3)
        .map(|i| {
            let mut it = feed_item(Some("x-n"), Some("s-n"));
            it.title = format!("event {i}");
            it.ts = format!("2026-09-02T1{i}:27:06Z");
            it
        })
        .collect();
    let g = feed_view::FeedOrder::Grouped;
    assert_eq!(feed_row_item(&items, 0, ROWS, 0, g), None, "panel header");
    assert_eq!(feed_row_item(&items, ROWS - 1, ROWS, 0, g), None, "footer");
    assert_eq!(
        feed_row_item(&items, 1, ROWS, 0, g),
        None,
        "a group header never opens a detail"
    );
    assert_eq!(feed_row_item(&items, 2, ROWS, 0, g), Some(2));
    assert_eq!(
        feed_row_item(&items, 5, ROWS, 0, g),
        None,
        "past the last item: blank"
    );
    // Offset 2 scrolls the header and the newest row off: painted row 2 is
    // now the OLDEST row (slot 3 = storage 0).
    assert_eq!(
        feed_row_item(&items, 2, ROWS, 2, g),
        Some(0),
        "offset applies"
    );
}

#[test]
fn a_click_on_a_feed_row_opens_that_rows_provenance() {
    // The full client path: feed open, click in the panel's item rows at the
    // geometry the painter draws, hit = that row's provenance view. The deep
    // link moved to that view's own action; a click inspects, never attaches.
    let mut v = view_with_rows(vec![]);
    let mut a = feed_item(Some("x-a"), Some("s-1"));
    a.ts = "2026-09-02T16:27:06Z".into();
    let mut b = feed_item(Some("x-b"), Some("s-2"));
    b.ts = "2026-09-02T17:27:06Z".into();
    let mut c = feed_item(Some("x-c"), Some("s-3"));
    c.ts = "2026-09-02T18:27:06Z".into();
    v.feed = Some(overlay(vec![a, b, c]));
    let w = v.feed_panel_w() as u16;
    assert!(w > 0, "the panel must render at the 100-col view");
    let col = v.term.1 - w + 2; // inside the panel text area
                                // The TOP item row displays the newest event (x-c) and clicking that
                                // same row deep-links THAT event's session: painter and resolver must
                                // name one event, never two.
    let f = v.feed.as_ref().unwrap();
    let lines = feed_panel_lines(f, w as usize - 1, v.term.0 as usize, 0);
    assert!(
        lines[2].contains("x-c"),
        "top ITEM row is the newest: {}",
        lines[2]
    );
    // The header row never deep-links; the item row under it does.
    assert!(v.chrome_hit(1, col).is_none(), "header row is chrome");
    let hit = v.chrome_hit(2, col).unwrap();
    assert!(
        matches!(&hit, ChromeHit::OpenFeedDetail(item) if item.session_id.as_deref() == Some("s-3")),
        "the click names the event the top row painted"
    );
    // And THAT modal's session row carries the same deep link the click used
    // to fire, resolved against the roster at open.
    v.feed_detail = Some(feed_detail::modal(&v, feed_item(Some("x-c"), Some("s-3"))));
    let m = v.feed_detail.as_ref().unwrap();
    assert!(m
        .actions
        .iter()
        .any(|a| matches!(a, feed_detail::FeedAction::Session(
            ChromeHit::Cmds(c)
        ) if *c == vec![Command::AttachAgent {
            id: "s-3".into(),
            placement: PanePlacement { portal: Some(0), ..Default::default() },
        }])));
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
    assert_eq!(v.feed_offset, 11, "the clamp counts the group header");
    v.scroll_feed(false);
    assert_eq!(v.feed_offset, 10);
}

#[test]
fn terminal_growth_reclamps_the_window() {
    let mut v = two_pane_view(); // 30 rows
    let mut items = overlay(vec![]).items;
    for i in 0..20 {
        let mut it = feed_item(Some("x-n"), Some("s-n"));
        it.title = format!("event {i}");
        items.push(it);
    }
    v.feed = Some(overlay(items));
    // Scroll to the bottom, then grow the terminal: everything fits, so the
    // window reopens at the newest row instead of parking on blanks.
    for _ in 0..40 {
        v.scroll_feed(true);
    }
    v.term = (60, 100);
    v.scroll_feed(false);
    assert_eq!(v.feed_offset, 0, "scroll re-clamps on growth");
    v.feed_offset = 7; // a stale offset must not reach paint/click either
    assert_eq!(v.feed_offset_clamped(), 0);
    let cells = v.compose().cells;
    assert!(
        cells.iter().any(|c| c.c == 'x'),
        "the panel paints items, not blanks"
    );
}

#[test]
fn a_double_width_glyph_claims_two_cells() {
    let mut v = two_pane_view();
    let mut it = feed_item(Some("x-cjk"), Some("s-cjk"));
    it.title = "世界".into(); // two CJK glyphs, each display width 2
    v.feed = Some(overlay(vec![it]));
    // A wide panel so the title column lands inside the text columns.
    v.term = (30, 200);
    v.feed_width = 80;
    let (rows, cols) = (v.term.0 as usize, v.term.1 as usize);
    let mut cells = vec![Cell::default(); rows * cols];
    v.draw_feed_panel(&mut cells, rows, cols);
    // Find the wide glyph's cell; its right neighbor must be a WIDE_SPACER.
    let wide = cells
        .iter()
        .position(|c| c.c == '\u{4e16}')
        .expect("the CJK glyph paints");
    let spacer = cells
        .get(wide + 1)
        .expect("a right neighbor exists inside the frame");
    assert!(
        spacer.flags & cell_flags::WIDE_SPACER != 0,
        "the wide glyph reserves both cells"
    );
}

// (AC4) The narrowed invariant, both halves. An unfocused panel takes no
// keys, so the header says how to focus and the marker does not move; a
// focused panel takes the arrows and says so.
#[test]
fn the_header_names_the_input_state_the_panel_is_in() {
    let unfocused = overlay(vec![feed_item(Some("x-a"), Some("s-1"))]);
    let lines = feed_panel_lines(&unfocused, W, ROWS, 0);
    assert!(
        lines[0].contains("E focus"),
        "unfocused header: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("details"),
        "unfocused header: {}",
        lines[0]
    );

    let mut focused = overlay(vec![feed_item(Some("x-a"), Some("s-1"))]);
    focused.focused = true;
    let lines = feed_panel_lines(&focused, W, ROWS, 0);
    assert!(lines[0].contains("FOCUSED"), "focused header: {}", lines[0]);
    assert!(lines[0].contains("esc release"));

    // The focus key is advertised nowhere else, so it survives every width
    // the border can be dragged to rather than being clipped off the end.
    for w in 30..90usize {
        assert!(
            feed_view::header_line(false, feed_view::FeedOrder::Grouped, w).contains("E focus"),
            "the focus key vanished at width {w}"
        );
        assert!(
            unicode_width::UnicodeWidthStr::width(
                feed_view::header_line(false, feed_view::FeedOrder::Grouped, w).as_str(),
            ) <= w
                || w < 32,
            "header overflows at width {w}"
        );
    }
}

// (AC4-EDGE) Esc releases the keyboard and leaves the panel open, so the very
// next byte reaches the pane again.
#[test]
fn esc_releases_the_keyboard_without_closing_the_panel() {
    let mut v = view_with_rows(vec![]);
    v.feed = Some(overlay(vec![feed_item(Some("x-a"), Some("s-1"))]));
    v.feed.as_mut().unwrap().focused = true;
    assert!(crate::client::feed_view::release(&mut v));
    assert!(v.feed.is_some(), "the panel stays open");
    assert!(!v.feed.as_ref().unwrap().focused);
    // Releasing twice is a no-op, never a close.
    assert!(!crate::client::feed_view::release(&mut v));
    assert!(v.feed.is_some());
}

// The pan moves the TITLE only, in display columns, and never splits a wide
// glyph: the stamp, kind and node stay anchored so a panned row is still the
// row that was selected.
#[test]
fn a_pan_moves_the_title_by_display_columns() {
    assert_eq!(feed_view::pan_by("abcdef", 0), "abcdef");
    assert_eq!(feed_view::pan_by("abcdef", 2), "cdef");
    // A two-column glyph straddling the cut is dropped whole, never halved.
    assert_eq!(feed_view::pan_by("漢字ab", 1), "字ab");
    assert_eq!(feed_view::pan_by("abc", 99), "");
    assert_eq!(
        feed_view::widest_title(&[feed_item(None, None), reaped_item("s", "r")]),
        "t-d145 removed by reap".len()
    );
}

// (AC5-HP) A removal reads as a normal outcome carrying its recovery line,
// never as a bare attach the server refuses. (x-1b90) Its pane field is a
// measurement question, not an applicability question: NOT RECORDED names
// that the removal did not measure the pane - whatever the recovery line
// says, since no removal record carries the measurement on a field yet.
#[test]
fn a_reaped_row_reads_as_a_good_outcome_with_its_resume_line() {
    use crate::client::feed_detail;
    let item = reaped_item(
        "00847995-e0db-47c2-ab5b-24468ba1a4f5",
        "resume: claude --resume x",
    );
    let (popup, actions, values) = feed_detail::build(&[], 0, &item);
    // A removal measures no pane: the row hides rather than printing a
    // NOT RECORDED stand-in for a measurement no record carries.
    let labels: Vec<String> = popup_rows(&popup)
        .iter()
        .filter_map(|(l, _)| l.clone())
        .collect();
    assert!(!labels.iter().any(|l| l == "pane"), "{labels:?}");
    assert!(
        !popup_lines(&popup)
            .iter()
            .any(|l| l.contains("NOT RECORDED")),
        "an absent field prints nothing: {labels:?}"
    );
    // The recovery line is its own row: Enter hands it over as a notice,
    // never an attach of a session that is gone.
    let i = actions
        .iter()
        .position(
            |a| matches!(a, feed_detail::FeedAction::Resume(d) if d == "resume: claude --resume x"),
        )
        .expect("the resume row exists");
    assert_eq!(values[i], "resume: claude --resume x");
    assert!(labels.iter().any(|l| l == "resume"), "{labels:?}");
}

/// The popup's rendered body lines, for label-level assertions.
fn popup_lines(popup: &crate::popup::Popup) -> Vec<String> {
    popup
        .render((40, 200))
        .lines
        .iter()
        .map(|l| l.text.clone())
        .collect()
}

/// The popup's rendered (label, value) field pairs: Entry rows key on the
/// glyph column, Info rows carry label and value.
fn popup_rows(popup: &crate::popup::Popup) -> Vec<(Option<String>, String)> {
    let mut rows = Vec::new();
    for line in popup.render((40, 200)).lines {
        let text = line.text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let mut parts = text.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("").to_string();
        let tail = parts.next().unwrap_or("").trim().to_string();
        rows.push((Some(head), tail));
    }
    rows
}

// (AC5-EDGE) Pane ids allocate from zero, so pane 0 is a real seat. The join
// is on the exact session id, never the row name a later worker can reuse.
#[test]
fn a_live_row_at_pane_zero_reports_its_seat_and_resolves_its_focus() {
    use crate::client::feed_detail;
    let mut row = joined_row("some-other-name", None, Some(0));
    row.harness_session_id = Some("s-9".into());
    row.portal = Some(0);
    row.spawned_by_session = Some("s-parent".into());
    row.crown_scope = Some("e-0001".into());
    row.crown_level = Some(1);

    let item = feed_item(Some("x-a"), Some("s-9"));
    let rows = [row];
    let d = destination(&rows, &item);
    assert!(
        matches!(d, Destination::Exact(_)),
        "joined on the session id"
    );
    let (popup, _, _) = feed_detail::build(&rows, 0, &item);
    let by = |label: &str| -> String {
        popup_rows(&popup)
            .into_iter()
            .find(|(l, _)| l.as_deref() == Some(label))
            .map(|(_, v)| v)
            .unwrap_or_default()
    };
    assert_eq!(by("pane"), "pane 0 · portal 0");
    assert_eq!(by("parent"), "s-parent");
    assert_eq!(by("lead"), "L1 e-0001");

    // A row whose session id does not match is NOT this event's session,
    // however its name reads: parent and king stay unrecorded, and the pane
    // says whose seat it actually is.
    let other = feed_item(Some("some-other-name"), Some("s-someone-else"));
    let other_dest = destination(&rows, &other);
    assert!(matches!(other_dest, Destination::NameOnly(_)));
    let (popup2, _, _) = feed_detail::build(&rows, 0, &other);
    let pane2 = popup_rows(&popup2)
        .into_iter()
        .find(|(l, _)| l.as_deref() == Some("pane"))
        .map(|(_, v)| v)
        .unwrap_or_default();
    // A name join reaches the node's CURRENT worker, and the pane says so;
    // the unjoined facts stay hidden rather than printed as silences.
    assert!(pane2.contains("the node's current worker"), "{pane2}");
    assert!(
        !popup_lines(&popup2)
            .iter()
            .any(|l| l.contains("NOT RECORDED")),
        "absent fields print nothing"
    );
}

// The panel drags narrower than any prose fits, and the caller clips from the
// end. The key must survive that clip: it is the only place it is advertised.
#[test]
fn the_header_keeps_its_key_at_every_draggable_width() {
    use crate::client::feed_view::{header_line, FeedOrder};
    for w in 0..=80usize {
        let unfocused = header_line(false, FeedOrder::Grouped, w);
        assert!(
            unfocused.starts_with(" E focus")
                || unicode_width::UnicodeWidthStr::width(unfocused.as_str()) <= w,
            "w={w} picked {unfocused:?}"
        );
        let focused = header_line(true, FeedOrder::Grouped, w);
        assert!(
            focused.starts_with(" esc release")
                || unicode_width::UnicodeWidthStr::width(focused.as_str()) <= w,
            "w={w} picked {focused:?}"
        );
    }
    // Below every prose spelling, the fallback leads with the key, so an
    // 8-column clip still reads "E focus" rather than a truncated label.
    assert_eq!(header_line(false, FeedOrder::Grouped, 8), " E focus");
    assert!(header_line(true, FeedOrder::Grouped, 8).starts_with(" esc"));
}

// A node_created modal hides what the source lacks and prints what the
// birth stamped: no NOT RECORDED filler anywhere, ever.
#[test]
fn a_node_created_modal_hides_its_unrecorded_fields() {
    let mut bare = feed_item(Some("x-a"), Some("s-1"));
    bare.kind = "node_created".into();
    let (popup, _, _) = feed_detail::build(&[], 0, &bare);
    let lines = popup_lines(&popup);
    assert!(
        !lines.iter().any(|l| l.contains("NOT RECORDED")),
        "an absent field prints nothing: {lines:?}"
    );
    let labels: Vec<String> = popup_rows(&popup)
        .into_iter()
        .filter_map(|(l, _)| l)
        .collect();
    assert!(!labels.iter().any(|l| l == "model"), "{labels:?}");
    assert!(!labels.iter().any(|l| l == "parent"), "{labels:?}");
    assert!(!labels.iter().any(|l| l == "crown"), "{labels:?}");

    // The birth stamps ride the row: model, effort, parent and crown print
    // when the creating session's registry row carried them.
    let mut stamped = feed_item(Some("x-a"), Some("s-1"));
    stamped.kind = "node_created".into();
    stamped.model = Some("glm-5.3-flash".into());
    stamped.effort = Some("high".into());
    stamped.parent = Some("s-parent".into());
    stamped.crown = Some("L2 e-0001".into());
    let (popup, _, _) = feed_detail::build(&[], 0, &stamped);
    let by = |label: &str| -> String {
        popup_rows(&popup)
            .into_iter()
            .find(|(l, _)| l.as_deref() == Some(label))
            .map(|(_, v)| v)
            .unwrap_or_default()
    };
    assert_eq!(by("model"), "glm-5.3-flash");
    assert_eq!(by("effort"), "high");
    assert_eq!(by("parent"), "s-parent");
    assert_eq!(by("crown"), "L2 e-0001");
}

// The whole render path, not just the line builder: open the provenance view
// on a real View and read the COMPOSED frame. A field that never reaches the
// screen is the defect this view exists to prevent, so the assertion is on
// painted text.
#[tokio::test]
async fn the_composed_frame_paints_every_field_and_opens_the_blueprint_composer() {
    let mut v = view_with_rows(vec![]);
    v.term = (44, 120);
    v.feed = Some(overlay(vec![feed_item(Some("x-a"), Some("s-1"))]));
    let mut item = feed_item(Some("x-9223"), Some("s-1"));
    v.feed_detail = Some(feed_detail::modal(&v, item.clone()));
    let ordinary_text = crate::vt::frame_text(&v.compose());
    assert!(
        !ordinary_text.contains("b: blueprint"),
        "non-created rows have no blueprint action"
    );
    item.kind = "node_created".into();
    item.cwd = Some("/workspace/node-project".into());
    item.harness = Some("claude".into());
    item.model = Some("glm-5.3-flash".into());
    v.feed_detail = Some(feed_detail::modal(&v, item));

    let text = crate::vt::frame_text(&v.compose());
    for label in [
        "harness",
        "timestamp",
        "model",
        "node",
        "session-id",
        "pane",
    ] {
        assert!(text.contains(label), "the frame never painted {label}");
    }
    // The values that ARE recorded print; the ones that are not hide.
    assert!(text.contains("glm-5.3-flash"));
    assert!(text.contains("x-9223"));
    assert!(
        !text.contains("NOT RECORDED"),
        "an absent field paints nothing"
    );
    // The footer names the gestures before they are pressed, including the
    // created-node composer key.
    assert!(text.contains("enter open"), "footer missing");
    assert!(text.contains("y copy"), "copy affordance missing");
    assert!(text.contains("b blueprint"), "blueprint key missing");

    let (mut writer, _reader) = tokio::io::duplex(4096);
    feed_view::feed_keys(&mut v, b"b", &mut writer)
        .await
        .unwrap();
    let launch = v.launcher.as_ref().expect("blueprint key opens composer");
    assert_eq!(launch.draft.message, "/fno:blueprint x-9223");
    assert_eq!(launch.draft.node.as_deref(), Some("x-9223"));
    assert_eq!(
        launch
            .draft
            .projects
            .get(launch.draft.project_idx)
            .map(String::as_str),
        Some("/workspace/node-project")
    );
    assert!(v.feed_detail.is_none(), "composer replaces feed detail");
}

// (x-9cbf) The parent field spends the derived NAME when the edge resolves
// to a row in the set, keeps the handoff word on a PEER edge, shows the bare
// session id when the edge names no row, and falls back to the birth's own
// reason (or the honest silence) when the row has no edge at all.
#[test]
fn the_parent_field_names_the_parent_row_when_the_edge_resolves() {
    let item = feed_item(Some("x-a"), Some("s-1"));
    let mut child = joined_row("jn-t-x-1", None, None);
    child.harness_session_id = Some("s-1".into());
    child.spawned_by_session = Some("s-lead".into());
    let parent_name = "t-x-lead";
    let by_parent = |row: &AgentRow| -> String {
        let (popup, _, _) = feed_detail::build(&[row.clone()], 0, &item);
        popup_rows(&popup)
            .into_iter()
            .find(|(l, _)| l.as_deref() == Some("parent"))
            .map(|(_, v)| v)
            .unwrap_or_default()
    };

    // Child edge resolving to a row: "<name> (<session>)".
    child.spawned_by_name = Some(parent_name.into());
    child.lineage_kind = Some("child".into());
    assert_eq!(by_parent(&child), "t-x-lead (s-lead)");

    // Peer edge: the same answer, plus the handoff word.
    child.lineage_kind = Some("peer".into());
    assert_eq!(by_parent(&child), "t-x-lead (s-lead) (handoff)");

    // Edge present, name absent (a session no row holds): the bare id, and
    // never a fabricated or borrowed name.
    child.spawned_by_name = None;
    child.lineage_kind = Some("child".into());
    assert_eq!(by_parent(&child), "s-lead");
    child.lineage_kind = Some("peer".into());
    assert_eq!(by_parent(&child), "s-lead (handoff)");

    // No edge: the birth's reason stands in for the parent it could not name.
    child.spawned_by_session = None;
    child.lineage_kind = None;
    child.lineage_reason = Some("daemon mint: spawn request carried no parent edge".into());
    assert_eq!(
        by_parent(&child),
        "daemon mint: spawn request carried no parent edge"
    );

    // No edge and no reason: the field hides (item.parent rides in its
    // place when the birth stamped one).
    child.lineage_reason = None;
    let (popup, _, _) = feed_detail::build(&[child], 0, &item);
    let labels: Vec<String> = popup_rows(&popup)
        .into_iter()
        .filter_map(|(l, _)| l)
        .collect();
    assert!(!labels.iter().any(|l| l == "parent"), "{labels:?}");
}

// (AC7-HP) The crowns band leads, then one header per owner ordered by
// its newest row, then `other`; a header click resolves to no detail.
#[test]
fn display_slots_group_the_rows() {
    let mut crown = feed_item(None, None);
    crown.kind = "crown_vacated".into();
    crown.ts = "2026-09-28T16:45:58Z".into();
    crown.title = "warden left L2 e: succession".into();
    let mut owned_a = feed_item(Some("x-a"), None);
    owned_a.owner = Some("epic x-29a8 the epic".into());
    owned_a.ts = "2026-09-28T17:00:00Z".into();
    let mut owned_b = feed_item(Some("x-b"), None);
    owned_b.owner = Some("epic x-29a8 the epic".into());
    owned_b.ts = "2026-09-28T17:30:00Z".into();
    let mut loose = feed_item(Some("x-c"), None);
    loose.ts = "2026-09-28T18:00:00Z".into();
    let items = vec![loose, owned_a, owned_b, crown];
    let slots = feed_view::display_slots(&items, feed_view::FeedOrder::Grouped);
    // Slot shapes: crowns header + the crown row, the owner header with
    // its rows newest first, then the other header with the loose row.
    let shape: Vec<String> = slots
        .iter()
        .map(|s| match s {
            feed_view::Slot::Header(h) => format!("H:{h}"),
            feed_view::Slot::Item(i) => format!("I:{}", items[*i].node.as_deref().unwrap_or("?")),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "H:crowns",
            "I:?",
            "H:epic x-29a8 the epic",
            "I:x-b",
            "I:x-a",
            "H:other",
            "I:x-c",
        ],
        "{shape:?}"
    );
    // A header row never resolves to a detail.
    let g = feed_view::FeedOrder::Grouped;
    assert_eq!(feed_row_item(&items, 1, ROWS, 0, g), None, "crowns header");
    // The first item row IS the crown row.
    assert_eq!(feed_row_item(&items, 2, ROWS, 0, g), Some(3));
}

// (AC8-HP) The kind span goes bold in the brand colour for kinds that
// need action; the flattened text is what the panel always drew.
#[test]
fn feed_panel_rows_style_the_actionable_kinds() {
    let mut vacated = feed_item(Some("x-a"), None);
    vacated.kind = "crown_vacated".into();
    vacated.ts = "2026-09-28T16:45:58Z".into();
    vacated.title = "warden left".into();
    let o = overlay(vec![vacated]);
    let rows = feed_view::feed_panel_rows(&o, W, ROWS, 0);
    // Row 1 is the group header: every span bold.
    assert!(
        rows[1].iter().all(|s| s.bold),
        "headers render bold: {:?}",
        rows[1]
    );
    // Row 2 is the vacated row: its kind span is bold AND brand.
    assert!(
        rows[2]
            .iter()
            .any(|s| s.bold && s.brand && s.text.contains("crown_vacated")),
        "the actionable kind is bold brand: {:?}",
        rows[2]
    );
    assert!(
        rows[2].iter().any(|s| s.bold && s.text == "x-a"),
        "the node id is bold: {:?}",
        rows[2]
    );
    let lines = feed_panel_lines(&o, W, ROWS, 0);
    assert!(
        lines[2].contains("crown_vacated") && lines[2].contains("warden left"),
        "text is unchanged: {}",
        lines[2]
    );
}

// (AC9-HP) Local time: a UTC stamp renders in the zone the test pins.
#[test]
fn short_ts_renders_local_time() {
    let tz = chrono::FixedOffset::east_opt(-7 * 3600).unwrap();
    assert_eq!(feed_view::short_ts_in("2026-09-28T16:48:49Z", &tz), "09:48");
    assert_eq!(
        feed_view::short_ts_in("not-a-time", &tz),
        "not-a-time",
        "unparseable stamps show raw"
    );
}

// (AC10-EDGE) A settled fold goes stale and refolds on its own; the
// operator's selected row survives the refresh.
#[tokio::test]
async fn a_stale_fold_refolds_and_keeps_the_selection() {
    let mut v = view_with_rows(vec![]);
    let mut it = feed_item(Some("x-a"), Some("s-1"));
    it.ts = "2026-09-28T16:00:00Z".into();
    let mut other = feed_item(Some("x-b"), Some("s-2"));
    other.ts = "2026-09-28T17:00:00Z".into();
    v.feed = Some(overlay(vec![it.clone(), other.clone()]));
    v.feed.as_mut().unwrap().last_fold =
        Some(Instant::now() - feed_view::FEED_REFRESH_EVERY - std::time::Duration::from_secs(1));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    feed_view::maybe_kick(&mut v, &tx);
    assert!(
        v.feed.as_ref().unwrap().inflight,
        "a stale fold arms the single-flight"
    );
    // Selection kept across a refresh: select the OLDER row (slot 2),
    // refresh, and the same (ts, kind, title) stays selected.
    v.feed.as_mut().unwrap().sel = 2;
    let gen = v.feed.as_ref().unwrap().gen;
    feed_view::apply_fold(&mut v, gen, Ok(vec![other, it]));
    let f = v.feed.as_ref().unwrap();
    assert_eq!(f.sel, 2, "the selected row survived the fold");
    assert!(f.last_fold.is_some(), "the fold stamped its time");
    let _ = rx;
}

// (x-182e) The order toggle: `Recent` is one flat newest-first list with no
// headers; `Grouped` stays the shipped shape. The click resolver inverts the
// painter in BOTH orders, so a row and its detail can never disagree.
#[test]
fn the_recent_order_flattens_the_group_headers() {
    let mut a = feed_item(Some("x-a"), None);
    a.ts = "2026-09-28T17:00:00Z".into();
    let mut b = feed_item(Some("x-b"), None);
    b.ts = "2026-09-28T18:00:00Z".into();
    let items = vec![a, b];
    // Grouped: the unowned rows sit under the `other` header.
    let grouped = feed_view::display_slots(&items, feed_view::FeedOrder::Grouped);
    assert!(matches!(grouped.first(), Some(feed_view::Slot::Header(_))));
    // Recent: no headers at all, newest first.
    let recent = feed_view::display_slots(&items, feed_view::FeedOrder::Recent);
    assert!(recent.iter().all(|s| matches!(s, feed_view::Slot::Item(_))));
    let first = match &recent[0] {
        feed_view::Slot::Item(i) => items[*i].node.clone().unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(first, "x-b", "newest first");
    // The resolver answers the same item the painter drew, in Recent too.
    // Painted row 1 is the first item row (row 0 is the panel header).
    assert_eq!(
        feed_row_item(&items, 1, ROWS, 0, feed_view::FeedOrder::Recent),
        Some(1),
        "painted row 1 is storage 1 (the newest)"
    );
}

// (x-182e) A question row answers from the feed: the click opens the whole
// question on the questions view's own path, never a provenance detour.
#[test]
fn a_question_row_answers_from_the_feed() {
    let mut v = view_with_rows(vec![]);
    let mut q = feed_item(None, None);
    q.kind = "question_asked".into();
    q.r#ref = Some("q-1".into());
    q.ts = "2026-09-28T19:00:00Z".into();
    let mut older = feed_item(Some("x-a"), Some("s-1"));
    older.ts = "2026-09-28T18:00:00Z".into();
    v.feed = Some(overlay(vec![q, older]));
    let w = v.feed_panel_w() as u16;
    let col = v.term.1 - w + 2;
    let hit = v.chrome_hit(2, col).expect("the question row deep-links");
    assert!(
        matches!(&hit, ChromeHit::OpenQuestionDetail(id) if id == "q-1"),
        "the question row opens the question, not the provenance: {hit:?}"
    );
}

// (x-182e) The owner line resolves its holder against the live roster: a
// dead crown's handle says it is gone, a live one keeps the line, and a
// line with no parenthesized holder passes through untouched.
#[test]
fn a_dead_owner_says_it_is_gone() {
    let mut item = feed_item(Some("x-a"), None);
    item.owner = Some("king jolly-finch (king-4d9b)".into());
    // Live holder: the line stands.
    let (popup, _, _) = feed_detail::build(&[joined_row("king-4d9b", None, Some(1))], 0, &item);
    let owner = popup_rows(&popup)
        .into_iter()
        .find(|(l, _)| l.as_deref() == Some("owner"))
        .map(|(_, v)| v)
        .unwrap();
    assert_eq!(owner, "king jolly-finch (king-4d9b)");
    // Dead holder: the modal says so instead of naming a current king that
    // is not there.
    let (popup, _, _) = feed_detail::build(&[], 0, &item);
    let owner = popup_rows(&popup)
        .into_iter()
        .find(|(l, _)| l.as_deref() == Some("owner"))
        .map(|(_, v)| v)
        .unwrap();
    assert_eq!(owner, "king jolly-finch (king-4d9b) · gone");
    // No holder to resolve: untouched.
    let mut plain = feed_item(Some("x-a"), None);
    plain.owner = Some("epic x-29a8 the epic".into());
    let (popup, _, _) = feed_detail::build(&[], 0, &plain);
    let owner = popup_rows(&popup)
        .into_iter()
        .find(|(l, _)| l.as_deref() == Some("owner"))
        .map(|(_, v)| v)
        .unwrap();
    assert_eq!(owner, "epic x-29a8 the epic");
}

