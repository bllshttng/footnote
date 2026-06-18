//! Per-pane render core (ab-3c063856, Wave 2.1).
//!
//! A [`Pane`] owns an alacritty [`Term`] (fed via a `vte` [`Processor`])
//! with raw PTY bytes from a watcher WebSocket and emits a [`PaneSnapshot`]
//! the compositor's renderer paints inside the pane's tile rect. Cell
//! extraction is intentionally cell-by-cell: the per-pane rect frequently
//! differs from the agent's PTY size (the winsize push in Wave 4.2 only
//! kicks in when the compositor holds the claim or is the agent's sole
//! connection), so a whole-row passthrough would clip incorrectly in the
//! contended case. Going through a renderer-agnostic [`RenderCell`] also
//! lets the compositor draw chrome (borders, title, mode indicator) around
//! each row at paint time without re-parsing escape sequences.
//!
//! ## Substrate
//!
//! Same `alacritty_terminal` grid the readiness seam ([`crate::screen`])
//! uses - shared `GridSize` + `visible_only_config`. See [`crate::grid`]
//! for why the whole crate standardized on alacritty over vt100.

use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

use crate::grid::state::{ConnState, ScrollCmd};
use crate::readiness::ScreenView;
use crate::screen::{scrollback_config, GridSize};

/// Per-pane scrollback retention (lines kept above the visible screen). Grid
/// panes only - the readiness seam keeps zero history. A cap, not an upfront
/// allocation; 1000 lines is ample for "how did this agent get here" without
/// holding alacritty's 10k default times the whole fleet in memory.
pub(crate) const SCROLLBACK_LINES: usize = 1000;

/// Render-cell payload the compositor paints into a tile.
///
/// One per visible cell, indexed by `row * cols + col` in
/// [`PaneSnapshot::cells`]. The text field carries the cell's exact glyph
/// contents (a `&str` originally - owned here because [`PaneSnapshot`]
/// outlives the underlying parser borrow). Wide / combining characters
/// are preserved verbatim; the painter is responsible for honoring cell
/// width when it lays the row down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCell {
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// True for the second column of a wide (CJK/emoji) glyph. The painter
    /// emits nothing for it - the wide glyph in the preceding cell already
    /// spans both columns. Distinct from a blank cell (empty `text`,
    /// `wide_spacer == false`), which the painter renders as a space so the
    /// diff can erase stale content.
    pub wide_spacer: bool,
}

impl Default for RenderCell {
    fn default() -> Self {
        RenderCell {
            text: String::new(),
            fg: CellColor::Default,
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            wide_spacer: false,
        }
    }
}

/// Renderer-agnostic color. Mirrors [`vt100::Color`] without leaking the
/// parser type past this module's API (the compositor will translate to
/// [`crossterm::style::Color`] at paint time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellColor {
    #[default]
    Default,
    /// 256-color palette index, including the 8 + 8 ANSI colors at indices
    /// 0..16 (vt100 reports these as `Idx` rather than as discrete enum
    /// variants).
    Indexed(u8),
    /// True-color RGB.
    Rgb(u8, u8, u8),
}

impl From<Color> for CellColor {
    fn from(c: Color) -> Self {
        match c {
            // Concrete 256-palette index and true-color pass through.
            Color::Indexed(i) => CellColor::Indexed(i),
            Color::Spec(Rgb { r, g, b }) => CellColor::Rgb(r, g, b),
            // alacritty reports the 16 ANSI colors as `Named` rather than
            // `Indexed`. Fold them back to their palette index (0..=15) so
            // the compositor's crossterm translation is uniform and so the
            // CellColor enum stays small. Semantic names (Foreground /
            // Background / Cursor / Bright-/DimForeground) have no concrete
            // index - they map to Default, and the painter uses the
            // terminal's default fg/bg there.
            Color::Named(named) => named_to_cell_color(named),
        }
    }
}

/// Map an alacritty [`NamedColor`] to a [`CellColor`]. The 16 ANSI colors
/// fold to their palette index (Black=0 .. White=7, Bright* = 8..15); the
/// `Dim*` variants collapse to their base index (we do not carry a dim
/// attribute through `RenderCell`); semantic names map to `Default`.
fn named_to_cell_color(named: NamedColor) -> CellColor {
    use NamedColor::*;
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
        // No concrete palette index - defer to the terminal default.
        Foreground | Background | Cursor | BrightForeground | DimForeground => {
            return CellColor::Default
        }
    };
    CellColor::Indexed(idx)
}

/// Owned snapshot of a pane's current screen, sized exactly `rows × cols`.
/// The compositor reads this in the render loop and clips to the tile rect
/// the layout manager (Wave 2.2) hands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Display offset at snapshot time (lines above the live tail; 0 == live).
    /// Carried so the compositor can render the scrollback affordance (footer
    /// indicator + focused-pane title badge) without re-borrowing the pane.
    pub scroll_offset: usize,
    /// Flattened row-major cell grid; length is exactly `rows * cols`.
    pub cells: Vec<RenderCell>,
}

impl PaneSnapshot {
    /// Look up a cell by `(row, col)`. Returns `None` for out-of-bounds.
    pub fn cell(&self, row: u16, col: u16) -> Option<&RenderCell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells
            .get((row as usize) * (self.cols as usize) + (col as usize))
    }
}

/// Ownership predicate gating the `{"t":"resize"}` push to the agent.
///
/// Locked Decision #4: push the tile-rect winsize **only** when the
/// compositor holds the driver claim OR is the agent's sole connection.
/// Otherwise tail-clip the render and leave the agent's winsize alone,
/// because resizing an agent another viewer is also watching would steal
/// their fit. The compositor labels the pane `tailing (busy elsewhere)`
/// in that branch.
///
/// Either path keeps the operator's view of the agent legible without
/// stomping a co-viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinsizePolicy {
    pub holds_claim: bool,
    pub sole_connection: bool,
}

impl WinsizePolicy {
    /// True when the compositor may push a winsize change to the agent.
    pub fn may_resize(&self) -> bool {
        self.holds_claim || self.sole_connection
    }
}

/// Clip rectangle for the tail-clip branch (`may_resize == false`). The
/// compositor reads the agent's snapshot at its current winsize and paints
/// the bottom `rows` × top-left `cols` window so the prompt and cursor
/// stay visible. Coordinates are within the agent's screen, not the
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailClip {
    pub row_start: u16,
    pub col_start: u16,
    pub rows: u16,
    pub cols: u16,
}

/// Compute the tail-clip window for a `snap_rows × snap_cols` agent
/// screen rendered into a `tile_rows × tile_cols` tile. When the agent
/// is taller than the tile, take the bottom `tile_rows` rows so the
/// prompt / cursor stays visible (Locked Decision #4); when narrower,
/// pass through the entire screen.
pub fn tail_clip(snap_rows: u16, snap_cols: u16, tile_rows: u16, tile_cols: u16) -> TailClip {
    let rows = tile_rows.min(snap_rows);
    let cols = tile_cols.min(snap_cols);
    let row_start = snap_rows.saturating_sub(rows);
    TailClip {
        row_start,
        col_start: 0,
        rows,
        cols,
    }
}

/// Trailing-edge debouncer for SIGWINCH-driven winsize pushes.
///
/// AC5-FR: a resize storm (operator dragging the terminal window)
/// coalesces into a single push per agent rather than spamming one push
/// per SIGWINCH event. Each [`schedule`] call resets the deadline to
/// `now + delay`; [`poll`] yields `Some(rect)` exactly once when the
/// deadline elapses and clears the pending state.
///
/// Pure FSM, no actual timer. The run loop drives it from a tick
/// (e.g. `tokio::time::interval`) plus the SIGWINCH branch of its
/// select.
#[derive(Debug, Clone)]
pub struct WinsizeDebouncer {
    pending: Option<(u16, u16)>,
    deadline: Option<Instant>,
    delay: Duration,
}

impl WinsizeDebouncer {
    /// Default 150ms trailing-edge window - the plan's suggested value.
    pub const DEFAULT_DELAY: Duration = Duration::from_millis(150);

    pub fn new() -> Self {
        Self::with_delay(Self::DEFAULT_DELAY)
    }

    pub fn with_delay(delay: Duration) -> Self {
        WinsizeDebouncer {
            pending: None,
            deadline: None,
            delay,
        }
    }

    /// Schedule (or re-schedule) a push of `rect = (rows, cols)`. Resets
    /// the trailing-edge deadline to `now + delay`. Subsequent
    /// schedule() calls before the deadline elapse drop the previously
    /// pending rect and re-arm the timer.
    pub fn schedule(&mut self, rect: (u16, u16), now: Instant) {
        self.pending = Some(rect);
        self.deadline = Some(now + self.delay);
    }

    /// Poll the debouncer. Returns the pending rect exactly once when
    /// the deadline has elapsed, then clears state until the next
    /// schedule().
    pub fn poll(&mut self, now: Instant) -> Option<(u16, u16)> {
        let deadline = self.deadline?;
        if now >= deadline {
            let rect = self.pending.take();
            self.deadline = None;
            rect
        } else {
            None
        }
    }
}

impl Default for WinsizeDebouncer {
    fn default() -> Self {
        Self::new()
    }
}

/// One agent's render state. Holds the parser, the per-pane winsize
/// debouncer, and the operator's pre-grid winsize baseline (for
/// best-effort restore on grid exit). The compositor's run loop owns
/// feeding watcher bytes, polling the debouncer, and reading snapshots
/// each frame tick.
pub struct Pane {
    term: Term<VoidListener>,
    processor: Processor,
    rows: u16,
    cols: u16,
    /// First observed winsize from the agent's `state.json` before the
    /// grid resized it. Restored best-effort on grid exit so leaving the
    /// compositor does not leave the agent reflowed (Invariants).
    prior_winsize: Option<(u16, u16)>,
    /// SIGWINCH debouncer; the run loop polls this each tick and emits
    /// a `{"t":"resize"}` push when it fires.
    winsize_debouncer: WinsizeDebouncer,
}

impl Pane {
    /// Construct a pane sized to the tile rect (or to the default 24x80
    /// when the layout has not yet decided). Zero dimensions are clamped
    /// to 1, mirroring [`crate::screen::TerminalGrid::new`].
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let size = GridSize {
            rows: rows as usize,
            cols: cols as usize,
        };
        Pane {
            // Retain SCROLLBACK_LINES of history so the compositor's scrollback
            // mode can scroll back through this pane's captured output. The
            // visible screen still renders by default (display offset 0);
            // history is only consulted when the operator scrolls.
            term: Term::new(scrollback_config(SCROLLBACK_LINES), &size, VoidListener),
            processor: Processor::new(),
            rows,
            cols,
            prior_winsize: None,
            winsize_debouncer: WinsizeDebouncer::new(),
        }
    }

    /// Construct a pane at the default 24x80 size (used before the first
    /// SIGWINCH / tile-rect computation arrives).
    pub fn with_default_size() -> Self {
        Self::new(crate::screen::DEFAULT_ROWS, crate::screen::DEFAULT_COLS)
    }

    /// Current pane size in `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Feed raw PTY bytes from the watcher WS. Partial escape sequences
    /// split across reads are safe - `vt100::Parser` buffers incomplete
    /// sequences internally (mirroring `screen.rs`'s precedent).
    pub fn feed(&mut self, bytes: &[u8]) {
        // `term` and `processor` are disjoint fields; both mutable borrows
        // are allowed. The `vte` parser buffers partial escape sequences
        // across feeds.
        self.processor.advance(&mut self.term, bytes);
    }

    /// Resize the pane (called from the layout manager on SIGWINCH or when
    /// a pane enters / leaves the grid). Dimensions are clamped to 1.
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

    /// Record the agent's pre-grid winsize for best-effort restore on
    /// grid exit. Only the FIRST recorded winsize is preserved; later
    /// calls are layout adjustments, not the operator's baseline.
    pub fn record_prior_winsize(&mut self, rows: u16, cols: u16) {
        if self.prior_winsize.is_none() {
            self.prior_winsize = Some((rows, cols));
        }
    }

    /// The agent's pre-grid winsize, if it was recorded.
    pub fn prior_winsize(&self) -> Option<(u16, u16)> {
        self.prior_winsize
    }

    /// Schedule a debounced winsize push to the agent's PTY. The run
    /// loop calls this on SIGWINCH / layout change and then polls
    /// [`poll_pending_winsize`] each tick to fire the push.
    pub fn schedule_winsize_push(&mut self, rows: u16, cols: u16, now: Instant) {
        self.winsize_debouncer.schedule((rows, cols), now);
    }

    /// Poll the winsize debouncer. Returns the pending `(rows, cols)`
    /// once when the trailing-edge deadline elapses.
    pub fn poll_pending_winsize(&mut self, now: Instant) -> Option<(u16, u16)> {
        self.winsize_debouncer.poll(now)
    }

    /// Build the visible screen as plain text - one line per grid row with
    /// trailing blank cells trimmed and trailing blank rows dropped, exactly
    /// matching [`crate::screen::TerminalGrid::snapshot`]'s `text` semantics
    /// so the readiness check sees what the daemon would. Used by
    /// [`is_waiting`](Pane::is_waiting) (fu-grid-pagination, task 3.1).
    fn visible_text(&self) -> String {
        // Single allocation: build directly into one pre-sized buffer rather
        // than a String-per-row Vec + join. is_waiting() calls this for every
        // pane each dirty tick (eager), so the per-row allocs added up
        // (gemini-code-assist, PR #374). Semantics still match
        // `screen::TerminalGrid::snapshot`: per-row trailing blanks trimmed,
        // trailing blank rows dropped, rows joined by '\n'.
        let grid = self.term.grid();
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let mut text = String::with_capacity(rows * (cols + 1));
        for r in 0..rows {
            let line = Line(r as i32);
            let row_start = text.len();
            for c in 0..cols {
                text.push(grid[line][Column(c)].c);
            }
            // Strip this row's trailing blanks without crossing into the prior row.
            while text.len() > row_start && text.ends_with(' ') {
                text.pop();
            }
            text.push('\n');
        }
        // Drop trailing blank rows + the final separator (a trailing blank row
        // contributes just a '\n', so trimming trailing newlines covers both).
        while text.ends_with('\n') {
            text.pop();
        }
        text
    }

    /// Whether this pane's agent is waiting for input - the attention-scanner
    /// signal that raises an off-screen badge (fu-grid-pagination, task 3.1).
    ///
    /// Runs the shared prompt-glyph readiness check
    /// ([`crate::readiness::screen_is_waiting`]) on the pane's client-side
    /// `Term` snapshot - the SAME logic the daemon uses (`screen.rs` tail
    /// match), so no daemon protocol change is needed (grid Locked Decision
    /// 4). Gated on `conn.is_scannable()`: only live `Watching`/`Driving`
    /// panes are scanned; an `exited`/`disconnected` frozen frame returns
    /// false so its trailing glyph cannot false-positive (Domain Pitfall).
    /// `host_mode`/interactive panes (ab-26b5fe82) scan the same way - the
    /// daemon-side readiness-dwell refinement is not in this seam.
    pub fn is_waiting(&self, conn: &ConnState) -> bool {
        if !conn.is_scannable() {
            return false;
        }
        let text = self.visible_text();
        let view = ScreenView {
            visible_text: &text,
            // prompt_ready inspects only visible_text; cursor is unused here.
            cursor_row: 0,
            cursor_col: 0,
        };
        crate::readiness::screen_is_waiting(&view)
    }

    /// Read the current screen as an owned snapshot. Allocates one
    /// [`RenderCell`] per visible cell; the compositor calls this once per
    /// frame tick per pane, then clips to the tile rect.
    pub fn snapshot(&self) -> PaneSnapshot {
        let grid = self.term.grid();
        let cursor_point = grid.cursor.point;
        // alacritty reports the cursor 0-indexed via the inner `.0`.
        let cur_row = cursor_point.line.0.max(0) as u16;
        let cur_col = cursor_point.column.0 as u16;

        // Honor the display offset so scrollback mode renders the scrolled
        // region. `grid[Line(r)]` is viewport-relative but does NOT fold in
        // the display offset (verified: Storage::compute_index ignores it);
        // the displayed viewport starts at Line(-display_offset), so display
        // row r maps to Line(r - offset). At offset 0 this is the live screen
        // (unchanged behavior). The offset is pre-clamped to history_size() by
        // scroll_display, so the negative line indices stay within valid
        // storage.
        let offset = grid.display_offset() as i32;
        let mut cells = Vec::with_capacity((self.rows as usize) * (self.cols as usize));
        for r in 0..(self.rows as usize) {
            let line = Line(r as i32 - offset);
            for c in 0..(self.cols as usize) {
                let cell = &grid[line][Column(c)];
                let flags = cell.flags;
                // A wide-character spacer (the second cell of a CJK / emoji
                // glyph) carries a placeholder; render it as a blank so the
                // painter does not double-draw the glyph. The wide glyph
                // itself lives in the preceding cell.
                let wide_spacer = flags.contains(Flags::WIDE_CHAR_SPACER);
                let text = if wide_spacer {
                    String::new()
                } else {
                    cell.c.to_string()
                };
                cells.push(RenderCell {
                    text,
                    fg: cell.fg.into(),
                    bg: cell.bg.into(),
                    // BOLD_ITALIC sets the BOLD bit and DIM_BOLD sets it too,
                    // so `contains(BOLD)` covers every bold variant; likewise
                    // ITALIC is set within BOLD_ITALIC.
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: flags.contains(Flags::INVERSE),
                    // The painter skips spacers so a wide glyph's right half is
                    // never overwritten on the diff path (chatgpt-codex PR #386).
                    wide_spacer,
                });
            }
        }
        PaneSnapshot {
            rows: self.rows,
            cols: self.cols,
            cursor_row: cur_row,
            cursor_col: cur_col,
            scroll_offset: offset.max(0) as usize,
            cells,
        }
    }

    /// Apply a scroll command to this pane's terminal (scrollback mode). The
    /// resulting display offset is clamped to the retained history by
    /// alacritty, so over-scrolling at either boundary is a safe no-op.
    pub fn apply_scroll(&mut self, cmd: ScrollCmd) {
        let scroll = match cmd {
            // Positive delta scrolls toward older lines (up); negative toward
            // the live tail (down). Page/Top/Bottom map straight through.
            ScrollCmd::LineUp => Scroll::Delta(1),
            ScrollCmd::LineDown => Scroll::Delta(-1),
            ScrollCmd::PageUp => Scroll::PageUp,
            ScrollCmd::PageDown => Scroll::PageDown,
            ScrollCmd::Top => Scroll::Top,
            ScrollCmd::Bottom => Scroll::Bottom,
        };
        self.term.scroll_display(scroll);
    }

    /// Current display offset in lines above the live tail (0 == live). The
    /// footer renders this as the scrollback position indicator.
    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines of scrollback history currently retained. 0 means nothing has
    /// scrolled off yet, so scrollback mode has nothing to show - the run loop
    /// uses this to gate entry (and surface a "no history" hint instead).
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC1-UI / Locked Decision #3 fallback: a plain character feed renders
    /// into the cell grid at the expected position.
    #[test]
    fn feed_then_snapshot_returns_cells() {
        let mut pane = Pane::new(2, 4);
        pane.feed(b"AB\r\nCD");
        let snap = pane.snapshot();
        assert_eq!(snap.rows, 2);
        assert_eq!(snap.cols, 4);
        assert_eq!(snap.cell(0, 0).unwrap().text, "A");
        assert_eq!(snap.cell(0, 1).unwrap().text, "B");
        assert_eq!(snap.cell(1, 0).unwrap().text, "C");
        assert_eq!(snap.cell(1, 1).unwrap().text, "D");
        // Cursor sits one past 'D' on row 1.
        assert_eq!(snap.cursor_row, 1);
        assert_eq!(snap.cursor_col, 2);
    }

    /// AC1-UI: ANSI escape sequences must be **interpreted** by vt100, not
    /// echoed into cell contents. Mirrors `screen::tests::ansi_escapes_are_interpreted_not_echoed`.
    #[test]
    fn ansi_escapes_are_interpreted_not_echoed() {
        let mut pane = Pane::new(4, 20);
        // Write "AAA", then cursor-home + clear-line, then "B".
        pane.feed(b"AAA\x1b[1;1H\x1b[2KB");
        let snap = pane.snapshot();
        for cell in &snap.cells {
            assert!(
                !cell.text.contains('\x1b'),
                "raw escape leaked into a cell: {:?}",
                cell.text
            );
        }
        assert_eq!(snap.cell(0, 0).unwrap().text, "B");
    }

    /// AC1-FR / "partial-escape split safety" - Domain Pitfall + screen.rs
    /// precedent: a CSI sequence split across two feeds is still parsed
    /// correctly because vt100 buffers incomplete escapes.
    #[test]
    fn partial_escape_split_across_feeds_is_buffered() {
        let mut pane = Pane::new(4, 20);
        pane.feed(b"X\x1b[1");
        pane.feed(b";1HY");
        let snap = pane.snapshot();
        // The completed home sequence positions back to (0,0) and writes 'Y'.
        assert_eq!(snap.cell(0, 0).unwrap().text, "Y");
    }

    /// SGR foreground / background color extraction. Red on default, then
    /// reset, then default-on-blue.
    #[test]
    fn foreground_and_background_colors_propagate() {
        let mut pane = Pane::new(1, 4);
        // ESC[31m R ESC[0m  ESC[44m B
        pane.feed(b"\x1b[31mR\x1b[0m\x1b[44mB");
        let snap = pane.snapshot();
        let r = snap.cell(0, 0).unwrap();
        assert_eq!(r.text, "R");
        assert_eq!(r.fg, CellColor::Indexed(1));
        assert_eq!(r.bg, CellColor::Default);
        let b = snap.cell(0, 1).unwrap();
        assert_eq!(b.text, "B");
        assert_eq!(b.fg, CellColor::Default);
        assert_eq!(b.bg, CellColor::Indexed(4));
    }

    /// True-color (24-bit) SGR. vt100 reports these as `Color::Rgb`.
    #[test]
    fn truecolor_rgb_propagates() {
        let mut pane = Pane::new(1, 1);
        pane.feed(b"\x1b[38;2;10;20;30mX");
        let snap = pane.snapshot();
        let x = snap.cell(0, 0).unwrap();
        assert_eq!(x.text, "X");
        assert_eq!(x.fg, CellColor::Rgb(10, 20, 30));
    }

    /// Per the plan's Discretion #3: cover bold / underline / inverse at a
    /// minimum. Italic is covered alongside since vt100 already exposes it.
    #[test]
    fn bold_italic_underline_inverse_propagate() {
        let mut pane = Pane::new(1, 4);
        pane.feed(b"\x1b[1mB\x1b[22m\x1b[3mI\x1b[23m\x1b[4mU\x1b[24m\x1b[7mR");
        let snap = pane.snapshot();
        assert!(snap.cell(0, 0).unwrap().bold);
        assert!(snap.cell(0, 1).unwrap().italic);
        assert!(snap.cell(0, 2).unwrap().underline);
        assert!(snap.cell(0, 3).unwrap().inverse);
        // Each attr clears at its dedicated reset, so adjacent cells are clean.
        assert!(!snap.cell(0, 1).unwrap().bold);
        assert!(!snap.cell(0, 2).unwrap().italic);
        assert!(!snap.cell(0, 3).unwrap().underline);
    }

    /// Zero dimensions clamp to 1 (mirrors `screen::TerminalGrid::new`), so
    /// a degenerate winsize from the layout manager cannot panic the
    /// renderer.
    #[test]
    fn zero_dimensions_clamp_to_one() {
        let pane = Pane::new(0, 0);
        assert_eq!(pane.size(), (1, 1));
        let snap = pane.snapshot();
        assert_eq!(snap.rows, 1);
        assert_eq!(snap.cols, 1);
        assert_eq!(snap.cells.len(), 1);
    }

    /// Resize updates both the parser screen and the cached dimensions so
    /// the next snapshot is sized correctly.
    #[test]
    fn resize_grows_and_shrinks() {
        let mut pane = Pane::new(2, 4);
        pane.feed(b"AB\r\nCD");
        pane.resize(4, 8);
        assert_eq!(pane.size(), (4, 8));
        let snap = pane.snapshot();
        assert_eq!(snap.rows, 4);
        assert_eq!(snap.cols, 8);
        // Cell count matches the new grid.
        assert_eq!(snap.cells.len(), 32);

        pane.resize(1, 2);
        let snap = pane.snapshot();
        assert_eq!(snap.cells.len(), 2);
    }

    /// Default-size constructor matches the readiness seam's defaults so
    /// pre-SIGWINCH panes look identical to the daemon's grids.
    #[test]
    fn default_size_matches_screen_defaults() {
        let pane = Pane::with_default_size();
        assert_eq!(
            pane.size(),
            (crate::screen::DEFAULT_ROWS, crate::screen::DEFAULT_COLS)
        );
    }

    /// Out-of-bounds cell access returns None rather than panicking - the
    /// renderer must never crash on a stale row/col index.
    #[test]
    fn out_of_bounds_cell_access_returns_none() {
        let pane = Pane::new(2, 3);
        let snap = pane.snapshot();
        assert!(snap.cell(2, 0).is_none());
        assert!(snap.cell(0, 3).is_none());
        assert!(snap.cell(0, 0).is_some());
    }

    // ---- fu-grid-pagination task 3.1: attention scanner is_waiting ----

    /// AC2-HP: a live `Watching` pane whose tail ends in a prompt glyph is
    /// waiting for input.
    #[test]
    fn is_waiting_true_on_prompt_glyph_when_watching() {
        let mut pane = Pane::new(3, 20);
        pane.feed("codex 0.130\nbuild X\n\u{276f} ".as_bytes());
        assert!(pane.is_waiting(&ConnState::Watching));
    }

    /// A live pane with no prompt glyph (mid-render) is not waiting.
    #[test]
    fn is_waiting_false_without_prompt_glyph() {
        let mut pane = Pane::new(3, 20);
        pane.feed(b"loading a long banner of text");
        assert!(!pane.is_waiting(&ConnState::Watching));
    }

    /// AC2-ERR / Domain Pitfall: an EXITED pane's frozen last frame ending in
    /// a prompt glyph must NOT count as waiting (false-badge guard).
    #[test]
    fn is_waiting_false_when_exited_even_with_glyph() {
        let mut pane = Pane::new(3, 20);
        pane.feed("done\n\u{276f} ".as_bytes()); // frozen frame ends in glyph
        assert!(
            !pane.is_waiting(&ConnState::Exited { code: 0 }),
            "exited pane must not raise a waiting badge"
        );
        assert!(!pane.is_waiting(&ConnState::Disconnected {
            reason: "lost".into()
        }));
        // Same frame, but live → it IS waiting (proves the gate is the cause).
        assert!(pane.is_waiting(&ConnState::Watching));
    }

    /// A busy/auth wall on a live pane is not waiting (shared readiness
    /// discipline carries through).
    #[test]
    fn is_waiting_false_on_busy_or_wall() {
        let mut pane = Pane::new(4, 30);
        pane.feed("running tool\nEsc to interrupt\n\u{276f} ".as_bytes());
        assert!(!pane.is_waiting(&ConnState::Watching));
    }

    // ---- Wave 4.2 tests: winsize policy + debouncer + tail-clip ----

    /// Locked Decision #4: may_resize is true only when the compositor
    /// holds the driver claim OR is the agent's sole connection.
    #[test]
    fn winsize_policy_holds_claim_may_resize() {
        let p = WinsizePolicy {
            holds_claim: true,
            sole_connection: false,
        };
        assert!(p.may_resize());
    }

    #[test]
    fn winsize_policy_sole_connection_may_resize() {
        let p = WinsizePolicy {
            holds_claim: false,
            sole_connection: true,
        };
        assert!(p.may_resize());
    }

    #[test]
    fn winsize_policy_neither_may_not_resize() {
        let p = WinsizePolicy {
            holds_claim: false,
            sole_connection: false,
        };
        assert!(!p.may_resize());
    }

    #[test]
    fn winsize_policy_both_may_resize() {
        let p = WinsizePolicy {
            holds_claim: true,
            sole_connection: true,
        };
        assert!(p.may_resize());
    }

    /// AC5-FR: a resize storm coalesces into a single trailing-edge push
    /// per agent. Multiple schedule() calls before the deadline elapse
    /// all reset the timer; only the last rect ever fires.
    #[test]
    fn winsize_debouncer_coalesces_storm() {
        let t0 = Instant::now();
        let delay = Duration::from_millis(150);
        let mut deb = WinsizeDebouncer::with_delay(delay);
        // Storm of three schedules within a single tick window.
        deb.schedule((24, 80), t0);
        deb.schedule((30, 100), t0 + Duration::from_millis(50));
        deb.schedule((40, 120), t0 + Duration::from_millis(100));
        // Poll just before the deadline relative to the LAST schedule:
        // last schedule was at t0+100ms; deadline = t0+250ms.
        assert!(deb.poll(t0 + Duration::from_millis(200)).is_none());
        assert!(deb.poll(t0 + Duration::from_millis(240)).is_none());
        // After the deadline, the LAST scheduled rect fires once.
        assert_eq!(deb.poll(t0 + Duration::from_millis(260)), Some((40, 120)));
        // Subsequent polls without new schedules return None.
        assert!(deb.poll(t0 + Duration::from_millis(500)).is_none());
    }

    /// Debouncer with no pending schedule returns None on every poll.
    #[test]
    fn winsize_debouncer_idle_returns_none() {
        let mut deb = WinsizeDebouncer::new();
        let now = Instant::now();
        assert!(deb.poll(now).is_none());
        assert!(deb.poll(now + Duration::from_secs(10)).is_none());
    }

    /// A new schedule after a previous fire starts a fresh debounce window.
    #[test]
    fn winsize_debouncer_re_arms_after_fire() {
        let t0 = Instant::now();
        let delay = Duration::from_millis(50);
        let mut deb = WinsizeDebouncer::with_delay(delay);
        deb.schedule((24, 80), t0);
        assert_eq!(deb.poll(t0 + Duration::from_millis(60)), Some((24, 80)));
        // Re-arm for a different rect.
        deb.schedule((30, 100), t0 + Duration::from_millis(200));
        assert!(deb.poll(t0 + Duration::from_millis(220)).is_none());
        assert_eq!(deb.poll(t0 + Duration::from_millis(260)), Some((30, 100)));
    }

    /// AC5-EDGE corollary: when ownership forbids a resize, the
    /// compositor renders the agent at its current winsize tail-clipped
    /// into the tile rect (bottom H rows, top-left W cols).
    #[test]
    fn tail_clip_when_agent_larger_than_tile() {
        // Agent screen is 30x100, tile is only 10x40. Clip to bottom 10
        // rows × top-left 40 cols so the prompt / cursor stays visible.
        let clip = tail_clip(30, 100, 10, 40);
        assert_eq!(clip.row_start, 20); // 30 - 10
        assert_eq!(clip.col_start, 0);
        assert_eq!(clip.rows, 10);
        assert_eq!(clip.cols, 40);
    }

    /// Tile larger than agent screen → show the entire agent screen.
    #[test]
    fn tail_clip_when_tile_larger_than_agent() {
        let clip = tail_clip(10, 40, 30, 100);
        assert_eq!(clip.row_start, 0);
        assert_eq!(clip.col_start, 0);
        assert_eq!(clip.rows, 10);
        assert_eq!(clip.cols, 40);
    }

    /// Equal dimensions: full passthrough.
    #[test]
    fn tail_clip_equal_dimensions() {
        let clip = tail_clip(24, 80, 24, 80);
        assert_eq!(clip.row_start, 0);
        assert_eq!(clip.col_start, 0);
        assert_eq!(clip.rows, 24);
        assert_eq!(clip.cols, 80);
    }

    /// Pane records its prior winsize on entry so the compositor can
    /// best-effort restore it on grid exit (Invariants).
    #[test]
    fn pane_prior_winsize_round_trips() {
        let mut p = Pane::new(24, 80);
        assert_eq!(p.prior_winsize(), None);
        p.record_prior_winsize(40, 120);
        assert_eq!(p.prior_winsize(), Some((40, 120)));
        // Repeated record DOES NOT overwrite - only the first observed
        // winsize is preserved; subsequent state changes are layout
        // adjustments, not the operator's pre-grid baseline.
        p.record_prior_winsize(10, 30);
        assert_eq!(p.prior_winsize(), Some((40, 120)));
    }

    // ── Scrollback ───────────────────────────────────────────────────────

    /// Feed 4 lines into a 2-row pane: 2 stay visible, 2 land in history.
    /// `CC`/`DD` are the live tail; `AA`/`BB` scrolled off.
    fn pane_with_history() -> Pane {
        let mut p = Pane::new(2, 4);
        p.feed(b"AA\r\nBB\r\nCC\r\nDD");
        p
    }

    /// AC1-HP: scrolling up renders older (scrolled-off) lines; the snapshot
    /// honors the display offset (the snapshot offset fix).
    #[test]
    fn scroll_up_renders_older_lines() {
        let mut p = pane_with_history();
        assert!(p.history_size() >= 2, "two lines scrolled into history");
        assert_eq!(p.scroll_offset(), 0, "starts at the live tail");
        // Live tail: top visible row is the third line `CC`.
        assert_eq!(p.snapshot().cell(0, 0).unwrap().text, "C");

        p.apply_scroll(ScrollCmd::LineUp);
        assert_eq!(p.scroll_offset(), 1);
        // Scrolled up one line: top visible row is now `BB`.
        assert_eq!(
            p.snapshot().cell(0, 0).unwrap().text,
            "B",
            "snapshot renders the scrolled region, not the live screen"
        );
    }

    /// AC3-FR: new output while scrolled does NOT yank the viewport - alacritty
    /// bumps the display offset so the same content stays visible.
    #[test]
    fn feed_while_scrolled_preserves_the_view() {
        let mut p = pane_with_history();
        p.apply_scroll(ScrollCmd::LineUp); // offset 1, showing `BB` at top
        assert_eq!(p.scroll_offset(), 1);
        assert_eq!(p.snapshot().cell(0, 0).unwrap().text, "B");

        // A new line pushes the live region up by one. The frozen view must
        // not jump to the tail: the offset bumps to 2 and `BB` stays on top.
        p.feed(b"\r\n");
        assert_eq!(p.scroll_offset(), 2, "offset bumps to preserve the view");
        assert_eq!(
            p.snapshot().cell(0, 0).unwrap().text,
            "B",
            "the operator's content is undisturbed by new output"
        );
    }

    /// AC1-EDGE: scrolling clamps at both boundaries (no underflow / overshoot).
    #[test]
    fn scroll_clamps_at_history_boundaries() {
        let mut p = pane_with_history();
        let hist = p.history_size();
        p.apply_scroll(ScrollCmd::Top);
        assert_eq!(p.scroll_offset(), hist, "Top clamps to the oldest line");
        p.apply_scroll(ScrollCmd::LineUp); // past the top
        assert_eq!(
            p.scroll_offset(),
            hist,
            "scrolling up past the top is a no-op"
        );

        p.apply_scroll(ScrollCmd::Bottom);
        assert_eq!(p.scroll_offset(), 0, "Bottom snaps to the live tail");
        p.apply_scroll(ScrollCmd::LineDown); // past the bottom
        assert_eq!(
            p.scroll_offset(),
            0,
            "scrolling down past the tail is a no-op"
        );
    }

    /// AC2-EDGE: a pane that has not overflowed its screen has no history, so
    /// the run loop's entry gate keeps the operator in WATCH.
    #[test]
    fn no_history_when_output_fits_on_screen() {
        let mut p = Pane::new(4, 8);
        p.feed(b"hi");
        assert_eq!(p.history_size(), 0, "one short line never scrolls off");
    }
}
