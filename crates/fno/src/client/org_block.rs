//! The org block's sideline geometry and painting, split out of
//! `client.rs` because that file is over the line budget and shrink-only.
//! A child module of `client`, so `View`'s private fields stay reachable
//! without widening them.

use super::glyph_cols;
use super::View;
use crate::proto::{cell_flags, Cell, Color};

impl View {
    /// The width an org line may fill: the paint width less the divider
    /// column the painter never writes into. Fitting to the full paint width
    /// lets a line that just fits lose its last character to the clip.
    fn org_text_w(&self) -> usize {
        self.sideline_paint_w().saturating_sub(1)
    }

    /// Rows the org block owns at the bottom of the sideline: the minimized
    /// glance's `MINIMIZED_ROWS`, the expanded reading's height when
    /// expanded, and ZERO when the terminal cannot hold it beside at least
    /// one sideline row - the block yields, the rows never do. The strip row
    /// comes off the block's budget first: it is chrome the block may never
    /// paint into. Zero too when the sideline shows the backlog board: the
    /// board paints the full column, and rows the block does not paint there
    /// must not come off the hit math's region.
    pub(super) fn org_block_rows(&self) -> usize {
        if self.sideline_view != crate::view_store::SidelineView::Agents {
            return 0;
        }
        let block = if self.org.is_expanded() {
            let available = (self.term.0 as usize)
                .saturating_sub(1)
                .saturating_sub(self.bottom_row_is_chrome() as usize);
            self.org
                .expanded_lines(
                    &self.layout.agents,
                    self.org_text_w(),
                    (available / 2).max(3),
                )
                .len()
        } else {
            crate::org_overlay::MINIMIZED_ROWS
        };
        let block = block + self.observer_line_count();
        let available = (self.term.0 as usize)
            .saturating_sub(1)
            .saturating_sub(self.bottom_row_is_chrome() as usize);
        if available > block {
            block
        } else {
            0
        }
    }

    /// The block's lines rendered once, with the row count the sideline
    /// must reserve for them: zero when the terminal cannot hold the block
    /// beside at least one row - the block yields, the rows never do. The
    /// strip row comes off the budget first, the same rule
    /// `org_block_rows` yields by, so both resolvers agree at the boundary
    /// height.
    pub(super) fn org_block_layout(&self, term_rows: usize) -> (usize, Vec<String>) {
        if self.sideline_view != crate::view_store::SidelineView::Agents {
            return (0, Vec::new());
        }
        let available = term_rows
            .saturating_sub(1)
            .saturating_sub(self.bottom_row_is_chrome() as usize);
        let mut lines = if self.org.is_expanded() {
            self.org.expanded_lines(
                &self.layout.agents,
                self.org_text_w(),
                (available / 2).max(3),
            )
        } else {
            self.org
                .minimized_lines(&self.layout.agents, self.org_text_w())
        };
        lines.extend(self.observer_lines(self.org_text_w()));
        let available = term_rows
            .saturating_sub(1)
            .saturating_sub(self.bottom_row_is_chrome() as usize);
        let reserved = if available > lines.len() {
            lines.len()
        } else {
            0
        };
        (reserved, lines)
    }
}

/// Paint the org block's lines at the bottom of the sideline column. The
/// row list already stopped above them (`list_rows`); the block renders DIM
/// so it reads as chrome beside the live rows, and the painter truncates to
/// the panel width - the same rule every sideline row follows.
pub(super) fn paint_org_block(
    cells: &mut [Cell],
    lines: Vec<String>,
    list_rows: usize,
    rows: usize,
    cols: usize,
    text_w: usize,
) {
    for (k, text) in lines.into_iter().enumerate() {
        let r = list_rows + k;
        if r >= rows {
            break;
        }
        let mut col = 0usize;
        for ch in text.chars() {
            let w = glyph_cols(ch);
            if col + w > text_w {
                break;
            }
            cells[r * cols + col] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::DIM,
            };
            if w == 2 {
                cells[r * cols + col + 1] = Cell {
                    c: ' ',
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: cell_flags::DIM | cell_flags::WIDE_SPACER,
                };
            }
            col += w;
        }
    }
}
