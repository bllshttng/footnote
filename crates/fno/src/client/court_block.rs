//! (x-aeab) The court block's sideline geometry and painting, split out of
//! `client.rs` because that file is over the line budget and shrink-only.
//! A child module of `client`, so `View`'s private fields stay reachable
//! without widening them.

use super::glyph_cols;
use super::View;
use crate::proto::{cell_flags, Cell, Color};

impl View {
    /// Rows the court block owns at the bottom of the sideline: three when
    /// minimized, the expanded reading's height when expanded, and ZERO when
    /// the terminal cannot hold it beside at least one sideline row - the
    /// block yields, the rows never do.
    pub(super) fn court_block_rows(&self) -> usize {
        let block = if self.court.is_expanded() {
            self.court.expanded_lines(&self.agent_ages()).len()
        } else {
            crate::court_overlay::MINIMIZED_ROWS
        };
        let available = (self.term.0 as usize).saturating_sub(self.bottom_row_is_chrome() as usize);
        if available > block {
            block
        } else {
            0
        }
    }

    /// The transcript-derived age the daemon stamped on every row it handed
    /// us, in row order - the one source the census split reads.
    pub(super) fn agent_ages(&self) -> Vec<Option<u64>> {
        self.layout
            .agents
            .iter()
            .map(|a| a.last_activity_age_s)
            .collect()
    }

    /// The block's lines rendered once, with the row count the sideline
    /// must reserve for them: zero when the terminal cannot hold the block
    /// beside at least one row - the block yields, the rows never do.
    pub(super) fn court_block_layout(&self, term_rows: usize) -> (usize, Vec<String>) {
        let lines = if self.court.is_expanded() {
            self.court.expanded_lines(&self.agent_ages())
        } else {
            self.court.minimized_lines(&self.agent_ages())
        };
        let available = term_rows.saturating_sub(self.bottom_row_is_chrome() as usize);
        let reserved = if available > lines.len() {
            lines.len()
        } else {
            0
        };
        (reserved, lines)
    }
}

/// Paint the court block's lines at the bottom of the sideline column. The
/// row list already stopped above them (`list_rows`); the block renders DIM
/// so it reads as chrome beside the live rows, and the painter truncates to
/// the panel width - the same rule every sideline row follows.
pub(super) fn paint_court_block(
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
