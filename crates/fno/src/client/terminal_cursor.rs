//! The composed frame's one terminal cursor: the composer sheet's editor
//! while it is open (the keyboard owner), else the focused pane's, offset
//! into its rect - the one place the cursor may sit (AC1-UI/AC5-UI).

use super::{region_focus, View};

/// The (row, col, visible) cell the real terminal cursor paints this frame.
/// The composer sheet wins when its cell is known; the pane branch runs only
/// when every overlay is closed, so a pane behind a modal never borrows the
/// cursor.
pub(super) fn compose_cursor(view: &View, launcher_cursor: Option<(u16, u16)>) -> (u16, u16, bool) {
    if let Some((r, c)) = launcher_cursor {
        return (r, c, true);
    }
    let overlays_closed = view.selector.is_none()
        && view.answers.is_none()
        && view.yard.is_none()
        && view.digest.is_none()
        && view.move_pick.is_none()
        && view.attach_place.is_none()
        && view.portal_pick.is_none()
        && view.nav.is_none()
        && view.peek.is_none()
        && view.connections.is_none()
        && view.keys_modal.is_none()
        && view.row_menu.is_none()
        && view.aux.is_none()
        && !((view.backlog_board.is_some() || view.org_board.is_some())
            && (view.board_full || view.input_owner() == region_focus::RegionOwner::Board))
        && view.messages_board.is_none();
    if !overlays_closed {
        return (0, 0, false);
    }
    let Some((_, rect)) = view
        .layout
        .panes
        .iter()
        .find(|(id, _)| *id == view.layout.focus)
    else {
        return (0, 0, false);
    };
    let Some(f) = view.frames.get(&view.layout.focus) else {
        return (0, 0, false);
    };
    // The cursor sits in the pty grid, which is the CONTENT rect for a
    // framed pane.
    let content = crate::pane_border::content_rect(*rect);
    let mut cur_r =
        super::TAB_BAR_ROWS + content.y + f.cursor_row.min(content.rows.saturating_sub(1));
    let mut cur_c =
        view.left_chrome_w() + content.x + f.cursor_col.min(content.cols.saturating_sub(1));
    if !view.sideline_full && view.layout.area != (0, 0) {
        // Never in the filler (AC1-UI), even mid-race when a stale rect
        // exceeds the just-shrunk area. Skipped in full-screen sideline: the
        // cursor belongs to the composer, not a pane that is not painted.
        cur_r = cur_r.min(super::TAB_BAR_ROWS + view.layout.area.0.saturating_sub(1));
        cur_c = cur_c.min(view.left_chrome_w() + view.layout.area.1.saturating_sub(1));
    }
    (cur_r, cur_c, f.cursor_visible)
}
