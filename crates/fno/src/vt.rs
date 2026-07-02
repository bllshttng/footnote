//! Server-side VT emulation: PTY bytes -> a styled cell grid -> `proto::Frame`.
//!
//! Seeded from `crates/fno-agents/src/screen.rs` (headless
//! `Term<VoidListener>` + `vte::ansi::Processor`) and its grid compositor's
//! cell mapping, adapted for the mux: alt-screen is exercised (vim under
//! detach/reattach, AC3-EDGE), scrollback is bounded (10k lines - a cap, not
//! an upfront allocation, so server memory per pane is bounded), and the
//! snapshot is a full styled [`Frame`], not trimmed text. The server grid is
//! the single source of truth; the client never emulates VT itself.

use std::collections::VecDeque;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VtColor, NamedColor, Processor, Rgb};

use crate::proto::{cell_flags, BlockMeta, BlockSel, Cell, Color, Frame};

/// Default grid until the first client reports its real size. 24x80 is the
/// historical terminal default (matches the fno-agents drive fallback).
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Default scrollback cap per pane. `Term::new` treats `scrolling_history` as a
/// bound the history grows toward, not an allocation, so a pane that emits 500
/// lines costs 500 lines even at this ceiling. 100k is alacritty's documented
/// max; a memory-constrained deployment lowers it via `config.mux.scrollback_lines`
/// (wired in Task 1.5). Governs how far `pane read --lines` and a still-streaming
/// block can reach into history (4b).
const SCROLLBACK_LINES: usize = 100_000;

/// Resolve the per-pane scrollback cap. `config.mux.scrollback_lines` is
/// exported by the launcher as `FNO_MUX_SCROLLBACK_LINES` (the crate reads env,
/// not settings.yaml - same pattern as `FNO_SESSION`/`FNO_MUX_DIR`); a
/// memory-constrained deployment lowers it. Falls back to the 100k default.
fn scrollback_lines() -> usize {
    std::env::var("FNO_MUX_SCROLLBACK_LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(SCROLLBACK_LINES)
}

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
    /// OSC 133 shell-integration scanner: splits PTY output at FinalTerm markers
    /// BEFORE they reach the Term (which drops unknown OSC anyway) so 4b can turn
    /// a pane's scroll into typed command blocks. Stateful across feeds.
    scanner: Osc133Scanner,
    /// Completed command blocks (oldest first), capped at `MAX_BLOCKS` and
    /// front-dropped; capture-at-completion stores each block's text so a
    /// finished block is width- and scroll-independent.
    blocks: VecDeque<Block>,
    /// The block whose `C` (output start) was seen but not yet its `D`. Read live
    /// from the grid until finalized.
    open: Option<OpenBlock>,
    /// Monotonic per-pane block sequence; never reuses.
    next_seq: u64,
    /// Whether any OSC 133 marker was ever seen. A pane that never emitted markers
    /// reads as ONE implicit block (whole output); one that did but has none
    /// retained reads `BLOCK_UNAVAILABLE`.
    saw_marker: bool,
    /// The resolved scrollback cap (from [`scrollback_lines`]). A block whose
    /// text is captured while history is at this cap may have lost its top to
    /// eviction, so it is flagged `truncated` (honest, conservative).
    scrollback: usize,
}

impl Pane {
    pub fn new(rows: u16, cols: u16) -> Self {
        Pane::with_scrollback(rows, cols, scrollback_lines())
    }

    /// Construct with an explicit scrollback cap (the [`Pane::new`] path resolves
    /// it from config). Lets tests exercise the at-cap `truncated` flag without
    /// touching a process-global env var.
    fn with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let scrollback = scrollback.max(1);
        let config = Config {
            scrolling_history: scrollback,
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
            scanner: Osc133Scanner::default(),
            blocks: VecDeque::new(),
            open: None,
            next_seq: 0,
            saw_marker: false,
            scrollback,
        }
    }

    /// Feed raw PTY output. The OSC 133 scanner splits the stream: passthrough
    /// spans advance the Term (which buffers partial escape sequences across
    /// calls); recognized FinalTerm markers are stripped and queued for the block
    /// store. Partial markers split across reads carry over in the scanner state.
    pub fn feed(&mut self, bytes: &[u8]) {
        for seg in self.scanner.scan(bytes) {
            match seg {
                // Advance passthrough FIRST so a marker records the Term's row
                // AFTER its preceding output landed.
                Seg::Pass(b) => self.processor.advance(&mut self.term, &b),
                Seg::Marker(m) => self.on_marker(m),
            }
        }
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

    // -- OSC 133 command blocks (Task 1.2) -------------------------------------

    /// Drive the block store from a recognized marker at the Term's current row.
    /// `C` opens a block; `D` finalizes it (captures its text). `A`/`B` are
    /// prompt/command boundaries the output-span model needs no transition for.
    fn on_marker(&mut self, m: Osc133) {
        self.saw_marker = true;
        match m {
            Osc133::OutputStart => {
                // A prior still-open block (C with no D - e.g. a reprinted prompt)
                // finalizes with unknown exit before the new one opens.
                if self.open.is_some() {
                    self.finalize_open(None);
                }
                let seq = self.next_seq;
                self.next_seq += 1;
                self.open = Some(OpenBlock {
                    seq,
                    anchor: self.abs_row(),
                });
            }
            Osc133::CmdDone { exit } => self.finalize_open(exit),
            Osc133::PromptStart | Osc133::CmdStart => {}
        }
    }

    /// The cursor's absolute row: lines scrolled into history + current grid line.
    /// Exact below the scrollback cap (history grows monotonically as lines scroll
    /// off); see `extract_from` for the past-cap honesty.
    fn abs_row(&self) -> u64 {
        let grid = self.term.grid();
        grid.history_size() as u64 + grid.cursor.point.line.0.max(0) as u64
    }

    /// Close the open block: capture its output text now and retain it.
    fn finalize_open(&mut self, exit: Option<i32>) {
        let Some(open) = self.open.take() else { return };
        let (text, truncated) = self.extract_from(open.anchor);
        if self.blocks.len() >= MAX_BLOCKS {
            self.blocks.pop_front();
        }
        self.blocks.push_back(Block {
            seq: open.seq,
            exit,
            truncated,
            text,
        });
    }

    /// Text from absolute row `anchor` down to the current output bottom (cursor
    /// line), plus whether the span is possibly `truncated` at its top.
    ///
    // ponytail: `history_size()` saturates at the scrollback cap and alacritty
    // exposes no post-cap scroll counter (VoidListener), so once the pane fills
    // its scrollback we cannot prove a block's top survived - `truncated` is
    // then conservatively true (older rows of a large block MAY have evicted).
    // Below the cap it is exact-false: nothing has scrolled off, so the whole
    // span is present. Upgrade path for a precise flag: a self-maintained
    // scrolled-off counter fed from a real EventListener.
    fn extract_from(&self, anchor: u64) -> (String, bool) {
        let grid = self.term.grid();
        let hist = grid.history_size();
        let top = grid.topmost_line().0 as i64; // == -(hist as i64)
        let start = (anchor as i64 - hist as i64).max(top);
        let truncated = hist >= self.scrollback;
        let end = grid.cursor.point.line.0 as i64;
        (self.rows_text(start, end), truncated)
    }

    /// Render grid rows `[start ..= end]` (Line coords; negatives reach history)
    /// as trimmed text, trailing blank rows dropped. Clamps to the live window.
    fn rows_text(&self, start: i64, end: i64) -> String {
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let top = grid.topmost_line().0 as i64;
        let bot = grid.bottommost_line().0 as i64;
        let start = start.max(top);
        let end = end.min(bot);
        let mut rows: Vec<String> = Vec::new();
        let mut ln = start;
        while ln <= end {
            let row = &grid[Line(ln as i32)];
            let mut s = String::with_capacity(cols);
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                s.push(if cell.flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                });
            }
            while s.ends_with(' ') {
                s.pop();
            }
            rows.push(s);
            ln += 1;
        }
        while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// The last `lines` logical rows, reaching into history (US5). `lines` at or
    /// below the viewport height reproduces the visible grid (AC5-UI); above it,
    /// history is included up to the scrollback window.
    pub fn read_tail(&self, lines: u16) -> String {
        let grid = self.term.grid();
        let bot = grid.bottommost_line().0 as i64;
        let start = bot - (lines.max(1) as i64) + 1;
        self.rows_text(start, bot)
    }

    /// Read a command block. `Err(())` is `BLOCK_UNAVAILABLE`: an evicted or
    /// nonexistent block, or a specific `seq` on a markerless pane. A markerless
    /// pane's `Last` degrades to ONE implicit block (whole output), flagged.
    pub fn read_block(&self, sel: BlockSel) -> Result<BlockRead, ()> {
        match sel {
            BlockSel::Last => {
                if let Some(open) = &self.open {
                    return Ok(self.read_open(open));
                }
                if let Some(b) = self.blocks.back() {
                    return Ok(BlockRead::complete(b));
                }
                if !self.saw_marker {
                    return Ok(self.implicit_block());
                }
                Err(())
            }
            BlockSel::Seq(n) => {
                if let Some(open) = &self.open {
                    if open.seq == n {
                        return Ok(self.read_open(open));
                    }
                }
                self.blocks
                    .iter()
                    .find(|b| b.seq == n)
                    .map(BlockRead::complete)
                    .ok_or(())
            }
        }
    }

    /// Live snapshot of a still-streaming block (no `D` yet): text so far,
    /// `complete=false`. Never hangs waiting for the terminator (AC2-FR).
    fn read_open(&self, open: &OpenBlock) -> BlockRead {
        let (text, truncated) = self.extract_from(open.anchor);
        BlockRead {
            seq: Some(open.seq),
            exit: None,
            complete: false,
            truncated,
            implicit: false,
            text,
        }
    }

    /// The most recently completed block's `(seq, exit)`, or `None`. Feeds the
    /// `command_done` wait signal.
    pub fn last_done(&self) -> Option<(u64, Option<i32>)> {
        self.blocks.back().map(|b| (b.seq, b.exit))
    }

    /// The whole output (history + grid) as one implicit block for a pane that
    /// never emitted markers.
    fn implicit_block(&self) -> BlockRead {
        let grid = self.term.grid();
        let top = grid.topmost_line().0 as i64;
        let bot = grid.cursor.point.line.0 as i64;
        BlockRead {
            seq: None,
            exit: None,
            complete: true,
            truncated: false,
            implicit: true,
            text: self.rows_text(top, bot),
        }
    }

    /// The pane's negotiated terminal modes - the per-pane state whose
    /// EFFECT lives on the client's real terminal. Only the focused pane's
    /// modes may own the client TTY at any moment (replaying an unfocused
    /// vim's mouse reporting would break the focused shell), so the server
    /// syncs these on focus change and attach via [`mode_diff`].
    pub fn modes(&self) -> Modes {
        let m = self.term.mode();
        Modes {
            app_cursor: m.contains(TermMode::APP_CURSOR),
            app_keypad: m.contains(TermMode::APP_KEYPAD),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            focus_in_out: m.contains(TermMode::FOCUS_IN_OUT),
            mouse_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: m.contains(TermMode::MOUSE_DRAG),
            mouse_motion: m.contains(TermMode::MOUSE_MOTION),
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            utf8_mouse: m.contains(TermMode::UTF8_MOUSE),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            kitty_flags: kitty_bits(m),
        }
    }
}

/// The negotiated-mode subset that crosses the wire in `ModeSync`. Alt screen
/// is deliberately ABSENT: panes composite into the client's own alt screen,
/// so a pane's alt-screen state must never leak to the client TTY. Cursor
/// visibility rides inside every `Frame` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_in_out: bool,
    pub mouse_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub alternate_scroll: bool,
    /// The kitty keyboard-protocol flag bits (progressive enhancement), as
    /// alacritty_terminal 0.26 models them: 1 disambiguate, 2 event types,
    /// 4 alternate keys, 8 all-keys-as-esc, 16 associated text.
    pub kitty_flags: u8,
}

/// The kitty progressive-enhancement bits from a live mode set, in the wire
/// order the kitty protocol defines.
fn kitty_bits(m: &TermMode) -> u8 {
    let mut bits = 0u8;
    if m.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        bits |= 1;
    }
    if m.contains(TermMode::REPORT_EVENT_TYPES) {
        bits |= 2;
    }
    if m.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        bits |= 4;
    }
    if m.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        bits |= 8;
    }
    if m.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
        bits |= 16;
    }
    bits
}

/// The escape bytes that move a terminal from `old` to `new` mode state.
/// Pure and minimal: only CHANGED modes emit a sequence, so syncing a pane
/// against itself is zero bytes (the server skips empty syncs entirely).
pub fn mode_diff(old: Modes, new: Modes) -> Vec<u8> {
    let mut out = Vec::new();
    let mut dec = |on: bool, was: bool, code: &str| {
        if on != was {
            out.extend_from_slice(format!("\x1b[?{code}{}", if on { 'h' } else { 'l' }).as_bytes());
        }
    };
    dec(new.app_cursor, old.app_cursor, "1");
    dec(new.mouse_click, old.mouse_click, "1000");
    dec(new.mouse_drag, old.mouse_drag, "1002");
    dec(new.mouse_motion, old.mouse_motion, "1003");
    dec(new.focus_in_out, old.focus_in_out, "1004");
    dec(new.utf8_mouse, old.utf8_mouse, "1005");
    dec(new.sgr_mouse, old.sgr_mouse, "1006");
    dec(new.alternate_scroll, old.alternate_scroll, "1007");
    dec(new.bracketed_paste, old.bracketed_paste, "2004");
    if new.app_keypad != old.app_keypad {
        out.extend_from_slice(if new.app_keypad { b"\x1b=" } else { b"\x1b>" });
    }
    if new.kitty_flags != old.kitty_flags {
        // "CSI = flags ; 1 u": set the given flags and unset the rest - the
        // stateless form, so no push/pop stack bookkeeping crosses the wire.
        out.extend_from_slice(format!("\x1b[={};1u", new.kitty_flags).as_bytes());
    }
    out
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

// -- OSC 133 command block store (Task 1.2) -------------------------------------
//
// Capture-at-completion (blueprint amendment, overrides the epic's SumTree +
// lazy-row-anchor design): a block opens on the `C` marker with a below-cap-exact
// anchor and is read LIVE from the grid while streaming; on `D` its text is
// extracted and STORED, so a completed block owns immutable bytes - no anchor, no
// scroll counter, no reflow dependence. Eviction is a bounded retained-block
// budget (front-drop), not a row-scroll computation.

/// Max retained completed blocks per pane. Bounds retained block memory
/// independent of scrollback; oldest front-dropped, and an evicted block reads
/// `BLOCK_UNAVAILABLE` (AC1-FR).
const MAX_BLOCKS: usize = 512;

/// A completed command block: the output span between an OSC 133 `C` (output
/// start) and `D` (command done), with its text captured at `D`.
#[derive(Debug, Clone)]
struct Block {
    seq: u64,
    exit: Option<i32>,
    truncated: bool,
    text: String,
}

/// A block whose `C` was seen but not yet its `D`: read live from the grid.
#[derive(Debug, Clone, Copy)]
struct OpenBlock {
    seq: u64,
    /// Absolute output-start row (`history_size + line` at `C`).
    anchor: u64,
}

/// A block read result: text plus the metadata `--json` surfaces. Degradations
/// (still streaming, markerless-implicit, truncated) are VISIBLE flags, never
/// silent (silent-failure hunter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRead {
    /// `None` for the implicit whole-output block of a markerless pane.
    pub seq: Option<u64>,
    pub exit: Option<i32>,
    pub complete: bool,
    pub truncated: bool,
    pub implicit: bool,
    pub text: String,
}

impl BlockRead {
    fn complete(b: &Block) -> Self {
        BlockRead {
            seq: Some(b.seq),
            exit: b.exit,
            complete: true,
            truncated: b.truncated,
            implicit: false,
            text: b.text.clone(),
        }
    }

    /// The wire metadata for this read (the `text` rides `PaneText` alongside).
    pub fn meta(&self) -> BlockMeta {
        BlockMeta {
            seq: self.seq,
            exit: self.exit,
            complete: self.complete,
            truncated: self.truncated,
            implicit: self.implicit,
        }
    }
}

// -- OSC 133 (FinalTerm) shell-integration scanner ------------------------------
//
// alacritty's ANSI processor silently drops OSC sequences it does not model, so
// OSC 133 never reaches a handler we control (verified against the landed code).
// Capture is therefore a pre-`advance` byte scanner: it walks the PTY stream, and
// at each `ESC ] 133 ; <arg> (BEL | ST)` it emits a typed marker and STRIPS the
// bytes (the Term would ignore them anyway; stripping keeps the grid canonical).
// Everything else flows to the Term verbatim - nothing is ever swallowed. The
// discipline mirrors `keys.rs`: a tiny state machine with a bounded accumulator,
// safe with a marker split one byte per read.

/// A recognized OSC 133 marker. FinalTerm's letters: `A` prompt start, `B`
/// command start, `C` command output start, `D[;exit]` command finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc133 {
    PromptStart,
    CmdStart,
    OutputStart,
    CmdDone { exit: Option<i32> },
}

/// One unit of scanner output, in stream order: bytes bound for the Term, or a
/// marker to record at the Term's position after the preceding bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Pass(Vec<u8>),
    Marker(Osc133),
}

/// The FinalTerm prefix after `ESC ]`: the literal `133;`.
const OSC133_PREFIX: &[u8] = b"133;";
/// Payload cap: a real FinalTerm arg is a few bytes (`D;137`, `A;cl=m`). Anything
/// longer is hostile/garbage - flush what we held and bail (bounded, no panic).
const MAX_PAYLOAD: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScanState {
    /// Bytes flow to the Term.
    Ground,
    /// Saw `ESC`; awaiting `]` (or a divergence back to Ground).
    Esc,
    /// Saw `ESC ]`; matched `n` bytes of `133;`.
    Prefix(usize),
    /// Matched `ESC ] 133 ;`; accumulating the arg until a terminator.
    Payload(Vec<u8>),
    /// In the payload, saw `ESC`; a following `\` closes the marker (ST).
    PayloadEsc(Vec<u8>),
}

/// Per-pane OSC 133 scanner. State persists across `scan` calls so a marker split
/// across PTY reads still parses (`keys.rs` accumulation discipline, output-side).
pub struct Osc133Scanner {
    state: ScanState,
}

impl Default for Osc133Scanner {
    fn default() -> Self {
        Osc133Scanner {
            state: ScanState::Ground,
        }
    }
}

impl Osc133Scanner {
    /// Split `bytes` into passthrough spans and stripped markers, in order.
    /// Common case (no `ESC`): one `Pass` covering the whole read.
    // ponytail: one to_vec per read of terminal output; alacritty's advance
    // copies into the grid anyway, so this is not the hot allocation. Zero-copy
    // spans would need lifetimes across the carried-over state - not worth it.
    fn scan(&mut self, bytes: &[u8]) -> Vec<Seg> {
        let mut out: Vec<Seg> = Vec::new();
        let mut plain: Vec<u8> = Vec::new();

        // Flush any pending passthrough bytes as one segment before a marker.
        macro_rules! flush {
            () => {
                if !plain.is_empty() {
                    out.push(Seg::Pass(std::mem::take(&mut plain)));
                }
            };
        }

        for &b in bytes {
            match std::mem::replace(&mut self.state, ScanState::Ground) {
                ScanState::Ground => {
                    if b == 0x1b {
                        self.state = ScanState::Esc;
                    } else {
                        plain.push(b);
                    }
                }
                ScanState::Esc => {
                    if b == 0x5d {
                        // `ESC ]` - an OSC; start matching the 133 prefix.
                        self.state = ScanState::Prefix(0);
                    } else if b == 0x1b {
                        // `ESC ESC` - the first ESC is not ours; hand it back and
                        // stay in Esc for the new one.
                        plain.push(0x1b);
                        self.state = ScanState::Esc;
                    } else {
                        // `ESC <other>` - some other escape; hand both to the Term.
                        plain.push(0x1b);
                        plain.push(b);
                    }
                }
                ScanState::Prefix(n) => {
                    if b == OSC133_PREFIX[n] {
                        if n + 1 == OSC133_PREFIX.len() {
                            self.state = ScanState::Payload(Vec::new());
                        } else {
                            self.state = ScanState::Prefix(n + 1);
                        }
                    } else {
                        // Not a 133 OSC (e.g. a title `ESC ] 0 ;`). Hand back what
                        // we held and let the Term parse the rest of this OSC.
                        plain.push(0x1b);
                        plain.push(0x5d);
                        plain.extend_from_slice(&OSC133_PREFIX[..n]);
                        plain.push(b);
                    }
                }
                ScanState::Payload(mut buf) => {
                    if b == 0x07 {
                        // BEL terminator: emit + strip.
                        flush!();
                        if let Some(m) = parse_osc133(&buf) {
                            out.push(Seg::Marker(m));
                        }
                    } else if b == 0x1b {
                        self.state = ScanState::PayloadEsc(buf);
                    } else if buf.len() >= MAX_PAYLOAD {
                        // Hostile length: hand everything back, nothing swallowed.
                        plain.push(0x1b);
                        plain.push(0x5d);
                        plain.extend_from_slice(OSC133_PREFIX);
                        plain.append(&mut buf);
                        plain.push(b);
                    } else {
                        buf.push(b);
                        self.state = ScanState::Payload(buf);
                    }
                }
                ScanState::PayloadEsc(mut buf) => {
                    if b == 0x5c {
                        // `ESC \` (ST) terminator: emit + strip.
                        flush!();
                        if let Some(m) = parse_osc133(&buf) {
                            out.push(Seg::Marker(m));
                        }
                    } else {
                        // ESC not closing the marker: garbage. Hand it all back.
                        plain.push(0x1b);
                        plain.push(0x5d);
                        plain.extend_from_slice(OSC133_PREFIX);
                        plain.append(&mut buf);
                        plain.push(0x1b);
                        plain.push(b);
                    }
                }
            }
        }
        flush!();
        out
    }
}

/// Parse the payload after `133;` into a marker. Unknown subtypes yield `None`
/// (stripped, no event) - they are semantic markers the Term ignores anyway.
fn parse_osc133(buf: &[u8]) -> Option<Osc133> {
    let s = std::str::from_utf8(buf).ok()?;
    let mut fields = s.split(';');
    match fields.next()? {
        "A" => Some(Osc133::PromptStart),
        "B" => Some(Osc133::CmdStart),
        "C" => Some(Osc133::OutputStart),
        "D" => {
            // `D` or `D;<exit>[;...]`; the exit is the first field, if numeric.
            let exit = fields.next().and_then(|f| f.parse::<i32>().ok());
            Some(Osc133::CmdDone { exit })
        }
        _ => None,
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
    fn server_spine_vt_modes_track_dec_private_sets() {
        let mut pane = Pane::new(4, 20);
        // alacritty's TermMode::default() ships ALTERNATE_SCROLL on, so a
        // fresh pane is NOT Modes::default() (= a raw client terminal). The
        // attach-time sync therefore emits one ?1007h - deliberate: the
        // client terminal should mirror the pane's real state, not our guess.
        assert_eq!(
            pane.modes(),
            Modes {
                alternate_scroll: true,
                ..Modes::default()
            }
        );
        // vim-with-mouse territory: app cursor, SGR mouse w/ motion,
        // bracketed paste.
        pane.feed(b"\x1b[?1h\x1b[?1003h\x1b[?1006h\x1b[?2004h");
        let m = pane.modes();
        assert!(m.app_cursor && m.mouse_motion && m.sgr_mouse && m.bracketed_paste);
        assert!(!m.mouse_click && !m.utf8_mouse, "unset modes stay unset");
        pane.feed(b"\x1b[?1003l\x1b[?2004l");
        let m = pane.modes();
        assert!(!m.mouse_motion && !m.bracketed_paste);
        assert!(m.sgr_mouse, "unrelated modes survive");
    }

    #[test]
    fn server_spine_vt_mode_diff_emits_only_changes() {
        let plain = Modes::default();
        let vim = Modes {
            app_cursor: true,
            mouse_motion: true,
            sgr_mouse: true,
            bracketed_paste: true,
            ..Modes::default()
        };
        assert!(
            mode_diff(plain, plain).is_empty(),
            "identical modes must sync zero bytes"
        );
        let to_vim = String::from_utf8(mode_diff(plain, vim)).unwrap();
        assert!(to_vim.contains("\x1b[?1h"), "{to_vim:?}");
        assert!(to_vim.contains("\x1b[?1003h"), "{to_vim:?}");
        assert!(to_vim.contains("\x1b[?1006h"), "{to_vim:?}");
        assert!(to_vim.contains("\x1b[?2004h"), "{to_vim:?}");
        // The way back RESETS exactly what was set.
        let to_plain = String::from_utf8(mode_diff(vim, plain)).unwrap();
        assert!(to_plain.contains("\x1b[?1l"), "{to_plain:?}");
        assert!(to_plain.contains("\x1b[?1003l"), "{to_plain:?}");
        assert!(to_plain.contains("\x1b[?2004l"), "{to_plain:?}");
        // Kitty flags use the stateless absolute-set form.
        let kitty = Modes {
            kitty_flags: 0b1011,
            ..Modes::default()
        };
        let set = String::from_utf8(mode_diff(plain, kitty)).unwrap();
        assert!(set.contains("\x1b[=11;1u"), "{set:?}");
        let clear = String::from_utf8(mode_diff(kitty, plain)).unwrap();
        assert!(clear.contains("\x1b[=0;1u"), "{clear:?}");
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

    // -- OSC 133 scanner (Task 1.1) --------------------------------------------

    /// Run byte chunks through a fresh scanner; return (markers, passthrough).
    fn scan_chunks(chunks: &[&[u8]]) -> (Vec<Osc133>, Vec<u8>) {
        let mut sc = Osc133Scanner::default();
        let (mut markers, mut pass) = (Vec::new(), Vec::new());
        for ch in chunks {
            for seg in sc.scan(ch) {
                match seg {
                    Seg::Pass(b) => pass.extend_from_slice(&b),
                    Seg::Marker(m) => markers.push(m),
                }
            }
        }
        (markers, pass)
    }

    fn scan(bytes: &[u8]) -> (Vec<Osc133>, Vec<u8>) {
        scan_chunks(&[bytes])
    }

    #[test]
    fn osc133_recognizes_finalterm_letters_and_exit() {
        // A prompt, B command, C output-start, D;1 done-with-exit-1 (BEL-terminated).
        let (markers, pass) =
            scan(b"\x1b]133;A\x07$ \x1b]133;B\x07false\x1b]133;C\x07\x1b]133;D;1\x07");
        assert_eq!(
            markers,
            vec![
                Osc133::PromptStart,
                Osc133::CmdStart,
                Osc133::OutputStart,
                Osc133::CmdDone { exit: Some(1) },
            ]
        );
        // AC1-UI: no marker bytes survive in the passthrough - only real text.
        assert_eq!(pass, b"$ false");
    }

    #[test]
    fn osc133_st_terminator_and_bare_d() {
        // ST (ESC \) terminator, and a bare `D` with no exit field.
        let (markers, pass) = scan(b"\x1b]133;C\x1b\\ok\x1b]133;D\x1b\\");
        assert_eq!(
            markers,
            vec![Osc133::OutputStart, Osc133::CmdDone { exit: None }]
        );
        assert_eq!(pass, b"ok");
    }

    #[test]
    fn osc133_splits_one_byte_per_feed_identically() {
        // AC1-EDGE: a marker split one byte per feed parses identically.
        let stream = b"a\x1b]133;C\x07b\x1b]133;D;0\x07c";
        let chunks: Vec<&[u8]> = stream.iter().map(std::slice::from_ref).collect();
        let (markers, pass) = scan_chunks(&chunks);
        assert_eq!(
            markers,
            vec![Osc133::OutputStart, Osc133::CmdDone { exit: Some(0) }]
        );
        assert_eq!(pass, b"abc");
    }

    #[test]
    fn osc133_non_133_osc_passes_through() {
        // A window-title OSC (`ESC ] 0 ; ...`) is not ours: passed through whole,
        // no marker (the Term consumes it as a title).
        let (markers, pass) = scan(b"\x1b]0;my title\x07hello");
        assert!(markers.is_empty());
        assert_eq!(pass, b"\x1b]0;my title\x07hello");
    }

    #[test]
    fn osc133_hostile_payload_is_bounded_no_marker() {
        // AC1-ERR: a garbage payload past the length cap flushes as inert bytes,
        // records no phantom marker, does not panic or buffer unbounded.
        let mut hostile = b"\x1b]133;".to_vec();
        hostile.extend(std::iter::repeat(b'X').take(500));
        hostile.push(0x07);
        hostile.extend_from_slice(b"tail");
        let (markers, pass) = scan(&hostile);
        assert!(markers.is_empty());
        assert!(pass.ends_with(b"tail"), "nothing swallowed after flush");
    }

    #[test]
    fn osc133_unknown_subtype_is_stripped_without_marker() {
        // FinalTerm defines A/B/C/D; an unknown letter is stripped, no marker.
        let (markers, pass) = scan(b"x\x1b]133;Z;foo\x07y");
        assert!(markers.is_empty());
        assert_eq!(pass, b"xy");
    }

    #[test]
    fn osc133_markers_never_reach_the_grid() {
        // Integration: feed markers through a Pane; the grid holds only real text.
        let mut pane = Pane::new(24, 80);
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;C\x07out\x1b]133;D;0\x07");
        assert_eq!(pane.text(), "$ out");
    }

    // -- OSC 133 block store (Task 1.2) ----------------------------------------

    /// Feed one full command's markers + output: prompt, output-start, text,
    /// done-with-exit. `\r\n` moves to a fresh row so blocks don't overwrite.
    fn run_command(pane: &mut Pane, output: &str, exit: i32) {
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;C\x07");
        pane.feed(output.as_bytes());
        pane.feed(format!("\x1b]133;D;{exit}\x07\r\n").as_bytes());
    }

    #[test]
    fn blocks_capture_seq_and_exit_in_order() {
        // AC1-HP: two commands -> two blocks, correct seq order and exit codes.
        let mut pane = Pane::new(24, 80);
        run_command(&mut pane, "hello", 0);
        run_command(&mut pane, "boom", 1);

        let b0 = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert_eq!((b0.seq, b0.exit, b0.complete), (Some(0), Some(0), true));
        assert_eq!(b0.text, "$ hello");

        let b1 = pane.read_block(BlockSel::Seq(1)).unwrap();
        assert_eq!((b1.seq, b1.exit), (Some(1), Some(1)));
        assert_eq!(b1.text, "$ boom");

        // AC2-HP: `Last` is the most recent completed block.
        assert_eq!(pane.read_block(BlockSel::Last).unwrap().seq, Some(1));
    }

    #[test]
    fn markerless_pane_reads_one_implicit_block() {
        // AC2-ERR: no markers -> `Last` returns the whole output, flagged
        // implicit; a specific seq is BLOCK_UNAVAILABLE.
        let mut pane = Pane::new(24, 80);
        pane.feed(b"just output\r\nline two");
        let read = pane.read_block(BlockSel::Last).unwrap();
        assert!(read.implicit && read.seq.is_none() && read.complete);
        assert_eq!(read.text, "just output\nline two");
        assert!(pane.read_block(BlockSel::Seq(5)).is_err());
    }

    #[test]
    fn open_block_reads_live_and_incomplete() {
        // AC2-FR: a block mid-output (no D) reads the span so far, incomplete.
        let mut pane = Pane::new(24, 80);
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;C\x07partial");
        let read = pane.read_block(BlockSel::Last).unwrap();
        assert_eq!((read.seq, read.complete), (Some(0), false));
        assert_eq!(read.text, "$ partial");
    }

    #[test]
    fn completed_block_survives_width_resize() {
        // AC2-EDGE flip (amendment): a completed block returns its CAPTURED text
        // after a width-changing resize, never BLOCK_UNAVAILABLE.
        let mut pane = Pane::new(24, 80);
        run_command(&mut pane, "keepme", 0);
        pane.resize(24, 40); // width change reflows the grid
        let read = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert_eq!(read.text, "$ keepme");
    }

    #[test]
    fn evicted_block_reads_unavailable() {
        // AC1-FR: past the retained-block budget, the oldest evicts and reads
        // BLOCK_UNAVAILABLE; the newest still reads.
        let mut pane = Pane::new(24, 80);
        for i in 0..(MAX_BLOCKS + 3) {
            run_command(&mut pane, &format!("c{i}"), 0);
        }
        assert!(pane.read_block(BlockSel::Seq(0)).is_err(), "oldest evicted");
        let last = (MAX_BLOCKS + 3 - 1) as u64;
        assert!(pane.read_block(BlockSel::Seq(last)).is_ok(), "newest kept");
    }

    #[test]
    fn block_truncated_flag_fires_only_at_scrollback_cap() {
        // Below the cap a completed block is exact (nothing evicted) - not
        // truncated. Once the pane fills its (tiny) scrollback, a block's top
        // may have scrolled off, so the flag conservatively fires.
        let mut pane = Pane::with_scrollback(4, 20, 8);
        run_command(&mut pane, "small", 0);
        assert!(
            !pane.read_block(BlockSel::Seq(0)).unwrap().truncated,
            "below-cap block is exact"
        );
        // Push enough rows to fill the 8-line scrollback, then capture a block.
        for i in 0..40 {
            pane.feed(format!("filler{i}\r\n").as_bytes());
        }
        run_command(&mut pane, "big", 0);
        assert!(
            pane.read_block(BlockSel::Last).unwrap().truncated,
            "at-cap block is flagged possibly-truncated"
        );
    }

    #[test]
    fn read_tail_reaches_into_history() {
        // AC5-HP: output taller than the viewport; a large --lines reaches history.
        let mut pane = Pane::new(4, 20);
        for i in 0..20 {
            pane.feed(format!("row{i}\r\n").as_bytes());
        }
        // Viewport is 4 rows; a 10-line tail pulls scrolled-off rows back.
        let tail = pane.read_tail(10);
        assert!(tail.contains("row15") && tail.contains("row19"), "{tail:?}");
        assert!(tail.contains("row11"), "reached into history: {tail:?}");
        // AC5-UI: a viewport-sized read matches the visible grid.
        assert_eq!(pane.read_tail(4), pane.text());
    }
}
