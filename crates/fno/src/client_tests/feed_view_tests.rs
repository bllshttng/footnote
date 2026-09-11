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
        session_id: sid.map(str::to_string),
        harness: None,
        title: "PR 1395".into(),
        r#ref: Some("1395".into()),
        actor: None,
        model: None,
        effort: None,
        phase: None,
        detail: None,
    }
}

fn reaped_item(sid: &str, resume: &str) -> crate::feed_overlay::FeedItem {
    crate::feed_overlay::FeedItem {
        ts: "2026-09-06T10:00:00Z".into(),
        kind: "session_reaped".into(),
        node: None,
        session_id: Some(sid.into()),
        harness: Some("claude".into()),
        title: "t-d145 removed by reap".into(),
        r#ref: None,
        actor: None,
        model: None,
        effort: None,
        phase: None,
        detail: Some(resume.into()),
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
        focused: false,
        hpan: 0,
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
    // Newest first: the projection hands rows oldest-first, so display
    // index d reads storage len-1-d and the top row is x-c, the newest.
    assert!(
        lines[1].contains("x-c"),
        "top row shows the newest: {}",
        lines[1]
    );
    assert!(lines[1].starts_with(" ▸"));
    assert!(lines[2].contains("x-b"));
    assert!(lines[3].contains("x-a"));
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
    // Display index 1 (the second newest) opens the window: the newest row
    // (x-c) is scrolled off, x-b leads, x-a follows.
    let lines = feed_panel_lines(&o, W, ROWS, 1);
    assert!(lines[1].contains("x-b"));
    assert!(lines[2].contains("x-a"));
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
    let item = feed_item(Some("x-9223"), Some("s-ghost"));
    let joined = feed_detail::detail_hit(&v, &destination(&v.layout.agents, &item)).unwrap();
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
    let item = feed_item(Some("x-nope"), Some("s-ghost"));
    let hit = feed_detail::detail_hit(&v, &destination(&v.layout.agents, &item)).unwrap();
    assert!(matches!(hit, ChromeHit::Cmds(c)
    if c == vec![Command::AttachAgent {
        id: "s-ghost".into(),
        placement: PanePlacement { portal: Some(0), ..Default::default() },
    }]));
}

#[test]
fn hit_on_a_row_without_session_id_is_none() {
    let v = view_with_rows(vec![]);
    let item = feed_item(Some("x-nope"), None);
    assert!(feed_detail::detail_hit(&v, &destination(&v.layout.agents, &item)).is_none());
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
fn a_click_on_a_feed_row_opens_that_rows_provenance() {
    // The full client path: feed open, click in the panel's item rows at the
    // geometry the painter draws, hit = that row's provenance view. The deep
    // link moved to that view's own action; a click inspects, never attaches.
    let mut v = view_with_rows(vec![]);
    v.feed = Some(overlay(vec![
        feed_item(Some("x-a"), Some("s-1")),
        feed_item(Some("x-b"), Some("s-2")),
        feed_item(Some("x-c"), Some("s-3")),
    ]));
    let w = v.feed_panel_w() as u16;
    assert!(w > 0, "the panel must render at the 100-col view");
    let col = v.term.1 - w + 2; // inside the panel text area
                                // The TOP item row displays the newest event (x-c) and clicking that
                                // same row deep-links THAT event's session: painter and resolver must
                                // name one event, never two.
    let f = v.feed.as_ref().unwrap();
    let lines = feed_panel_lines(f, w as usize - 1, v.term.0 as usize, 0);
    assert!(
        lines[1].contains("x-c"),
        "top row is the newest: {}",
        lines[1]
    );
    let hit = v.chrome_hit(1, col).unwrap();
    assert!(
        matches!(&hit, ChromeHit::OpenFeedDetail(item) if item.session_id.as_deref() == Some("s-3")),
        "the click names the event the top row painted"
    );
    // And THAT view's action is the deep link the click used to fire.
    v.feed_detail_of = Some(feed_item(Some("x-c"), Some("s-3")));
    assert!(matches!(v.feed_detail_hit(), Some(ChromeHit::Cmds(c))
    if c == vec![Command::AttachAgent {
        id: "s-3".into(),
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
            feed_view::header_line(false, w).contains("E focus"),
            "the focus key vanished at width {w}"
        );
        assert!(
            unicode_width::UnicodeWidthStr::width(feed_view::header_line(false, w)) <= w || w < 32,
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
    let d = destination(&[], &item);
    let fields = feed_detail::detail_fields(&item, &d);
    let pane = fields.iter().find(|(l, _)| *l == "pane").unwrap();
    assert!(
        pane.1.starts_with(feed_detail::NOT_RECORDED),
        "a resume line says nothing about the pane: {}",
        pane.1
    );
    assert!(pane.1.contains("did not measure the pane"));
    // Even a line shaped like change 1's native-stop detail does not print
    // as the pane's value off a substring guess: the measurement must ride
    // a structured field, and none does yet.
    let stop_shaped = reaped_item(
        "00847995-e0db-47c2-ab5b-24468ba1a4f5",
        "resume line mentions pid 22287 gone in passing",
    );
    let fields = feed_detail::detail_fields(&stop_shaped, &destination(&[], &stop_shaped));
    let pane = fields.iter().find(|(l, _)| *l == "pane").unwrap();
    assert!(
        pane.1.starts_with(feed_detail::NOT_RECORDED),
        "a stop-shaped recovery line is still not a measurement: {}",
        pane.1
    );
    let lines = feed_detail::detail_lines(&item, &d);
    assert!(lines.iter().any(|l| l == "resume: claude --resume x"));
    assert!(feed_detail::detail_footer(&d).contains("resume line"));

    let mut v = view_with_rows(vec![]);
    v.feed_detail_of = Some(item);
    // Enter hands the line over; it never attaches a session that is gone.
    assert!(matches!(
        v.feed_detail_hit(),
        Some(ChromeHit::Notice(msg)) if msg == "resume: claude --resume x"
    ));
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
    let fields = feed_detail::detail_fields(&item, &d);
    let by = |label: &str| {
        fields
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(by("pane"), "pane 0 · portal 0");
    assert_eq!(by("parent"), "s-parent");
    assert_eq!(by("king"), "L1 e-0001");
    assert!(feed_detail::detail_footer(&d).contains("focus its pane"));

    // A row whose session id does not match is NOT this event's session,
    // however its name reads: parent and king stay unrecorded, and the pane
    // says whose seat it actually is.
    let other = feed_item(Some("some-other-name"), Some("s-someone-else"));
    let other_dest = destination(&rows, &other);
    assert!(matches!(other_dest, Destination::NameOnly(_)));
    let other_fields = feed_detail::detail_fields(&other, &other_dest);
    let by_other = |label: &str| {
        other_fields
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(by_other("parent"), feed_detail::NOT_RECORDED);
    assert_eq!(by_other("king"), feed_detail::NOT_RECORDED);
    assert!(by_other("pane").contains("the node's current worker"));
}

// The panel drags narrower than any prose fits, and the caller clips from the
// end. The key must survive that clip: it is the only place it is advertised.
#[test]
fn the_header_keeps_its_key_at_every_draggable_width() {
    use crate::client::feed_view::header_line;
    for w in 0..=80usize {
        let unfocused = header_line(false, w);
        assert!(
            unfocused.starts_with(" E focus")
                || unicode_width::UnicodeWidthStr::width(unfocused) <= w,
            "w={w} picked {unfocused:?}"
        );
        let focused = header_line(true, w);
        assert!(
            focused.starts_with(" esc release")
                || unicode_width::UnicodeWidthStr::width(focused) <= w,
            "w={w} picked {focused:?}"
        );
    }
    // Below every prose spelling, the fallback leads with the key, so an
    // 8-column clip still reads "E focus" rather than a truncated label.
    assert_eq!(header_line(false, 8), " E focus");
    assert!(header_line(true, 8).starts_with(" esc"));
}

// An absent field says WHICH silence it is. A blank cell would teach nothing
// and would read as broken UI when the defect is upstream.
#[test]
fn an_absent_field_names_its_own_kind_of_silence() {
    use crate::client::feed_detail;
    let mut item = feed_item(Some("x-a"), None);
    item.kind = "node_created".into();
    item.harness = None;
    let fields = feed_detail::detail_fields(&item, &destination(&[], &item));
    let by = |label: &str| {
        fields
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    // A graph field was never run by a session, so its lane is inapplicable.
    assert!(by("model").starts_with(feed_detail::NOT_APPLICABLE));
    assert!(by("pane").starts_with(feed_detail::NOT_APPLICABLE));
    // A mechanism acted, so there is no session to attach to - and that is a
    // different statement from "we never wrote one down".
    let mut acted = feed_item(None, None);
    acted.kind = "decision_recorded".into();
    acted.actor = Some("fno agents stale-escalate".into());
    let fields = feed_detail::detail_fields(&acted, &destination(&[], &acted));
    let sid = fields.iter().find(|(l, _)| *l == "session-id").unwrap();
    assert!(
        sid.1.contains("acted by fno agents stale-escalate"),
        "{}",
        sid.1
    );
    // A completed node that RAN carries the last do/ship session, so its
    // lane was recorded somewhere and simply is not on this row. Calling it
    // inapplicable would contradict the session the same view attaches to.
    let mut ended = feed_item(Some("x-a"), Some("s-last"));
    ended.kind = "node_ended".into();
    ended.harness = Some("claude".into());
    let fields = feed_detail::detail_fields(&ended, &destination(&[], &ended));
    let model = fields.iter().find(|(l, _)| *l == "model").unwrap();
    assert_eq!(model.1, feed_detail::NOT_RECORDED, "{}", model.1);
    // Lineage is measured to be unrecorded on almost every row: say so.
    let parent = fields.iter().find(|(l, _)| *l == "parent").unwrap();
    assert_eq!(parent.1, feed_detail::NOT_RECORDED);
}

// The whole render path, not just the line builder: open the provenance view
// on a real View and read the COMPOSED frame. A field that never reaches the
// screen is the defect this view exists to prevent, so the assertion is on
// painted text.
#[test]
fn the_composed_frame_paints_every_field_and_its_action() {
    let mut v = view_with_rows(vec![]);
    v.term = (44, 120);
    v.feed = Some(overlay(vec![feed_item(Some("x-a"), Some("s-1"))]));
    let mut item = feed_item(Some("x-9223"), Some("s-1"));
    item.harness = Some("claude".into());
    item.model = Some("glm-5.3-flash".into());
    v.feed_detail_of = Some(item);

    let text = crate::vt::frame_text(&v.compose());
    for label in [
        "harness",
        "timestamp",
        "model",
        "effort",
        "node",
        "session-id",
        "pane",
        "parent",
        "king",
    ] {
        assert!(text.contains(label), "the frame never painted {label}");
    }
    // The values that ARE recorded, and the honest silence for the ones that
    // are not - never a blank cell.
    assert!(text.contains("glm-5.3-flash"));
    assert!(text.contains("x-9223"));
    assert!(text.contains(crate::client::feed_detail::NOT_RECORDED));
    // The action is named before it is pressed.
    assert!(text.contains("attach on portal 0"), "footer missing");
}
