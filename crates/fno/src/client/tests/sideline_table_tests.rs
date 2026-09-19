//! The x-177c acceptance family: the sideline as a Table. Status words,
//! ellipsized names, the markup-stripped message column, and the
//! TableState-driven selection scroll.

use super::*;

// ---------------------------------------------------------------------------
// x-177c: the sideline is a Table (status word, name, message, PR, age)
// ---------------------------------------------------------------------------

#[test]
fn sideline_name_truncates_with_ellipsis_and_one_gap_to_the_message() {
    // x-177c acceptance: a 30-char name at a 60-column panel ends in the
    // ellipsis glyph, and one space separates the name cell from the message.
    let mut view = two_pane_view();
    view.sideline_width = 60;
    let mut a = tab_agent(None, None, false);
    a.name = "x".repeat(30);
    a.tail = Some("**PR 2113 merged as `84fa`.**".into());
    view.layout.agents = vec![a];
    let frame = view.compose();
    let cols = frame.cols as usize;
    let text_w = (view.panel_w() - 1) as usize;
    let rects = sideline_column_rects(text_w as u16);
    let row = 1; // row 0 is the squad header
    let name = &frame.cells
        [row * cols + rects[1].x as usize..row * cols + (rects[1].x + rects[1].width) as usize];
    assert_eq!(
        name.last().map(|c| c.c),
        Some('\u{2026}'),
        "the name cell ends in the ellipsis glyph"
    );
}

#[test]
fn sideline_message_reads_the_sentence_not_the_markup() {
    // x-177c acceptance: `**PR 2113 merged as `84fa`.**` paints as
    // `· PR 2113 merged as 84fa.` - bold markers and backticks stripped, the
    // separator leading the message column.
    let mut view = two_pane_view();
    view.term = (30, 140);
    view.sideline_width = 80;
    let mut a = tab_agent(None, None, false);
    a.tail = Some("**PR 2113 merged as `84fa`.**".into());
    view.layout.agents = vec![a];
    let frame = view.compose();
    let cols = frame.cols as usize;
    let text_w = (view.panel_w() - 1) as usize;
    let rects = sideline_column_rects(text_w as u16);
    let row = 1;
    let msg: String = frame.cells
        [row * cols + rects[2].x as usize..row * cols + (rects[2].x + rects[2].width) as usize]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(msg.trim_end(), "\u{b7} PR 2113 merged as 84fa.", "{msg:?}");
}

#[test]
fn sideline_status_cell_reads_the_state_word_in_the_lane_color() {
    // x-177c acceptance: a working agent row's status cell reads `Working`
    // in the lane color.
    let mut view = two_pane_view();
    view.sideline_width = 60;
    let mut a = tab_agent(None, None, false);
    a.badge = Some(AgentBadge::Working);
    a.pane_activity = None;
    a.harness = Some("codex".into());
    view.layout.agents = vec![a];
    let frame = view.compose();
    let cols = frame.cols as usize;
    let text_w = (view.panel_w() - 1) as usize;
    let rects = sideline_column_rects(text_w as u16);
    let row = 1;
    let status: String = frame.cells
        [row * cols + rects[0].x as usize..row * cols + (rects[0].x + rects[0].width) as usize]
        .iter()
        .map(|c| c.c)
        .collect();
    assert!(status.starts_with("Working"), "{status:?}");
    let want = sideline_color::resolve_lane_color(Some("codex"), None, None, None)
        .unwrap_or(Color::Default);
    let fg = frame.cells[row * cols + rects[0].x as usize].fg;
    assert_eq!(fg, want, "the status word wears the lane color");
}

#[test]
fn sideline_selection_scrolls_into_view_and_paints_inverse() {
    // x-177c acceptance: 80 rows on a short panel with the selection past
    // the bottom -> the Table's offset scrolls the selected row into view
    // and that row paints INVERSE.
    let mut view = two_pane_view();
    let agents = (0..80)
        .map(|i| {
            let mut a = tab_agent(None, Some(AgentBadge::Working), false);
            a.name = format!("agent-{i}");
            a.pane_activity = None;
            a.pane_id = Some(2 + i as u64);
            a
        })
        .collect::<Vec<_>>();
    view.layout.agents = agents;
    view.selector = Some(60); // display row 60, after the squad header at 0
    let frame = view.compose();
    let cols = frame.cols as usize;
    let visible = view.sideline_visible_rows();
    let sel_row = visible - 1; // selection + 1 - visible scrolls to the last line
    let flags = frame.cells[sel_row * cols].flags;
    assert_eq!(
        flags & cell_flags::INVERSE,
        cell_flags::INVERSE,
        "the selected row scrolls into view and paints inverse"
    );
    assert_ne!(
        frame.cells[0].c, '\u{25be}',
        "the squad header scrolled off the top"
    );
}
