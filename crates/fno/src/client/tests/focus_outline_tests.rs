// The focus-outline seam tests (this file is shrink-only under the file
// budget): the divider cells bounding the focused pane carry the lattice
// accent, and the outline follows focus in the same compose.
use super::*;

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
