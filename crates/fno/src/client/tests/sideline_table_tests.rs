//! The acceptance family for the sideline-as-a-Table rewrite: status words,
//! ellipsized names, the markup-stripped message column, and the
//! TableState-driven selection scroll.

use super::*;

#[path = "sideline_name_fit_tests.rs"]
mod sideline_name_fit_tests;

// ---------------------------------------------------------------------------
// the table rewrite: the sideline is a Table (status word, name, message, PR, age)
// ---------------------------------------------------------------------------

#[test]
fn sideline_name_middle_elides_and_keeps_suffix_gap_to_the_message() {
    // acceptance: a long worker name at a 60-column panel keeps its
    // distinguishing suffix after the middle ellipsis.
    let mut view = two_pane_view();
    view.sideline_width = 60;
    let mut a = tab_agent(None, None, false);
    a.name = "dispatch-fno-8bef7b".into();
    a.tail = Some("**PR 2113 merged as `84fa`.**".into());
    view.layout.agents = vec![a];
    let frame = view.compose();
    let cols = frame.cols as usize;
    let text_w = (view.panel_w() - 1) as usize;
    let rects = sideline_column_rects(text_w as u16);
    let row = 1; // row 0 is the squad header
    let name = &frame.cells
        [row * cols + rects[1].x as usize..row * cols + (rects[1].x + rects[1].width) as usize];
    assert!(
        name.iter().any(|c| c.c == '\u{2026}'),
        "the name cell contains the middle ellipsis"
    );
    let rendered_name: String = name.iter().map(|c| c.c).collect();
    assert!(
        rendered_name.contains("8bef7b"),
        "the name cell keeps the distinguishing suffix: {rendered_name:?}"
    );
}

#[test]
fn sideline_message_reads_the_sentence_not_the_markup() {
    // acceptance: `**PR 2113 merged as `84fa`.**` paints as
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
    // acceptance: a working agent row's status cell reads `Working`
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
    assert!(status.trim_start().starts_with("Work"), "{status:?}");
    let want = sideline_color::resolve_lane_color(Some("codex"), None, None, None)
        .unwrap_or(Color::Default);
    let fg = frame.cells[row * cols + rects[0].x as usize].fg;
    assert_eq!(fg, want, "the status word wears the lane color");
}

#[test]
fn sideline_selection_scrolls_into_view_and_paints_inverse() {
    // acceptance: 80 rows on a short panel with the selection past
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

#[test]
fn status_sort_arrow_fits_inside_the_status_header_span() {
    // The TableHead row is the paint now: the status column carries the
    // sort arrow where the column hit-test finds it.
    let mut v = wide_view(vec![agent_row(
        "agent",
        4,
        Some(AgentBadge::Working),
        false,
    )]);
    set_density(&mut v, Density::Extended);
    v.agent_sort = AgentSort::Attention;
    let frame = v.compose();
    let rects = sideline_column_rects((v.panel_w() - 1) as u16);
    let status: String = frame.cells[rects[0].x as usize..(rects[0].x + rects[0].width) as usize]
        .iter()
        .map(|c| c.c)
        .collect();
    assert!(
        status.contains('\u{2191}'),
        "status header shows direction: {status:?}"
    );
}

#[test]
fn extended_pr_cell_shows_number_or_neutral_value() {
    // The PR cell is the paint now: `#482` when known, the em-dash when not.
    let mut known = agent_row("known", 4, Some(AgentBadge::Working), false);
    known.pr = Some(482);
    let unknown = agent_row("unknown", 5, Some(AgentBadge::Working), false);
    let mut v = wide_view(vec![known, unknown]);
    set_density(&mut v, Density::Extended);
    let frame = v.compose();
    let cols = frame.cols as usize;
    let rects = sideline_column_rects((v.panel_w() - 1) as u16);
    let cell_text = |row: usize| {
        frame.cells
            [row * cols + rects[3].x as usize..row * cols + (rects[3].x + rects[3].width) as usize]
            .iter()
            .map(|c| c.c)
            .collect::<String>()
    };
    // Row 0 is the TableHead, row 1 the squad band; the agents paint at
    // rows 2 and 3.
    assert!(cell_text(2).contains("#482"), "known PR renders");
    assert!(
        cell_text(3).contains('\u{2014}'),
        "unknown PR renders the neutral dash"
    );
}

#[test]
fn sort_label_survives_every_column_configuration() {
    // The sort toggle must stay VISIBLE at every width the table renders
    // at: the head row's arrows must survive the paint at the widest and
    // narrowest admitted tables.
    // Below ~40 columns the solver crushes the age cell out of the row,
    // so the toggle's honest floor is the table's own; at and above it the
    // arrow must always survive.
    for cols in [EXTENDED_PANEL_W, 44] {
        let mut v = wide_view(vec![agent_row("a", 4, Some(AgentBadge::Working), false)]);
        set_density(&mut v, Density::Extended);
        v.term = (24, cols + MIN_CONTENT_COLS + 4);
        v.agent_sort = AgentSort {
            column: AgentSortColumn::Age,
            direction: SortDirection::Descending,
        };
        let first_line = frame_text(&v.compose()).lines().next().unwrap().to_string();
        assert!(
            first_line.contains("age\u{2193}"),
            "age header visible at width {cols}: {first_line:?}"
        );
    }
}

#[test]
fn status_word_sits_one_column_from_the_name_cell_parent_and_child() {
    // Acceptance: the gap is the test, the widths are not. A short status
    // word right-aligns inside its fixed cell, so its last glyph sits exactly
    // one spacing column from the name cell - for a parent row and for a
    // spawned child (depth 1), whose indent must not widen the gap.
    let parent = {
        let mut a = agent_row(
            "architect-with-a-very-long-name",
            4,
            Some(AgentBadge::Working),
            false,
        );
        a.pane_activity = None;
        a.harness_session_id = Some("sess-arch".into());
        a
    };
    let child = {
        let mut a = agent_row(
            "child-with-a-very-long-name-too",
            5,
            Some(AgentBadge::Working),
            false,
        );
        a.pane_activity = None;
        a.lineage_kind = Some("child".into());
        a.spawned_by_session = Some("sess-arch".into());
        a
    };
    let mut v = wide_view(vec![parent, child]);
    set_density(&mut v, Density::Extended);
    let frame = v.compose();
    let cols = frame.cols as usize;
    let rects = sideline_column_rects((v.panel_w() - 1) as u16);
    // Rows: 0 TableHead, 1 squad band, 2 parent, 3 child.
    for (row, label) in [(2usize, "parent"), (3, "child")] {
        let status: String = frame.cells
            [row * cols + rects[0].x as usize..row * cols + (rects[0].x + rects[0].width) as usize]
            .iter()
            .map(|c| c.c)
            .collect();
        assert_eq!(
            status.trim(),
            "Work",
            "{label} status word reads the state: {status:?}"
        );
        // The acceptance IS the gap: from the word's last glyph to the name
        // cell, exactly the one inter-column space - the widths are not the
        // test. Left-aligned paint fails this (5 columns for a 7-wide word).
        assert_eq!(
            rects[1].x - rects[0].x - status.trim_end().chars().count() as u16,
            1,
            "{label}: one column between the status word and the name cell"
        );
    }
}
