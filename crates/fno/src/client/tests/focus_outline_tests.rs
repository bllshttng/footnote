// The focus-outline seam tests (this file is shrink-only under the file
// budget): the divider cells bounding the focused pane carry the lattice
// accent, and the outline follows focus in the same compose. Since the pane frame a
// FRAMED pane carries its focus signal on its own border instead, so the
// seam outline is exercised on narrow (unframed) panes and its ABSENCE on
// framed ones is asserted too.
use super::*;

/// Three 19-col panes: below the 20-col frame minimum, so the seam outline
/// still fires.
fn narrow_three(focus: u64) -> View {
    let mut view = two_pane_view();
    let rect = |x| Rect {
        x,
        y: 0,
        rows: 29,
        cols: 19,
    };
    view.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![(10, rect(0)), (11, rect(20)), (12, rect(40))],
        focus,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
    });
    view
}

#[test]
fn focus_outline_accents_focused_pane_seams_and_moves_with_focus() {
    // AC1-HP: the divider cells bounding the focused pane render
    // in the lattice accent at full brightness; a seam between two unfocused
    // panes stays DIM. Moving focus moves the accent in the same compose.
    let view = narrow_three(10);
    let frame = view.compose();
    let cols = frame.cols as usize;
    let seam_10_11 = 28 + 19; // divider left of pane 11: borders focused 10
    let seam_11_12 = 28 + 39; // divider between unfocused 11 and 12
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
    let mut moved = narrow_three(10);
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
fn a_framed_pane_owns_its_focus_signal_on_the_border() {
    // The seam outline stops firing for framed panes - the frame's
    // own border carries the focus color instead. The gaps between frames
    // stay blank even around the focused pane.
    let view = three_pane_view(); // 23-col panes: all framed; focus = 10
    let frame = view.compose();
    let cols = frame.cols as usize;
    let seam_10_11 = 28 + 23;
    let row = 5;
    let gap = frame.cells[row * cols + seam_10_11];
    assert_eq!(gap.c, ' ', "the gap between frames paints blank");
    assert_eq!(gap.fg, Color::Default, "no seam accent between frames");
    // The focused pane's frame border is the accent.
    let border = frame.cells[row * cols + 28]; // pane 10's left border cell
    assert_eq!(border.c, '│');
    assert_eq!(border.fg, LATTICE_ACCENT, "the focused frame is amber");
}

#[test]
fn single_pane_tab_paints_no_focus_outline() {
    // AC5-EDGE, reworded for the pane frame: one pane fills the content area,
    // so there are no interior seams and no seam outline; the frame (not a
    // seam) carries the focus signal.
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
    // No divider glyphs exist on a single-pane tab, so no accented '│' seam
    // can exist; the frame's own border is the accent.
    let cols = frame.cols as usize;
    let accented_seam = (0..frame.rows as usize).any(|r| {
        (view.panel_w() as usize..cols).any(|c| {
            let cell = frame.cells[r * cols + c];
            cell.fg == LATTICE_ACCENT && (cell.c == '┼' || cell.c == '─')
        })
    });
    assert!(
        !accented_seam,
        "a single-pane tab paints no seam outline; the frame is the signal"
    );
    let _ = cols;
}
