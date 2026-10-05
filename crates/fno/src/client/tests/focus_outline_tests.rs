// The focus-outline seam tests (this file is shrink-only under the file
// budget): the divider cells bounding the focused pane carry the lattice
// accent, and the outline follows focus in the same compose. Since the pane frame a
// FRAMED pane carries its focus signal on its own border instead, so the
// seam outline is exercised on narrow (unframed) panes and its ABSENCE on
// framed ones is asserted too. The region-owner work extends the same three
// functions: the accent is a KEYBOARD mark (it dies while the feed or the
// board owns typing), and the click routing that moves the owner.
use super::*;

use crate::client::region_focus::{mouse_pre_pass, RegionOwner};
use crate::keys::Scanner;
use crate::proto::{MouseButton, MouseKind};

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

fn report(kind: MouseKind, row: u16, col: u16, shift: bool) -> crate::mouse::MouseReport {
    crate::mouse::MouseReport {
        kind,
        row,
        col,
        shift,
    }
}

fn press_left(row: u16, col: u16) -> crate::mouse::MouseReport {
    report(MouseKind::Press(MouseButton::Left), row, col, false)
}

/// Drive one report through the extracted pre-pass; returns the wire bytes.
fn pre_pass(view: &mut View, rep: crate::mouse::MouseReport) -> Vec<u8> {
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut scanner = Scanner::default();
        mouse_pre_pass(view, &mut scanner, vec![rep], &mut sock)
            .await
            .unwrap();
    });
    sock
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

    // The seam accent is a KEYBOARD mark (AC3-HP): with the feed owning
    // typing, no pane wears the outline, though the server focus is
    // unchanged. A pane click hands the owner back and the outline returns.
    let mut feed_owns = narrow_three(10);
    feed_owns.feed = Some(super::feed_view::open_overlay(None, 0));
    feed_owns.region_owner = RegionOwner::Feed;
    assert_eq!(feed_owns.input_owner(), RegionOwner::Feed);
    let frame = feed_owns.compose();
    assert_eq!(
        frame.cells[row * cols + seam_10_11].flags & cell_flags::DIM,
        cell_flags::DIM,
        "the seam outline dims while the feed owns typing"
    );
    // The focused feed's header row 0 wears the title fill in the same
    // compose that dims the seams (AC2-UI).
    let x0 = (feed_owns.term.1 - feed_owns.feed_panel_w()) as usize;
    assert_eq!(
        frame.cells[x0 + 2].bg,
        feed_owns.theme.brand,
        "the focused feed header carries the accent fill"
    );
    feed_owns.region_owner = RegionOwner::Pane;
    let frame = feed_owns.compose();
    assert_eq!(
        frame.cells[row * cols + seam_10_11].fg,
        LATTICE_ACCENT,
        "the outline returns when a pane owns typing again"
    );
    assert_ne!(
        frame.cells[x0 + 2].bg,
        feed_owns.theme.brand,
        "the feed header drops the fill in the same frame the seam takes the mark"
    );
    // AC5-EDGE, reworded for the pane frame: one pane fills the content
    // area, so there are no interior seams and no seam outline; the frame
    // (not a seam) carries the focus signal.
    let mut single = three_pane_view();
    single.set_layout(LayoutView {
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
    let frame = single.compose();
    let cols = frame.cols as usize;
    let accented_seam = (0..frame.rows as usize).any(|r| {
        (single.panel_w() as usize..cols).any(|c| {
            let cell = frame.cells[r * cols + c];
            cell.fg == LATTICE_ACCENT && cell.c == '┼'
        })
    });
    assert!(
        !accented_seam,
        "a single-pane tab paints no seam outline; the frame is the signal"
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

    // The border accent is the same keyboard mark: with the feed on the
    // keyboard the focused frame goes dim, in both a named theme and the
    // terminal one (theme cases ride the same band vocabulary).
    for name in ["footnote-paper", "terminal"] {
        let mut feed_owns = three_pane_view();
        feed_owns.theme = crate::theme::Theme::from_name(name).0;
        feed_owns.feed = Some(super::feed_view::open_overlay(None, 0));
        feed_owns.region_owner = RegionOwner::Feed;
        let frame = feed_owns.compose();
        let dimmed = frame.cells[row * cols + 28];
        assert_eq!(
            dimmed.flags & cell_flags::DIM,
            cell_flags::DIM,
            "{name}: the focused frame dims while the feed owns typing"
        );
    }
}

#[test]
fn clicks_select_one_input_owner_and_route_bytes() {
    // AC1-HP/AC2-HP routing matrix: an explicit click names the input owner
    // (pane border/content, feed panel, backlog column), a same-chunk
    // click+typing lands on the new owner, exclusive modals and Shift
    // selection never move it, and a closed region normalizes back to the
    // pane. The wire assertions ride the forward points: a pane content
    // click forwards Mouse, a border click sends FocusPane, a feed click
    // sends nothing.
    // A content click on pane 11 forwards Mouse and moves the owner.
    let mut view = narrow_three(10);
    // A pointer that swept across pane 12 left a hover pending its settle:
    // the click must cancel it, or the settle timer refocuses pane 12 over
    // the operator's explicit choice.
    view.hover_pending = Some((12, std::time::Instant::now()));
    let wire = pre_pass(&mut view, press_left(5, 53)); // pane 11's content
    assert_eq!(view.input_owner(), RegionOwner::Pane);
    assert!(
        view.hover_pending.is_none(),
        "an explicit click cancels a pending focus-follow"
    );
    assert!(!wire.is_empty(), "a content click forwards Mouse");

    // A border click on a FRAMED pane sends FocusPane (pane 10's left ring).
    let mut view = three_pane_view();
    let wire = pre_pass(&mut view, press_left(5, 28));
    assert_eq!(view.input_owner(), RegionOwner::Pane);
    assert!(!wire.is_empty(), "a border click sends FocusPane");

    // A Shift-modified press is dropped: no owner move, no wire.
    let mut view = narrow_three(10);
    let wire = pre_pass(
        &mut view,
        report(MouseKind::Press(MouseButton::Left), 5, 53, true),
    );
    assert_eq!(view.input_owner(), RegionOwner::Pane);
    assert!(wire.is_empty(), "a Shift press reaches nothing");

    // A click inside the feed panel moves the owner to the feed and sends
    // nothing (the header cell carries no provenance row).
    let mut view = narrow_three(10);
    view.feed = Some(super::feed_view::open_overlay(None, 0));
    let feed_col = view.term.1 - 4; // inside the panel, off the divider
    let wire = pre_pass(&mut view, press_left(0, feed_col));
    assert_eq!(view.input_owner(), RegionOwner::Feed);
    assert!(wire.is_empty(), "a feed chrome click sends nothing");

    // Same-chunk click + typing: the click set the owner first, so the
    // chunk's plain bytes route to the FEED, not a pane (consumed, no wire).
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut scanner = Scanner::default();
        let flow = super::overlay_keys::route(&mut view, &mut scanner, b"j", &mut sock)
            .await
            .expect("the focused feed owns the chunk");
        assert!(matches!(flow, Ok(super::StdinFlow::Continue)));
    });
    assert!(sock.is_empty(), "typing reaches the feed, never a pane");

    // A closed region normalizes: dropping the feed returns typing to the
    // pane without any bookkeeping at the close site.
    view.feed = None;
    assert_eq!(view.input_owner(), RegionOwner::Pane);

    // The windowed board column is the board's surface: the click focuses
    // the board and resolves nothing else there (no phantom agent row drag).
    let mut view = narrow_three(10);
    view.backlog_board = Some(super::backlog_board::BoardView::new(0));
    view.sideline_view = crate::view_store::SidelineView::Backlog;
    let wire = pre_pass(&mut view, press_left(5, 10));
    assert_eq!(view.input_owner(), RegionOwner::Board);
    assert!(wire.is_empty(), "a board-column click acts on no row");

    // An exclusive modal owns the pointer: the same pane press moves
    // nothing (AC2-HP).
    let mut view = narrow_three(10);
    view.keys_modal = Some(super::keys_modal::build_keys_modal());
    let wire = pre_pass(&mut view, press_left(5, 53));
    assert_eq!(view.input_owner(), RegionOwner::Pane);
    assert!(
        wire.is_empty(),
        "a modal owns the press; no forward escapes"
    );

    // Hover never steals from a focused region (q-7eadc5fe): with the feed
    // owning typing, a pointer resting on pane 11 past the settle delay
    // leaves no pending focus, and closing the feed hands typing to the
    // pane focused before it opened. The board owns the same way.
    let mut view = narrow_three(10);
    view.feed = Some(super::feed_view::open_overlay(None, 0));
    view.region_owner = RegionOwner::Feed;
    view.on_hover(5, 53, std::time::Instant::now());
    assert!(
        view.hover_pending.is_none(),
        "hover under a focused feed never arms a focus settle"
    );
    view.feed = None;
    assert_eq!(view.input_owner(), RegionOwner::Pane);
    assert_eq!(view.layout.focus, 10, "server focus never moved");

    let mut view = narrow_three(10);
    view.backlog_board = Some(super::backlog_board::BoardView::new(0));
    view.sideline_view = crate::view_store::SidelineView::Backlog;
    view.region_owner = RegionOwner::Board;
    view.on_hover(5, 53, std::time::Instant::now());
    assert!(
        view.hover_pending.is_none(),
        "hover under a focused board never arms a focus settle"
    );
    view.backlog_board = None;
    assert_eq!(view.input_owner(), RegionOwner::Pane);

    // The composer cursor follows the owner: backlog open but a pane
    // clicked (Pane owns), the frame shows the focused pane's cursor;
    // the board owning typing hides it (AC4-UI).
    let mut view = narrow_three(10);
    view.backlog_board = Some(super::backlog_board::BoardView::new(0));
    view.sideline_view = crate::view_store::SidelineView::Backlog;
    let frame = view.compose();
    assert!(
        frame.cursor_visible,
        "a pane owns typing under an open backlog: its cursor shows"
    );
    view.region_owner = RegionOwner::Board;
    let frame = view.compose();
    assert!(
        !frame.cursor_visible,
        "the board owning typing hides the pane cursor"
    );
}
