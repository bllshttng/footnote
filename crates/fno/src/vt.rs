//! Server-side VT emulation: PTY bytes -> a styled cell grid -> `proto::Frame`.
//!
//! Seeded from `crates/fno-agents/src/screen.rs` (headless
//! `Term<VoidListener>` + `vte::ansi::Processor`) and its grid compositor's
//! cell mapping, adapted for the mux: alt-screen is exercised (vim under
//! detach/reattach, AC3-EDGE), scrollback is bounded (10k lines - a cap, not
//! an upfront allocation, so server memory per pane is bounded), and the
//! snapshot is a full styled [`Frame`], not trimmed text. The server grid is
//! the single source of truth; the client never emulates VT itself.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VtColor, NamedColor, Processor, Rgb};

use crate::proto::{cell_flags, Cell, Color, Frame};

/// Default grid until the first client reports its real size. 24x80 is the
/// historical terminal default (matches the fno-agents drive fallback).
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Scrollback cap per pane. `Term::new` treats `scrolling_history` as a bound
/// the history grows toward, not an allocation.
const SCROLLBACK_LINES: usize = 10_000;

/// `Dimensions` impl for constructing/resizing the headless [`Term`].
#[derive(Debug, Clone, Copy)]
struct GridSize {
    rows: usize,
    cols: usize,
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

/// The one Phase-1 pane: a terminal grid fed raw PTY bytes.
pub struct Pane {
    term: Term<VoidListener>,
    processor: Processor,
    rows: u16,
    cols: u16,
}

impl Pane {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        Pane {
            term: Term::new(
                config,
                &GridSize {
                    rows: rows as usize,
                    cols: cols as usize,
                },
                VoidListener,
            ),
            processor: Processor::new(),
            rows,
            cols,
        }
    }

    /// Feed raw PTY output. Safe with partial escape sequences split across
    /// reads; `vte` buffers incomplete sequences between calls. Malformed
    /// sequences are the parser's problem (bounded parse, no panic).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Resize the grid, mirroring a PTY winsize change. Clamped to 1 so a
    /// degenerate size can never panic the parser.
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

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Snapshot the ACTIVE grid (alt screen included - `Term` swaps grids
    /// internally, so vim's screen is what a reattaching client redraws from)
    /// into a self-contained [`Frame`].
    pub fn frame(&self) -> Frame {
        let grid = self.term.grid();
        let cursor_point = grid.cursor.point;
        let mut cells = Vec::with_capacity(self.rows as usize * self.cols as usize);
        for r in 0..(self.rows as usize) {
            let line = Line(r as i32);
            for c in 0..(self.cols as usize) {
                let cell = &grid[line][Column(c)];
                cells.push(map_cell(cell.c, cell.fg, cell.bg, cell.flags));
            }
        }
        Frame {
            rows: self.rows,
            cols: self.cols,
            cells,
            cursor_row: cursor_point.line.0.max(0) as u16,
            cursor_col: cursor_point.column.0 as u16,
            cursor_visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
        }
    }

    /// The visible screen as trimmed plain text (test/debug helper; the wire
    /// format is [`Pane::frame`]).
    #[allow(dead_code)]
    pub fn text(&self) -> String {
        let frame = self.frame();
        frame_text(&frame)
    }
}

/// Render a [`Frame`]'s cells as trimmed plain text, one line per row with
/// trailing blanks trimmed and trailing blank rows dropped. Shared by tests
/// on both the server and client side of the wire.
pub fn frame_text(frame: &Frame) -> String {
    let mut rows: Vec<String> = Vec::with_capacity(frame.rows as usize);
    for r in 0..(frame.rows as usize) {
        let mut row = String::with_capacity(frame.cols as usize);
        for c in 0..(frame.cols as usize) {
            let cell = &frame.cells[r * frame.cols as usize + c];
            if cell.flags & cell_flags::WIDE_SPACER == 0 {
                row.push(cell.c);
            }
        }
        while row.ends_with(' ') {
            row.pop();
        }
        rows.push(row);
    }
    while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
        rows.pop();
    }
    rows.join("\n")
}

fn map_cell(c: char, fg: VtColor, bg: VtColor, flags: Flags) -> Cell {
    let mut f = 0u8;
    // BOLD_ITALIC sets both bits; DIM_BOLD sets BOLD - `contains` covers them.
    if flags.contains(Flags::BOLD) {
        f |= cell_flags::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        f |= cell_flags::ITALIC;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        f |= cell_flags::UNDERLINE;
    }
    if flags.contains(Flags::INVERSE) {
        f |= cell_flags::INVERSE;
    }
    if flags.contains(Flags::DIM) {
        f |= cell_flags::DIM;
    }
    // The second cell of a wide glyph: the client must skip it so the glyph's
    // right half is never overdrawn.
    if flags.contains(Flags::WIDE_CHAR_SPACER) || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        f |= cell_flags::WIDE_SPACER;
    }
    let c = if flags.contains(Flags::HIDDEN) {
        ' '
    } else {
        c
    };
    Cell {
        c,
        fg: map_color(fg),
        bg: map_color(bg),
        flags: f,
    }
}

/// Same folding as the fno-agents compositor: concrete indices and true-color
/// pass through; the 16 ANSI names fold to their palette index; semantic
/// names (Foreground/Background/Cursor/...) map to `Default`.
fn map_color(c: VtColor) -> Color {
    use NamedColor::*;
    match c {
        VtColor::Indexed(i) => Color::Indexed(i),
        VtColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        VtColor::Named(named) => {
            let idx: u8 = match named {
                Black | DimBlack => 0,
                Red | DimRed => 1,
                Green | DimGreen => 2,
                Yellow | DimYellow => 3,
                Blue | DimBlue => 4,
                Magenta | DimMagenta => 5,
                Cyan | DimCyan => 6,
                White | DimWhite => 7,
                BrightBlack => 8,
                BrightRed => 9,
                BrightGreen => 10,
                BrightYellow => 11,
                BrightBlue => 12,
                BrightMagenta => 13,
                BrightCyan => 14,
                BrightWhite => 15,
                Foreground | Background | Cursor | BrightForeground | DimForeground => {
                    return Color::Default
                }
            };
            Color::Indexed(idx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_spine_vt_renders_text_and_cursor() {
        let mut pane = Pane::new(24, 80);
        pane.feed(b"hello");
        let frame = pane.frame();
        assert_eq!(frame_text(&frame), "hello");
        assert_eq!(frame.cursor_row, 0);
        assert_eq!(frame.cursor_col, 5);
        assert!(frame.cursor_visible);
        assert_eq!(frame.cells.len(), 24 * 80);
    }

    #[test]
    fn server_spine_vt_styles_map_to_cell_flags_and_colors() {
        let mut pane = Pane::new(4, 20);
        // Bold red fg on indexed-blue bg, then reset.
        pane.feed(b"\x1b[1;31;44mX\x1b[0m");
        let frame = pane.frame();
        let x = &frame.cells[0];
        assert_eq!(x.c, 'X');
        assert_eq!(x.flags & cell_flags::BOLD, cell_flags::BOLD);
        assert_eq!(x.fg, Color::Indexed(1));
        assert_eq!(x.bg, Color::Indexed(4));
    }

    #[test]
    fn server_spine_vt_true_color_passes_through() {
        let mut pane = Pane::new(2, 10);
        pane.feed(b"\x1b[38;2;10;20;30mZ");
        let frame = pane.frame();
        assert_eq!(frame.cells[0].fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn server_spine_vt_alt_screen_swaps_and_restores() {
        let mut pane = Pane::new(4, 20);
        pane.feed(b"main-screen");
        // Enter the alternate screen (what vim does), draw, snapshot.
        pane.feed(b"\x1b[?1049h\x1b[2J\x1b[HALT");
        let alt = pane.frame();
        assert!(
            frame_text(&alt).starts_with("ALT"),
            "{:?}",
            frame_text(&alt)
        );
        // Leave: the main screen content is restored.
        pane.feed(b"\x1b[?1049l");
        let main = pane.frame();
        assert!(
            frame_text(&main).contains("main-screen"),
            "{:?}",
            frame_text(&main)
        );
    }

    #[test]
    fn server_spine_vt_cursor_visibility_tracks_dectcem() {
        let mut pane = Pane::new(4, 20);
        pane.feed(b"\x1b[?25l");
        assert!(!pane.frame().cursor_visible);
        pane.feed(b"\x1b[?25h");
        assert!(pane.frame().cursor_visible);
    }

    #[test]
    fn server_spine_vt_wide_glyph_spacer_is_flagged_and_skipped() {
        let mut pane = Pane::new(2, 10);
        pane.feed("宽x".as_bytes());
        let frame = pane.frame();
        assert_eq!(frame.cells[0].c, '宽');
        assert_eq!(
            frame.cells[1].flags & cell_flags::WIDE_SPACER,
            cell_flags::WIDE_SPACER,
            "second cell of a wide glyph must be a flagged spacer"
        );
        // The text projection skips the spacer: 宽 then x, no phantom gap.
        assert_eq!(frame_text(&frame), "宽x");
    }

    #[test]
    fn server_spine_vt_resize_is_clamped_and_safe() {
        let mut pane = Pane::new(24, 80);
        pane.resize(0, 0);
        pane.feed(b"q");
        assert_eq!(pane.size(), (1, 1));
        let frame = pane.frame();
        assert_eq!(frame.cells.len(), 1);
    }
}
