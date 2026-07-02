//! Terminal-grid construction behind the [`ScreenView`] seam (Wave 2).
//!
//! Wave 1 defined [`crate::readiness::ScreenView`] as the read-only shape every
//! [`crate::readiness::ReadinessDetector`] inspects, and deliberately left the
//! grid *construction* to Wave 2 "so the substrate stays decoupled from the
//! terminal-emulator crate." This module is that construction: it feeds raw PTY
//! output bytes (ANSI escapes, cursor moves, partial sequences) through a
//! terminal-state parser and snapshots the rendered grid into an
//! [`OwnedScreen`] a [`ScreenView`] borrows from.
//!
//! ## Terminal-emulator crate choice
//!
//! This module uses [`alacritty_terminal`] (the crate the original design
//! named). An earlier revision used `vt100` on a "dependency weight"
//! rationale - that `alacritty_terminal` drags in winit-adjacent + Windows
//! GUI crates. That rationale was **wrong** for `alacritty_terminal` 0.26:
//! its transitive tree is `vte` (the same VT parser `vt100` wraps) plus
//! `parking_lot`, `polling`, `rustix-openpty`, `regex-automata`, and a
//! cfg-gated `windows-sys` (FFI, compiles to nothing off Windows) - no
//! winit, no GUI stack. The richer cell model (`Flags`, `NamedColor`/`Rgb`,
//! `Dimensions`) is exactly what the mux/agent surfaces
//! needs, so the whole crate standardized on it (ab-3c063856 review).
//!
//! The [`crate::readiness::ReadinessDetector`] trait is unchanged: it still
//! operates over [`ScreenView`], so the emulator crate is an implementation
//! detail of this file. Used headless via `Term<VoidListener>` +
//! `vte::ansi::Processor` - no event loop, no rendering half of the tree.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::readiness::ScreenView;

/// Default grid size used when the daemon has not yet observed a PTY winsize.
/// 24x80 is the historical terminal default and matches the drive UX fallback
/// ("no initial resize within 2s; using 24x80 default").
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// A `Dimensions` impl for constructing / resizing a headless alacritty
/// [`Term`]. `screen_lines` is the visible viewport height; `total_lines`
/// equals it because we keep zero scrollback (the readiness seam and the
/// compositor render only the visible screen). Shared with
/// so all surfaces size their grids identically.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GridSize {
    pub rows: usize,
    pub cols: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Build a zero-scrollback alacritty config. The readiness seam renders only
/// the visible screen, so scrollback history would be wasted retention
/// (vt100's `Parser::new(.., 0)` had the same intent).
pub(crate) fn visible_only_config() -> Config {
    Config {
        scrolling_history: 0,
        ..Config::default()
    }
}

/// A terminal grid fed incrementally with raw PTY bytes. The daemon (Wave 3)
/// owns one per PTY-managed agent and feeds it from the PTY drainer's ring; a
/// [`ReadinessDetector`](crate::readiness::ReadinessDetector) inspects its
/// [`snapshot`](TerminalGrid::snapshot) to decide whether the CLI is ready.
pub struct TerminalGrid {
    term: Term<VoidListener>,
    processor: Processor,
    // OSC title/progress capture (E6.1). `processor` parses OSC sequences but
    // dispatches them to the `VoidListener` (discarded), so a parallel scanner
    // keeps the OSC strings the manifest engine wants as detection regions.
    osc: crate::osc::OscCapture,
    rows: u16,
    cols: u16,
}

impl TerminalGrid {
    /// Construct a grid of the given size. Zero dimensions are clamped to 1 so
    /// the parser never panics on a degenerate winsize.
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let size = GridSize {
            rows: rows as usize,
            cols: cols as usize,
        };
        TerminalGrid {
            term: Term::new(visible_only_config(), &size, VoidListener),
            processor: Processor::new(),
            osc: crate::osc::OscCapture::new(),
            rows,
            cols,
        }
    }

    /// Construct a grid at the 24x80 default size.
    pub fn with_default_size() -> Self {
        Self::new(DEFAULT_ROWS, DEFAULT_COLS)
    }

    /// Feed raw PTY output. Safe to call with partial escape sequences split
    /// across reads; the underlying `vte` parser buffers incomplete sequences
    /// between `advance` calls.
    pub fn feed(&mut self, bytes: &[u8]) {
        // `term` and `processor` are disjoint fields, so both mutable borrows
        // are allowed.
        self.processor.advance(&mut self.term, bytes);
        // Same bytes, second pass: capture OSC title/progress the grid parser
        // throws away. Two passes over the stream is O(2n); titles are short.
        self.osc.feed(bytes);
    }

    /// Resize the grid, mirroring a PTY winsize change. Dimensions are clamped
    /// to 1.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.term.resize(GridSize {
            rows: rows as usize,
            cols: cols as usize,
        });
        self.rows = rows;
        self.cols = cols;
    }

    /// Snapshot the current screen into an owned holder a [`ScreenView`] can
    /// borrow from. `visible_text` is plain text (no formatting): one line per
    /// grid row with trailing blank cells trimmed, exactly what a human would
    /// see and what the readiness detectors substring-match against. Cursor is
    /// **0-indexed** `(row, col)` (preserved from the prior vt100 contract).
    pub fn snapshot(&self) -> OwnedScreen {
        let grid = self.term.grid();
        let cursor_point = grid.cursor.point;
        // alacritty reports the cursor 0-indexed via the inner `.0`; keep that
        // (the old vt100 `cursor_position()` was 0-indexed too).
        let cursor_row = cursor_point.line.0.max(0) as usize;
        let cursor_col = cursor_point.column.0;

        // Build one trimmed string per row, then drop trailing blank rows and
        // join with '\n'. This reproduces vt100 `contents()` semantics exactly
        // (trailing blank cells per row trimmed AND trailing blank rows
        // dropped), which the readiness detectors and the daemon's settled-
        // reply extraction (daemon.rs) substring-match / equality-check
        // against. A 1-row "done ❯" screen must read "done ❯", not
        // "done ❯\n\n\n…".
        let mut rows: Vec<String> = Vec::with_capacity(self.rows as usize);
        for row_idx in 0..(self.rows as usize) {
            let line = Line(row_idx as i32);
            let mut row = String::with_capacity(self.cols as usize);
            for col_idx in 0..(self.cols as usize) {
                row.push(grid[line][Column(col_idx)].c);
            }
            while row.ends_with(' ') {
                row.pop();
            }
            rows.push(row);
        }
        while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
            rows.pop();
        }
        let text = rows.join("\n");

        OwnedScreen {
            text,
            cursor_row,
            cursor_col,
            osc_title: self.osc.title().map(str::to_string),
            osc_progress: self.osc.progress().map(str::to_string),
        }
    }
}

/// Owned snapshot of a rendered grid. Exists because [`ScreenView`] borrows its
/// `visible_text` as `&str`, while the parser yields an owned `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedScreen {
    pub text: String,
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// Latest OSC window title (OSC 0/2), captured from the byte stream (E6.1).
    /// A detection region for the manifest engine; `None` until a title OSC is
    /// seen.
    pub osc_title: Option<String>,
    /// Latest OSC 9;4 progress payload, if any (E6.1).
    pub osc_progress: Option<String>,
}

impl OwnedScreen {
    /// Borrow this snapshot as the read-only [`ScreenView`] a detector inspects.
    pub fn view(&self) -> ScreenView<'_> {
        ScreenView {
            visible_text: &self.text,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            osc_title: self.osc_title.as_deref(),
            osc_progress: self.osc_progress.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_renders_and_cursor_advances() {
        let mut grid = TerminalGrid::with_default_size();
        grid.feed(b"hello");
        let owned = grid.snapshot();
        assert!(owned.text.starts_with("hello"));
        // Cursor sits just past the written text on row 0.
        assert_eq!(owned.cursor_row, 0);
        assert_eq!(owned.cursor_col, 5);
    }

    #[test]
    fn ansi_escapes_are_interpreted_not_echoed() {
        let mut grid = TerminalGrid::new(4, 20);
        // Write "AAA", then a CSI cursor-home + clear-line, then "B". The grid
        // must reflect the rendered result, not the raw escape bytes.
        grid.feed(b"AAA\x1b[1;1H\x1b[2KB");
        let owned = grid.snapshot();
        assert!(
            !owned.text.contains('\x1b'),
            "raw escape leaked into visible text: {:?}",
            owned.text
        );
        assert!(
            owned.text.starts_with('B'),
            "cursor-home + clear should leave 'B' at the top-left, got {:?}",
            owned.text
        );
    }

    #[test]
    fn partial_escape_split_across_feeds_is_buffered() {
        let mut grid = TerminalGrid::new(4, 20);
        // Split a CSI sequence (\x1b[1;1H) across two feeds.
        grid.feed(b"X\x1b[1");
        grid.feed(b";1HY");
        let owned = grid.snapshot();
        assert!(!owned.text.contains('\x1b'));
        // The completed home sequence repositions to top-left so 'Y' overwrites 'X'.
        assert!(owned.text.starts_with('Y'), "got {:?}", owned.text);
    }

    #[test]
    fn resize_does_not_panic_and_view_roundtrips() {
        let mut grid = TerminalGrid::with_default_size();
        grid.resize(0, 0); // clamped to 1x1
        grid.feed(b"q");
        let owned = grid.snapshot();
        let view = owned.view();
        assert_eq!(view.visible_text, owned.text);
        assert_eq!(view.cursor_row, owned.cursor_row);
    }

    #[test]
    fn osc_title_exposed_on_snapshot_and_reassembled_across_feeds() {
        // AC-E6-1 at the read-loop level: an OSC title split across two feeds
        // reassembles, is exposed on the snapshot (owned + borrowed view), and
        // is NOT echoed into the visible grid (the grid parser consumes it).
        let mut grid = TerminalGrid::with_default_size();
        // OSC 2 set-title "⠋ Compiling" (braille spinner = claude "working"
        // signal), split mid-codepoint, then plain "hello" to the grid.
        grid.feed(b"\x1b]2;\xe2\xa0\x8b Compil");
        grid.feed(b"ing\x07hello");
        let owned = grid.snapshot();
        assert_eq!(owned.osc_title.as_deref(), Some("\u{280b} Compiling"));
        assert!(
            owned.text.starts_with("hello"),
            "OSC bytes must not leak into the grid, got {:?}",
            owned.text
        );
        // Exposed through the borrowed detector seam too.
        let view = owned.view();
        assert_eq!(view.osc_title, Some("\u{280b} Compiling"));
    }

    #[test]
    fn prompt_glyph_survives_to_visible_tail() {
        // Readiness detectors check `visible_text.trim_end().ends_with('❯')`.
        // Confirm a prompt glyph drawn at the end of a line is the last
        // non-blank char in the snapshot.
        let mut grid = TerminalGrid::new(3, 20);
        grid.feed("\u{276f} ".as_bytes()); // "❯ "
        let owned = grid.snapshot();
        assert!(
            owned
                .text
                .lines()
                .next()
                .unwrap()
                .trim_end()
                .ends_with('\u{276f}'),
            "prompt glyph should be the visible tail, got {:?}",
            owned.text
        );
    }
}
