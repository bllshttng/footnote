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
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{viewport_to_point, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VtColor, NamedColor, Processor, Rgb};

use crate::proto::{cell_flags, BlockDir, BlockMeta, BlockSel, Cell, Color, Frame};

/// Default grid until the first client reports its real size. 24x80 is the
/// historical terminal default (matches the fno-agents drive fallback).
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Default scrollback cap per pane. `Term::new` treats `scrolling_history` as a
/// bound the history grows toward, not an allocation, so a pane that emits 500
/// lines costs 500 lines even at this ceiling. 100k is alacritty's documented
/// max; a memory-constrained deployment lowers it via the
/// `FNO_MUX_SCROLLBACK_LINES` env var (see `scrollback_lines`). Governs how far
/// `pane read --lines` and a still-streaming block can reach into history (4b).
const SCROLLBACK_LINES: usize = 100_000;

/// Resolve the per-pane scrollback cap. Read directly from the
/// `FNO_MUX_SCROLLBACK_LINES` env var (set it yourself); it is NOT wired to
/// `config.mux` - the crate reads env only, same pattern as
/// `FNO_SESSION`/`FNO_MUX_DIR`. Falls back to the 100k default.
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
    /// from its accumulated output buffer until finalized.
    open: Option<OpenBlock>,
    /// Monotonic per-pane block sequence; never reuses.
    next_seq: u64,
    /// Whether any OSC 133 marker was ever seen. A pane that never emitted markers
    /// reads as ONE implicit block (whole output); one that did but has none
    /// retained reads `BLOCK_UNAVAILABLE`.
    saw_marker: bool,
    /// Running sum of `text.len()` across retained `blocks`, so eviction can
    /// bound retained block memory by BYTES (a high-output agent's blocks would
    /// otherwise pin gigabytes under a count-only cap - the whole point of
    /// capture-at-completion is that the text is stored).
    retained_bytes: usize,
    /// Per-block captured-text cap: a single command's stored span is trimmed to
    /// its most-recent tail past this (flagged `truncated`), so one huge command
    /// cannot pin memory that eviction can never reclaim (the last block stays).
    max_block_bytes: usize,
    /// Total retained-block-text budget across the pane; oldest blocks front-drop
    /// until under it.
    max_retained_bytes: usize,
    /// Between `B` (command start) and `C` (output start): the block's start
    /// anchor row (see [`Pane::anchor_row`]) plus the command echo bytes, so the
    /// command line is byte-captured (not grid-scraped, which x-122e retired) for
    /// rerun. `None` outside that window.
    pending_cmd: Option<PendingCmd>,
    /// The block seq the keyboard block-selection walk (prefix+v) rests on.
    /// Cleared whenever the selection itself clears.
    selected_block: Option<u64>,
    /// (v12, x-e780) The active in-scrollback search, or `None`. Each step
    /// re-scans against the live grid (x-1e67: a frozen snapshot's `abs_row`
    /// anchors drift once scrollback saturates, landing the highlight on the
    /// wrong row). Shared per-pane (Locked 2) - every co-viewer sees the jump +
    /// highlight; only the initiator gets the `[i/n]` counter.
    search: Option<SearchState>,
}

/// The B..C command-echo capture in flight (see [`Pane::pending_cmd`]).
#[derive(Debug, Clone)]
struct PendingCmd {
    /// Start anchor row of the block (the command line), taken at `B`.
    start_abs: i64,
    /// Command-echo bytes accumulating between `B` and `C`, tail-bounded.
    raw: Vec<u8>,
}

/// One free-text match, oldest -> newest (ascending `abs_row`). Columns are grid
/// `Column` indices on the single rendered row (Locked 6: no cross-soft-wrap
/// match), so the highlight span is width-correct for ASCII; the leading cell of
/// a wide char is used, which is exact for the common case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchMatch {
    /// [`Pane::anchor_row`]-scheme absolute row (`grid_line + history_size`).
    abs_row: i64,
    /// Grid column of the first matched cell.
    col_start: usize,
    /// Grid column of the last matched cell.
    col_end: usize,
}

/// The per-pane active search (see [`Pane::search`]).
#[derive(Debug, Clone)]
struct SearchState {
    /// The query, kept so each step re-scans (anchors drift once scrollback
    /// saturates, so a frozen snapshot highlights the wrong row - x-1e67).
    needle: Vec<char>,
    /// Matches for the current position, ascending `abs_row` (oldest -> newest).
    /// Rebuilt against the live grid on every step, so `abs_row` is always fresh.
    matches: Vec<SearchMatch>,
    /// Index into `matches` of the current (highlighted) match.
    current: usize,
}

impl Pane {
    pub fn new(rows: u16, cols: u16) -> Self {
        Pane::with_scrollback(rows, cols, scrollback_lines())
    }

    /// Construct with an explicit scrollback cap (the [`Pane::new`] path resolves
    /// it from config). Lets tests exercise the at-cap `truncated` flag without
    /// touching a process-global env var.
    fn with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Self {
        Pane::with_limits(rows, cols, scrollback, MAX_BLOCK_BYTES, MAX_RETAINED_BYTES)
    }

    /// Construct with explicit block-memory budgets. Lets tests drive eviction
    /// with tiny caps instead of feeding megabytes.
    fn with_limits(
        rows: u16,
        cols: u16,
        scrollback: usize,
        max_block_bytes: usize,
        max_retained_bytes: usize,
    ) -> Self {
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
            retained_bytes: 0,
            max_block_bytes: max_block_bytes.max(1),
            max_retained_bytes: max_retained_bytes.max(1),
            pending_cmd: None,
            selected_block: None,
            search: None,
        }
    }

    /// Feed raw PTY output. The OSC 133 scanner splits the stream: passthrough
    /// spans advance the Term (which buffers partial escape sequences across
    /// calls); recognized FinalTerm markers are stripped and queued for the block
    /// store. Partial markers split across reads carry over in the scanner state.
    pub fn feed(&mut self, bytes: &[u8]) {
        for seg in self.scanner.scan(bytes) {
            match seg {
                Seg::Pass(b) => {
                    // While a block is open, capture the SAME output bytes that
                    // advance the Term - held pre-grid, so the capture is
                    // scroll- and scrollback-independent (the whole point: the
                    // old grid-anchor path drifted once history saturated).
                    // Prompt/echo bytes (before `C`) arrive with `open == None`
                    // and are never buffered, so block text is pure output.
                    if let Some(open) = self.open.as_mut() {
                        open.raw.extend_from_slice(&b);
                        // Bound a possibly never-closing block (`tail -f`, a busy
                        // build log). Hysteresis: let raw grow to 2x the cap, then
                        // drain the head back to the cap - amortized O(1) per byte
                        // (draining on every append is O(cap) per feed once
                        // saturated, i.e. O(N*cap) for a runaway stream).
                        // `finalize_open`/`read_open` slice to the exact cap, so
                        // the retained/returned text is still strictly bounded.
                        if open.raw.len() > self.max_block_bytes.saturating_mul(2) {
                            let cut = open.raw.len() - self.max_block_bytes;
                            open.raw.drain(..cut);
                            open.truncated = true;
                        }
                    } else if let Some(p) = self.pending_cmd.as_mut() {
                        // Between `B` and `C`: byte-capture the command echo,
                        // same tail-bound as the output span.
                        p.raw.extend_from_slice(&b);
                        if p.raw.len() > self.max_block_bytes.saturating_mul(2) {
                            let cut = p.raw.len() - self.max_block_bytes;
                            p.raw.drain(..cut);
                        }
                    }
                    self.processor.advance(&mut self.term, &b);
                }
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
        // Reflow rewraps history, staling any (line, col) anchors: drop the
        // selection rather than render a highlight over the wrong cells
        // (AC2-UI). The display offset is clamped by alacritty's own resize.
        self.term.selection = None;
        self.selected_block = None;
        // Reflow stales the stored match anchors too (v12, AC3-FR): drop the
        // search snapshot so a later `search_step` no-ops rather than selecting
        // a drifted span.
        self.search = None;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// (x-fbb1) True when this pane is a pristine, idle mux shell - safe to reap
    /// for `.`=here take-over. Requires positive OSC 133 evidence: markers are
    /// active (`saw_marker`, so the signal is trustworthy - an un-integrated
    /// shell reads false and is never reaped), NO command is running (`open` is
    /// None, which catches both a foreground program and an `exec` replacement,
    /// since `exec` emits `C` and never its `D`), and NO command has ever run
    /// (`blocks` empty - any completed command, including a background or stopped
    /// job, leaves a block). Anything the shell has touched fails this, so
    /// take-over never reaps live work (the P1 the tcgetpgrp check missed).
    pub fn is_pristine_idle_shell(&self) -> bool {
        self.saw_marker && self.open.is_none() && self.blocks.is_empty()
    }

    /// Snapshot the ACTIVE grid (alt screen included - `Term` swaps grids
    /// internally, so vim's screen is what a reattaching client redraws from)
    /// into a self-contained [`Frame`].
    pub fn frame(&self) -> Frame {
        let grid = self.term.grid();
        // Display offset (US1): 0 = live bottom, >0 = scrolled into history.
        // Viewport row `r` shows grid line `r - offset` (alacritty's
        // `viewport_to_point`), so at offset 0 this is byte-identical to the
        // live-view render.
        let offset = grid.display_offset() as i32;
        // Selection (US2): resolved to a concrete grid-point range once, then
        // each cell is tested against it so every co-viewer's frame carries the
        // same highlight (brief Locked 4). `None` when nothing is selected.
        let sel = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term));
        let cursor_point = grid.cursor.point;
        let mut cells = Vec::with_capacity(self.rows as usize * self.cols as usize);
        for r in 0..(self.rows as usize) {
            let line = Line(r as i32 - offset);
            for c in 0..(self.cols as usize) {
                let cell = &grid[line][Column(c)];
                let mut mapped = map_cell(cell.c, cell.fg, cell.bg, cell.flags);
                if sel
                    .as_ref()
                    .is_some_and(|range| range.contains(Point::new(line, Column(c))))
                {
                    mapped.flags |= cell_flags::SELECTED;
                }
                cells.push(mapped);
            }
        }
        // While scrolled the live cursor sits off-screen at the bottom; hiding
        // it (tmux copy-mode behavior) keeps a stale caret from rendering in
        // history. At offset 0 the cursor row is unchanged.
        let scrolled = offset != 0;
        Frame {
            rows: self.rows,
            cols: self.cols,
            cells,
            cursor_row: (cursor_point.line.0 + offset).max(0) as u16,
            cursor_col: cursor_point.column.0 as u16,
            cursor_visible: !scrolled && self.term.mode().contains(TermMode::SHOW_CURSOR),
            scroll_offset: offset.max(0).min(u16::MAX as i32) as u16,
        }
    }

    /// The visible screen as trimmed plain text (test/debug helper; the wire
    /// format is [`Pane::frame`]).
    #[allow(dead_code)]
    pub fn text(&self) -> String {
        let frame = self.frame();
        frame_text(&frame)
    }

    // -- Scroll (US1) ----------------------------------------------------------

    /// Scroll the display by `delta` lines (positive = up into history). Clamps
    /// at the oldest history line and at the live bottom (alacritty owns the
    /// clamp - AC1-EDGE, no panic, no wraparound). Returns the resulting offset.
    pub fn scroll(&mut self, delta: i32) -> usize {
        self.term.scroll_display(Scroll::Delta(delta));
        self.display_offset()
    }

    /// Snap back to the live bottom (offset 0). A keystroke to a scrolled pane
    /// calls this so input always lands on the visible line (Invariant).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Lines currently scrolled above the live bottom; 0 = live. Drives the
    /// `[+N]` indicator and the frame's cursor-hide (AC1-UI: present iff != 0).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    // -- Selection (US2) -------------------------------------------------------

    /// Begin a selection anchored at viewport cell `(row, col)`. The point is
    /// mapped through the CURRENT display offset to a stable grid point, so the
    /// selection stays pinned to content even if the view later scrolls.
    pub fn selection_start(&mut self, row: u16, col: u16) {
        let point = self.viewport_point(row, col);
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
    }

    /// Extend the in-progress selection to viewport cell `(row, col)`.
    pub fn selection_update(&mut self, row: u16, col: u16) {
        let point = self.viewport_point(row, col);
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, Side::Right);
        }
    }

    /// Drop any selection (release-with-no-drag, resize, pane close - AC2-UI).
    pub fn selection_clear(&mut self) {
        self.term.selection = None;
        self.selected_block = None;
    }

    /// The selected text, joining soft-wrapped lines per alacritty's semantic
    /// rules and reaching into scrolled-off history (AC2-EDGE). `None` when
    /// there is no non-empty selection.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.is_some()
    }

    /// Map a viewport cell to a grid point through the current display offset,
    /// clamping the column into the grid so an edge drag never indexes O.O.B.
    fn viewport_point(&self, row: u16, col: u16) -> Point {
        let col = (col as usize).min(self.cols.saturating_sub(1) as usize);
        let row = (row as usize).min(self.rows.saturating_sub(1) as usize);
        viewport_to_point(self.display_offset(), Point::new(row, Column(col)))
    }

    // -- Block navigation (x-38c4) ---------------------------------------------

    /// A grid row as an offset from the TOP of retained scrollback:
    /// `grid_line + history_size`. This is the block navigation anchor. It is
    /// STABLE while history is unsaturated - as new output scrolls a fixed
    /// content line upward, its grid index drops by exactly the amount history
    /// grows, so the sum holds - and drifts only once the scrollback cap starts
    /// dropping lines. Navigation is best-effort by contract (the brief): every
    /// resolution clamps into the live window, so a drifted anchor lands on a
    /// nearby row, never a panic and never a wrong-text read (block text/command
    /// come from the byte-captured store, not these rows).
    fn anchor_row(&self) -> i64 {
        let grid = self.term.grid();
        grid.cursor.point.line.0 as i64 + grid.history_size() as i64
    }

    /// The [`anchor_row`](Self::anchor_row) currently at the viewport top.
    fn viewport_top_abs(&self) -> i64 {
        self.term.grid().history_size() as i64 - self.display_offset() as i64
    }

    /// Scroll so `abs_row` sits at the viewport top, clamped into the live window
    /// (an evicted/oldest target lands on the oldest retained line, never a stale
    /// row). Returns the resulting display offset.
    fn scroll_to_abs(&mut self, abs_row: i64) -> usize {
        let hist = self.term.grid().history_size() as i64;
        let target = (hist - abs_row).clamp(0, hist);
        let cur = self.display_offset() as i64;
        self.term
            .scroll_display(Scroll::Delta((target - cur) as i32));
        self.display_offset()
    }

    /// The block start anchors oldest -> newest: every retained block, then the
    /// still-open block (the newest boundary for a `Next` walk). Jump INCLUDES
    /// the open block (you can scroll to a streaming command); `block_select` /
    /// `rerun_command` deliberately do NOT (you cannot copy/rerun output that has
    /// not finished) - they iterate `self.blocks` only.
    fn block_anchors(&self) -> Vec<(i64, u64)> {
        let mut anchors: Vec<(i64, u64)> =
            self.blocks.iter().map(|b| (b.start_abs, b.seq)).collect();
        if let Some(o) = &self.open {
            anchors.push((o.start_abs, o.seq));
        }
        anchors
    }

    /// Move the shared scroll to the `dir`-adjacent command block's first row.
    /// From the live tail a `Prev` jumps to the newest block; each subsequent
    /// `Prev` steps one block older, clamping at the oldest retained (AC1-EDGE).
    /// `Next` steps newer and snaps to the live bottom past the newest.
    pub fn block_jump(&mut self, dir: BlockDir) -> BlockJumpOutcome {
        let anchors = self.block_anchors();
        if anchors.is_empty() {
            return BlockJumpOutcome::NoBlocks;
        }
        let scrolled = self.display_offset() > 0;
        let top = self.viewport_top_abs();
        let target = match dir {
            // Live view: reference is +inf so the newest block is "prev".
            BlockDir::Prev => {
                let reference = if scrolled { top } else { i64::MAX };
                anchors
                    .iter()
                    .filter(|(a, _)| *a < reference)
                    .max_by_key(|(a, _)| *a)
                    .copied()
                    // Already at/above the oldest: rest on it (idempotent).
                    .or_else(|| anchors.first().copied())
            }
            BlockDir::Next if !scrolled => None, // already live
            BlockDir::Next => anchors
                .iter()
                .filter(|(a, _)| *a > top)
                .min_by_key(|(a, _)| *a)
                .copied(),
        };
        match target {
            Some((abs, seq)) => {
                self.scroll_to_abs(abs);
                BlockJumpOutcome::Moved { seq, abs_row: abs }
            }
            None => {
                // Nothing newer (or already live): return to the live bottom.
                self.scroll_to_bottom();
                BlockJumpOutcome::AtLive
            }
        }
    }

    /// Move the block-scoped selection to the `dir`-adjacent block (the whole
    /// command + output span) and bring it into view, so the existing copy chain
    /// (prefix+y) yanks it. First press (no current block) selects the newest;
    /// repeated presses walk. Returns the now-selected block's seq, or `None`
    /// when the pane has no retained blocks.
    pub fn block_select(&mut self, dir: BlockDir) -> Option<u64> {
        let seqs: Vec<u64> = self.blocks.iter().map(|b| b.seq).collect();
        let &newest = seqs.last()?;
        let target = match self
            .selected_block
            .and_then(|c| seqs.iter().position(|&s| s == c))
        {
            // No live selection (or the selected block was evicted): start newest.
            None => newest,
            Some(i) => match dir {
                BlockDir::Prev => seqs[i.saturating_sub(1)],
                BlockDir::Next => seqs[(i + 1).min(seqs.len() - 1)],
            },
        };
        self.set_block_selection(target);
        self.selected_block = Some(target);
        Some(target)
    }

    /// Set the grid selection to span block `seq` (command line through output
    /// end) and scroll it into view. Rows clamp into the live grid so an
    /// evicted/drifted anchor selects a valid span, never an O.O.B. point.
    fn set_block_selection(&mut self, seq: u64) {
        let Some(b) = self.blocks.iter().find(|b| b.seq == seq) else {
            return;
        };
        let (start_abs, end_abs) = (b.start_abs, b.end_abs);
        let grid = self.term.grid();
        let hist = grid.history_size() as i64;
        let top = grid.topmost_line().0 as i64;
        let bot = grid.bottommost_line().0 as i64;
        let sl = (start_abs - hist).clamp(top, bot) as i32;
        let el = (end_abs - hist).clamp(top, bot) as i32;
        let last_col = Column(self.cols.saturating_sub(1) as usize);
        let mut sel = Selection::new(
            SelectionType::Simple,
            Point::new(Line(sl), Column(0)),
            Side::Left,
        );
        sel.update(Point::new(Line(el), last_col), Side::Right);
        self.term.selection = Some(sel);
        self.scroll_to_abs(start_abs);
    }

    /// The command line to re-send for the selected block, else the newest
    /// block's. `None` when there is no block or the target block captured no
    /// command (a bare-`C` emitter with no `B` marker). A `selected_block` that
    /// was evicted since selection heals to the newest (same repair as
    /// `block_select`), rather than reporting "nothing to rerun" on a stale ref.
    pub fn rerun_command(&self) -> Option<String> {
        let target = self
            .selected_block
            .and_then(|seq| self.blocks.iter().find(|b| b.seq == seq))
            .or_else(|| self.blocks.back())?;
        target.cmd.clone()
    }

    // -- In-scrollback search (x-e780, v12) ------------------------------------

    /// Scan the pane's whole retained buffer (history + live grid) for `query`
    /// as a case-insensitive plain substring (Locked 6: no regex, single
    /// rendered row), storing the match snapshot; jump the shared scroll to the
    /// initial match and highlight it via `term.selection` (the v7 `SELECTED`
    /// broadcast path). Returns `(total, current)` where `current` is 1-based and
    /// `total == 0` means no matches (no scroll, highlight cleared - AC1-ERR).
    /// An empty `query` is a clear (Boundaries), never a scan that matches every
    /// row. A new call replaces any prior search (Invariants: last-writer-wins).
    pub fn search_open(&mut self, query: &str) -> (u32, u32) {
        let needle: Vec<char> = query.chars().collect();
        if needle.is_empty() {
            self.search_clear();
            return (0, 0);
        }
        let matches = self.scan_matches(&needle);
        if matches.is_empty() {
            // No match: drop any prior highlight/state, do not move the viewport.
            self.search_clear();
            return (0, 0);
        }
        // Initial: nearest match at/above the current viewport top scrolling up,
        // else the newest (Discretion 2 recommended default).
        let top = self.viewport_top_abs();
        let current = matches
            .iter()
            .rposition(|m| m.abs_row <= top)
            .unwrap_or(matches.len() - 1);
        let total = matches.len() as u32;
        self.search = Some(SearchState {
            needle,
            matches,
            current,
        });
        self.apply_current_match();
        (total, current as u32 + 1)
    }

    /// Walk the active search: `Prev` toward older (smaller `abs_row`), `Next`
    /// toward newer, resting at the ends (AC2-EDGE locked default). Re-scans the
    /// live grid first (x-1e67: stored anchors drift once scrollback saturates,
    /// so the frozen list would re-highlight the wrong row), then re-jumps and
    /// re-highlights. `None` only when no search is active. If every match has
    /// aged out of retained scrollback, the query now matches nothing: drop the
    /// highlight and report `(0, 0)` - the SAME zero-match reply as a no-match
    /// open, so the initiator hears "no matches" and the cleared highlight
    /// repaints (returning `None` here would leave a stale highlight - the
    /// server's no-active-search arm does not broadcast).
    pub fn search_step(&mut self, dir: BlockDir) -> Option<(u32, u32)> {
        let cur = self.search.as_ref()?.current;
        let needle = self.search.as_ref()?.needle.clone();
        // Fresh anchor of the currently-highlighted match: the live selection
        // tracks it across eviction (alacritty rotates it), so its start row is
        // the match's row in the re-scanned list. Re-anchor the walk to it, else
        // a stale ordinal skips matches that aged out from the front of the list
        // (gemini/codex review): on B in [A,B,C,D], A evicting leaves [B,C,D] but
        // `cur` still 1, so `Next` would jump to D and skip C.
        let anchor_abs = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term))
            .map(|r| r.start.line.0 as i64 + self.term.grid().history_size() as i64);
        let matches = self.scan_matches(&needle);
        if matches.is_empty() {
            self.search_clear();
            return Some((0, 0));
        }
        let last = matches.len() - 1;
        // Locate the current match in the fresh list by its live anchor; fall
        // back to the clamped ordinal if it aged out. An unchanged list (the pane
        // was not streaming) resolves to `cur`, so the walk is byte-identical to
        // the old snapshot behavior.
        let base = anchor_abs
            .and_then(|a| matches.iter().position(|m| m.abs_row == a))
            .unwrap_or(cur.min(last));
        let current = match dir {
            BlockDir::Prev => base.saturating_sub(1),
            BlockDir::Next => (base + 1).min(last),
        };
        let total = matches.len() as u32;
        self.search = Some(SearchState {
            needle,
            matches,
            current,
        });
        self.apply_current_match();
        Some((total, current as u32 + 1))
    }

    /// Drop the active search: clear the highlight and the snapshot. Idempotent
    /// (a no-active-search clear is a harmless no-op the server still reports).
    pub fn search_clear(&mut self) {
        self.term.selection = None;
        self.search = None;
        // search and block-select share term.selection; releasing it must not
        // leave a stale rerun target.
        self.selected_block = None;
    }

    /// Whether a search is active (non-empty snapshot). Lets the server reply a
    /// no-op `Notice` for a `SearchStep`/`SearchClear` on a searchless pane.
    pub fn has_search(&self) -> bool {
        self.search.as_ref().is_some_and(|s| !s.matches.is_empty())
    }

    /// Scroll to the current match and set `term.selection` over its span. Reads
    /// the current index from `self.search`; a no-op if there is none.
    fn apply_current_match(&mut self) {
        let Some(m) = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.current).copied())
        else {
            return;
        };
        // Clamp the stored anchor into the live grid (mirrors set_block_selection)
        // so an evicted/drifted row selects a valid span, never an O.O.B. point.
        let grid = self.term.grid();
        let hist = grid.history_size() as i64;
        let top = grid.topmost_line().0 as i64;
        let bot = grid.bottommost_line().0 as i64;
        let line = (m.abs_row - hist).clamp(top, bot) as i32;
        let mut sel = Selection::new(
            SelectionType::Simple,
            Point::new(Line(line), Column(m.col_start)),
            Side::Left,
        );
        sel.update(Point::new(Line(line), Column(m.col_end)), Side::Right);
        self.term.selection = Some(sel);
        self.selected_block = None; // search now owns the shared selection
        self.scroll_to_abs(m.abs_row);
    }

    /// Scan every retained row for `needle`, case-insensitively, returning
    /// non-overlapping matches oldest -> newest (ascending `abs_row`). Matches do
    /// not cross rows (Locked 6). Case folding is ASCII-only.
    // ponytail: ASCII case fold; Unicode fold (Turkish i, ß) deferred until asked.
    fn scan_matches(&self, needle: &[char]) -> Vec<SearchMatch> {
        let grid = self.term.grid();
        let hist = grid.history_size() as i64;
        let top = grid.topmost_line().0 as i64;
        let bot = grid.bottommost_line().0 as i64;
        let cols = self.cols as usize;
        let nlen = needle.len();
        let mut out = Vec::new();
        // Reused across rows (cleared per row) so a full 10k-line scrollback scan
        // does not allocate a fresh Vec per row. (gemini review, MEDIUM)
        let mut cells: Vec<(char, usize)> = Vec::with_capacity(cols);
        let mut ln = top;
        while ln <= bot {
            let row = &grid[Line(ln as i32)];
            // (char, grid column) for each visible cell, spacers skipped so the
            // char stream and the column map stay aligned for wide chars.
            cells.clear();
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let ch = if cell.flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                };
                cells.push((ch, c));
            }
            let abs_row = ln + hist;
            let mut i = 0;
            while i + nlen <= cells.len() {
                let hit = (0..nlen).all(|k| cells[i + k].0.eq_ignore_ascii_case(&needle[k]));
                if hit {
                    out.push(SearchMatch {
                        abs_row,
                        col_start: cells[i].1,
                        col_end: cells[i + nlen - 1].1,
                    });
                    i += nlen; // non-overlapping
                } else {
                    i += 1;
                }
            }
            ln += 1;
        }
        out
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
                // The command echo was byte-captured between `B` and here (same
                // discipline as the output span: never grid-scraped). Absent `B`
                // (a bare-C emitter), the block anchors at `C` with no command.
                let (start_abs, cmd) = match self.pending_cmd.take() {
                    Some(p) => {
                        let (text, _) = capped_text(&p.raw, self.max_block_bytes);
                        let cmd = text.trim().to_string();
                        (p.start_abs, (!cmd.is_empty()).then_some(cmd))
                    }
                    None => (self.anchor_row(), None),
                };
                let seq = self.next_seq;
                self.next_seq += 1;
                self.open = Some(OpenBlock {
                    seq,
                    raw: Vec::new(),
                    truncated: false,
                    start_abs,
                    cmd,
                });
            }
            Osc133::CmdDone { exit } => self.finalize_open(exit),
            Osc133::CmdStart => {
                self.pending_cmd = Some(PendingCmd {
                    start_abs: self.anchor_row(),
                    raw: Vec::new(),
                })
            }
            Osc133::PromptStart => {}
        }
    }

    /// Close the open block: slice its accumulated bytes to the strict per-block
    /// cap (feed's hysteresis lets raw run up to 2x), ANSI-strip to clean text,
    /// and retain it.
    fn finalize_open(&mut self, exit: Option<i32>) {
        let Some(open) = self.open.take() else { return };
        let (text, capped) = capped_text(&open.raw, self.max_block_bytes);
        self.retained_bytes += text.len();
        // Content end: at `D` the cursor sits where the next prompt draws - one
        // row past the output when it ended in a newline (col 0). Never above the
        // start row.
        let end_abs = self.anchor_row().max(open.start_abs);
        self.blocks.push_back(Block {
            seq: open.seq,
            exit,
            truncated: open.truncated || capped,
            text,
            start_abs: open.start_abs,
            end_abs,
            cmd: open.cmd,
        });
        // Front-drop oldest until within BOTH the count and byte budgets. Always
        // keep at least the block just pushed (a single over-budget block is
        // already tail-capped above).
        while self.blocks.len() > 1
            && (self.blocks.len() > MAX_BLOCKS || self.retained_bytes > self.max_retained_bytes)
        {
            if let Some(dropped) = self.blocks.pop_front() {
                self.retained_bytes -= dropped.text.len();
            }
        }
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
    // Err(()) is a documented BLOCK_UNAVAILABLE sentinel, not an error type; a
    // real error enum would ripple through every caller for no signal gained.
    #[allow(clippy::result_unit_err)]
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

    /// Live snapshot of a still-streaming block (no `D` yet): the output bytes so
    /// far, sliced to the per-block cap and ANSI-stripped, `complete=false`. Never
    /// hangs waiting for the terminator (AC2-FR).
    fn read_open(&self, open: &OpenBlock) -> BlockRead {
        let (text, capped) = capped_text(&open.raw, self.max_block_bytes);
        BlockRead {
            seq: Some(open.seq),
            exit: None,
            complete: false,
            truncated: open.truncated || capped,
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
///
/// Mouse-reporting modes (1000/1002/1003/1005/1006/1007) are deliberately
/// EXCLUDED (Phase 5, brief Locked 2): the client holds mouse capture on
/// permanently and forwards every pane-rect event; the server routes by reading
/// the pane's live modes directly (see `route_mouse`), never through this sync.
/// Letting a focus change toggle the client terminal's mouse reporting would
/// fight that capture, so it never crosses the wire.
pub fn mode_diff(old: Modes, new: Modes) -> Vec<u8> {
    let mut out = Vec::new();
    let mut dec = |on: bool, was: bool, code: &str| {
        if on != was {
            out.extend_from_slice(format!("\x1b[?{code}{}", if on { 'h' } else { 'l' }).as_bytes());
        }
    };
    dec(new.app_cursor, old.app_cursor, "1");
    dec(new.focus_in_out, old.focus_in_out, "1004");
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

/// Slice raw output to its most-recent `max` bytes, then ANSI-strip to clean
/// text; returns `(text, capped)` where `capped` is whether the head was
/// dropped. `feed`'s hysteresis lets an open block's raw run up to `2 * max`, so
/// this enforces the strict per-block cap at finalize/read time. Slicing on a
/// raw byte offset may start mid-escape or mid-codepoint, but that only happens
/// on the already-`capped` (flagged) path and `strip_ansi` decodes lossily.
fn capped_text(raw: &[u8], max: usize) -> (String, bool) {
    if raw.len() > max {
        (strip_ansi(&raw[raw.len() - max..]), true)
    } else {
        (strip_ansi(raw), false)
    }
}

/// Strip ANSI escape sequences from raw terminal output, returning escape-free
/// text (`from_utf8_lossy`, so malformed bytes yield the replacement char, never
/// a panic). Mirrors [`Osc133Scanner`]'s discipline: a tiny byte state machine,
/// bounded, panic-free. OSC 133 never reaches here - the scanner already split
/// it into `Seg::Marker`. A partial/unterminated escape at the buffer end is
/// held mid-state and dropped, never emitted as literal text.
fn strip_ansi(bytes: &[u8]) -> String {
    #[derive(Clone, Copy)]
    enum S {
        /// Bytes flow to the output.
        Ground,
        /// Saw `ESC`; the next byte selects the sequence kind.
        Esc,
        /// In a CSI (`ESC [ ...`); consuming until a final byte `0x40..=0x7e`.
        Csi,
        /// In an nF escape (`ESC` + intermediate `0x20..=0x2f`, e.g. charset
        /// designation `ESC ( B`); consuming intermediates until a final byte
        /// `0x30..=0x7e`.
        EscInt,
        /// In a string sequence (OSC/DCS/PM/APC); consuming until ST/BEL.
        Str,
        /// In a string, saw `ESC`; a following `\` is ST (terminator).
        StrEsc,
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut st = S::Ground;
    for &b in bytes {
        st = match st {
            S::Ground => {
                if b == 0x1b {
                    S::Esc
                } else {
                    out.push(b);
                    S::Ground
                }
            }
            S::Esc => match b {
                b'[' => S::Csi,
                // OSC, DCS, PM, APC all run until ST (or BEL for OSC).
                b']' | b'P' | b'^' | b'_' => S::Str,
                // nF escape: an intermediate byte opens a multi-byte sequence
                // (charset designation `ESC ( B`, `ESC ) 0`, ...) that ends only
                // at a final byte - so the final (e.g. the `B`) is not leaked.
                0x20..=0x2f => S::EscInt,
                0x1b => S::Esc, // ESC ESC: restart the escape.
                // A simple 2-byte escape (`ESC =`, `ESC M`, ...): drop both.
                _ => S::Ground,
            },
            S::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    S::Ground
                } else {
                    S::Csi
                }
            }
            S::EscInt => {
                // More intermediates stay in EscInt; a final byte ends the seq.
                if (0x30..=0x7e).contains(&b) {
                    S::Ground
                } else {
                    S::EscInt
                }
            }
            S::Str => match b {
                0x07 => S::Ground, // BEL terminates OSC.
                0x1b => S::StrEsc, // maybe ST.
                _ => S::Str,
            },
            S::StrEsc => match b {
                b'\\' => S::Ground, // ST.
                0x1b => S::StrEsc,  // another ESC; still awaiting `\`.
                _ => S::Str,        // ESC then non-`\`: still in the string.
            },
        };
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Max retained completed blocks per pane. A count backstop; retained block
/// memory is bounded primarily by BYTES (below). Oldest front-dropped, and an
/// evicted block reads `BLOCK_UNAVAILABLE` (AC1-FR).
const MAX_BLOCKS: usize = 512;

/// Per-block captured-text cap (256 KiB). A single command's stored span is
/// trimmed to its most-recent tail past this, so one huge command cannot pin
/// memory the total-budget eviction can never reclaim (the last block stays).
const MAX_BLOCK_BYTES: usize = 256 * 1024;

/// Total retained-block-text budget per pane (8 MiB). Oldest blocks front-drop
/// until under it, so a high-output agent's blocks stay bounded regardless of
/// the (now 100k-line) scrollback.
const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// A completed command block: the output span between an OSC 133 `C` (output
/// start) and `D` (command done), with its text captured at `D`.
#[derive(Debug, Clone)]
struct Block {
    seq: u64,
    exit: Option<i32>,
    truncated: bool,
    text: String,
    /// Absolute rows (see [`Pane::cursor_abs`]) of the block's command line and
    /// output end - the navigation anchors (jump/select). Unlike `text` these
    /// are grid-derived and BEST-EFFORT: reflow and scrollback saturation drift
    /// them, and every resolution clamps into the retained window, so a stale
    /// anchor degrades to a nearby row, never a panic or wrong text.
    start_abs: i64,
    end_abs: i64,
    /// The command line, byte-captured between `B` and `C` and ANSI-stripped.
    /// `None` when the emitter sent no `B`. Best-effort for rerun (a readline
    /// redraw is captured raw); the idle guard + user confirmation cover the rest.
    cmd: Option<String>,
}

/// A block whose `C` was seen but not yet its `D`: its output bytes accumulate
/// in `raw` (pre-grid), ANSI-stripped to text at `D` or on a live read.
#[derive(Debug, Clone)]
struct OpenBlock {
    seq: u64,
    /// Accumulated passthrough output bytes since `C`, tail-bounded by
    /// `max_block_bytes` (dropping the head sets `truncated`).
    raw: Vec<u8>,
    /// Head bytes were dropped by the per-block cap (this block only, decoupled
    /// from the pane's scrollback fullness).
    truncated: bool,
    /// Absolute row of the command line (the `B` point, else the `C` point).
    start_abs: i64,
    /// See [`Block::cmd`].
    cmd: Option<String>,
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

/// The result of a [`Pane::block_jump`]. `Moved` carries the landed block's seq
/// and its first-row anchor (the row navigation resolved to); `NoBlocks` and
/// `AtLive` are the visible edge cases the client renders as a one-line notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockJumpOutcome {
    Moved { seq: u64, abs_row: i64 },
    NoBlocks,
    AtLive,
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
        assert!(to_vim.contains("\x1b[?2004h"), "{to_vim:?}");
        // Mouse-reporting modes are NOT synced to the client (Phase 5, Locked
        // 2): the client keeps capture on permanently and the server routes.
        assert!(
            !to_vim.contains("\x1b[?1003h"),
            "mouse mode must not sync: {to_vim:?}"
        );
        assert!(
            !to_vim.contains("\x1b[?1006h"),
            "mouse mode must not sync: {to_vim:?}"
        );
        // The way back RESETS exactly what was set (still excluding mouse).
        let to_plain = String::from_utf8(mode_diff(vim, plain)).unwrap();
        assert!(to_plain.contains("\x1b[?1l"), "{to_plain:?}");
        assert!(to_plain.contains("\x1b[?2004l"), "{to_plain:?}");
        assert!(!to_plain.contains("\x1b[?1003l"), "{to_plain:?}");
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
        hostile.extend(std::iter::repeat_n(b'X', 500));
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
        // Locked decision 2: block text is pure output - the `$ ` prompt/echo
        // arrives before `C` (open == None), so it is never buffered.
        assert_eq!(b0.text, "hello");

        let b1 = pane.read_block(BlockSel::Seq(1)).unwrap();
        assert_eq!((b1.seq, b1.exit), (Some(1), Some(1)));
        assert_eq!(b1.text, "boom");

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
        assert_eq!(read.text, "partial");
    }

    #[test]
    fn completed_block_survives_width_resize() {
        // AC2-EDGE flip (amendment): a completed block returns its CAPTURED text
        // after a width-changing resize, never BLOCK_UNAVAILABLE.
        let mut pane = Pane::new(24, 80);
        run_command(&mut pane, "keepme", 0);
        pane.resize(24, 40); // width change reflows the grid
        let read = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert_eq!(read.text, "keepme");
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
    fn block_truncated_flag_fires_at_per_block_byte_cap() {
        // Locked decision 4: `truncated` now means THIS block's head bytes were
        // dropped by the per-block byte cap, decoupled from the pane's scrollback
        // fullness. A block under the cap is exact; one over it is flagged.
        let mut pane = Pane::with_limits(4, 20, 8, 10, 1000);
        run_command(&mut pane, "small", 0); // 5B <= 10B cap
        assert!(
            !pane.read_block(BlockSel::Seq(0)).unwrap().truncated,
            "under the per-block cap: exact, not truncated"
        );
        run_command(&mut pane, "0123456789ABCDEF", 0); // 16B > 10B cap
        assert!(
            pane.read_block(BlockSel::Last).unwrap().truncated,
            "over the per-block cap: head dropped, flagged truncated"
        );
    }

    #[test]
    fn block_capture_survives_scrollback_saturation() {
        // AC2-HP: fill a tiny scrollback well past its cap, THEN run a command
        // block. Its output is captured exactly - the pre-fix grid-anchor path
        // returned a wrong span here once `history_size()` saturated.
        let mut pane = Pane::with_scrollback(4, 20, 8);
        for i in 0..50 {
            pane.feed(format!("filler{i}\r\n").as_bytes());
        }
        run_command(&mut pane, "exact-output", 0);
        let read = pane.read_block(BlockSel::Last).unwrap();
        assert_eq!(
            read.text, "exact-output",
            "captured pre-grid, scroll-independent"
        );
        assert!(!read.truncated, "small block under the byte cap is exact");
    }

    #[test]
    fn runaway_open_block_is_memory_bounded() {
        // AC1-FR: an open block (no `D`) fed more than `max_block_bytes` keeps only
        // the most-recent tail, flagged truncated - a never-closing command
        // (`tail -f`) cannot pin unbounded server memory.
        let mut pane = Pane::with_limits(4, 80, 100, 16, 1000);
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;C\x07");
        for _ in 0..100 {
            pane.feed(b"0123456789"); // 1000B total, per-block cap 16B
        }
        let read = pane.read_block(BlockSel::Last).unwrap();
        assert!(!read.complete, "still streaming (no D)");
        assert!(read.text.len() <= 16, "tail-bounded: {:?}", read.text);
        assert!(read.truncated, "head dropped -> truncated");
        assert!(
            read.text.ends_with("0123456789"),
            "keeps the most-recent tail: {:?}",
            read.text
        );
    }

    #[test]
    fn block_captures_logical_not_wrapped_lines() {
        // AC1-EDGE: a 200-char logical line to an 80-col pane is captured as one
        // 200-char line (the grid path would have wrapped it to 3 rows).
        let mut pane = Pane::new(24, 80);
        let long = "x".repeat(200);
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;C\x07");
        pane.feed(long.as_bytes());
        pane.feed(b"\x1b]133;D;0\x07");
        assert_eq!(pane.read_block(BlockSel::Last).unwrap().text, long);
    }

    #[test]
    fn strip_ansi_removes_escapes_keeps_visible_text() {
        // AC1-ERR: CSI color, a non-133 OSC title, a 2-byte escape, and invalid
        // UTF-8 -> visible characters only, no ESC byte, no panic.
        let out = strip_ansi(b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b=text\xff!");
        assert!(!out.contains('\x1b'), "no ESC survives: {out:?}");
        assert!(out.starts_with("redtext"), "visible text kept: {out:?}");
        assert!(out.ends_with('!'), "trailing text kept: {out:?}");
        assert!(
            out.contains('\u{fffd}'),
            "bad UTF-8 -> replacement char: {out:?}"
        );
    }

    #[test]
    fn strip_ansi_drops_partial_trailing_escape() {
        // An unterminated escape at the buffer end is dropped, not emitted raw
        // (Errors: partial escape at an open block's end).
        assert_eq!(strip_ansi(b"ok\x1b[3"), "ok"); // partial CSI
        assert_eq!(strip_ansi(b"ok\x1b]0;unterminated"), "ok"); // partial OSC
        assert_eq!(strip_ansi(b"ok\x1b"), "ok"); // bare ESC
    }

    #[test]
    fn strip_ansi_keeps_newlines_tabs_and_cr() {
        // Discretion 4: keep `\n`/`\r`/`\t` verbatim; drop only escape sequences.
        assert_eq!(strip_ansi(b"a\tb\nc\rd"), "a\tb\nc\rd");
    }

    #[test]
    fn strip_ansi_consumes_charset_designation_final_byte() {
        // `ESC ( B` (designate US-ASCII into G0, common in `sgr0`/reset) is a
        // 3-byte nF escape: the intermediate `(` AND the final `B` must both be
        // dropped, not leak a stray `B` into captured text.
        assert_eq!(strip_ansi(b"a\x1b(Bb"), "ab");
        assert_eq!(strip_ansi(b"\x1b)0\x1b(Bx"), "x"); // ESC ) 0 then ESC ( B
        assert_eq!(strip_ansi(b"red\x1b(B\x1b[mtext"), "redtext");
        // Unterminated nF escape at the buffer end is dropped, not leaked.
        assert_eq!(strip_ansi(b"ok\x1b("), "ok");
    }

    #[test]
    fn block_text_is_bounded_by_byte_budgets() {
        // Tiny budgets: 10 bytes per block, 30 bytes total.
        let mut pane = Pane::with_limits(4, 80, 100, 10, 30);

        // A block past the per-block cap is tail-trimmed and flagged truncated.
        // Output is now prompt-excluded: raw is the 16B "0123456789ABCDEF".
        run_command(&mut pane, "0123456789ABCDEF", 0);
        let b0 = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert!(b0.text.len() <= 10, "per-block cap trims: {:?}", b0.text);
        assert!(b0.truncated, "a trimmed block is flagged truncated");
        assert!(b0.text.ends_with("ABCDEF"), "keeps the tail: {:?}", b0.text);

        // Many more blocks: total retained stays under budget, so the oldest
        // evicts by BYTES (not the 512 count cap, which 11 blocks never hit).
        for i in 0..10 {
            run_command(&mut pane, &format!("cmd{i}xxxxx"), 0);
        }
        assert!(
            pane.read_block(BlockSel::Seq(0)).is_err(),
            "oldest evicted by the byte budget"
        );
        assert!(
            pane.read_block(BlockSel::Last).is_ok(),
            "the newest block always survives"
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

    // -- US1 scroll ------------------------------------------------------------

    #[test]
    fn scroll_moves_view_into_history_and_back() {
        // AC1-HP: output taller than the viewport; scrolling up reveals history.
        let mut pane = Pane::new(4, 20);
        for i in 0..20 {
            pane.feed(format!("row{i}\r\n").as_bytes());
        }
        assert_eq!(pane.display_offset(), 0, "starts live");
        assert!(pane.text().contains("row19") && !pane.text().contains("row10"));

        let off = pane.scroll(6);
        assert_eq!(off, 6);
        assert_eq!(pane.display_offset(), 6);
        // The scrolled frame shows earlier rows and not the live bottom.
        let scrolled = pane.text();
        assert!(scrolled.contains("row13"), "reveals history: {scrolled:?}");
        assert!(!scrolled.contains("row19"), "live bottom pinned away");

        // AC1-ERR: back to live restores the bottom exactly once.
        pane.scroll_to_bottom();
        assert_eq!(pane.display_offset(), 0);
        assert!(pane.text().contains("row19"));
    }

    #[test]
    fn scroll_clamps_at_history_top_without_panic() {
        // AC1-EDGE: scrolling past the oldest line clamps, never wraps/panics.
        let mut pane = Pane::new(4, 20);
        for i in 0..30 {
            pane.feed(format!("row{i}\r\n").as_bytes());
        }
        let off = pane.scroll(1_000_000);
        assert!(off > 0, "scrolled up");
        // A second huge scroll is idempotent at the clamp.
        assert_eq!(pane.scroll(1_000_000), off, "clamped at history top");
    }

    #[test]
    fn frame_hides_cursor_and_marks_indicator_when_scrolled() {
        // AC1-UI: the cursor is hidden off-live; offset is the indicator source.
        let mut pane = Pane::new(4, 20);
        for i in 0..20 {
            pane.feed(format!("row{i}\r\n").as_bytes());
        }
        assert!(pane.frame().cursor_visible, "cursor visible at live");
        pane.scroll(3);
        assert!(!pane.frame().cursor_visible, "cursor hidden while scrolled");
        assert_ne!(pane.display_offset(), 0);
    }

    // -- US2 selection ---------------------------------------------------------

    #[test]
    fn selection_and_highlight_pin_to_the_press_cell() {
        // Regression: a drag anchored on the FIRST glyph must select and
        // highlight from that glyph, never N chars late. Repro that motivated
        // this: dragging '[' -> ']' over "[Image #4]" rendered "age #4]" (leading
        // '[Im' skipped). Copy and highlight both derive from the one selection
        // range, so this pins them TOGETHER - the severity fork (copy clipped vs
        // render-only) is moot here because they cannot diverge at the leading edge.
        let mut pane = Pane::new(2, 40);
        pane.feed(b"[Image #4]");
        pane.selection_start(0, 0); // press on '['
        pane.selection_update(0, 9); // drag to ']'
        assert_eq!(pane.selection_text().as_deref(), Some("[Image #4]"));
        let frame = pane.frame();
        assert_eq!(
            frame.cells[0].flags & cell_flags::SELECTED,
            cell_flags::SELECTED,
            "leading '[' highlighted, not skipped",
        );
    }

    #[test]
    fn selection_extracts_text_and_marks_cells() {
        // AC2-HP: a drag selects cells; the text and the SELECTED flags agree.
        let mut pane = Pane::new(2, 20);
        pane.feed(b"abcdefghij");
        assert!(!pane.has_selection());
        pane.selection_start(0, 0);
        pane.selection_update(0, 4);
        assert!(pane.has_selection());
        assert_eq!(pane.selection_text().as_deref(), Some("abcde"));

        // The broadcast frame carries the highlight so co-viewers see it.
        let frame = pane.frame();
        let selected: String = (0..5).map(|c| frame.cells[c].c).collect();
        assert_eq!(selected, "abcde");
        for c in 0..5 {
            assert_eq!(
                frame.cells[c].flags & cell_flags::SELECTED,
                cell_flags::SELECTED
            );
        }
        assert_eq!(
            frame.cells[5].flags & cell_flags::SELECTED,
            0,
            "past selection"
        );
    }

    #[test]
    fn selection_spans_history_lines() {
        // AC2-EDGE: a selection reaching scrolled-off rows extracts them.
        let mut pane = Pane::new(3, 20);
        for i in 0..10 {
            pane.feed(format!("line{i}\r\n").as_bytes());
        }
        pane.scroll(5);
        // Select the whole first visible (historical) row.
        pane.selection_start(0, 0);
        pane.selection_update(0, 5);
        let text = pane.selection_text().unwrap_or_default();
        assert!(text.starts_with("line"), "history row selected: {text:?}");
    }

    #[test]
    fn resize_clears_selection() {
        // AC2-UI: reflow drops a stale selection rather than mis-highlight.
        let mut pane = Pane::new(2, 20);
        pane.feed(b"abcdefghij");
        pane.selection_start(0, 0);
        pane.selection_update(0, 4);
        assert!(pane.has_selection());
        pane.resize(2, 10);
        assert!(!pane.has_selection(), "selection cleared on resize");
    }

    // -- Block navigation (x-38c4) ---------------------------------------------

    /// Feed one full command WITH a command echo between `B` and `C` (so the
    /// block captures a rerun-able command line), then multi-line output tall
    /// enough to push history on a short pane.
    fn run_named(pane: &mut Pane, cmd: &str, output: &str, exit: i32) {
        pane.feed(b"\x1b]133;A\x07$ ");
        pane.feed(format!("\x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07").as_bytes());
        for line in output.lines() {
            pane.feed(format!("{line}\r\n").as_bytes());
        }
        pane.feed(format!("\x1b]133;D;{exit}\x07").as_bytes());
    }

    fn three_blocks() -> Pane {
        // Short pane so three multi-line blocks scroll into history.
        let mut pane = Pane::new(4, 40);
        run_named(&mut pane, "echo one", "a\nb\nc", 0);
        run_named(&mut pane, "echo two", "d\ne\nf", 0);
        run_named(&mut pane, "echo three", "g\nh\ni", 0);
        pane
    }

    #[test]
    fn block_jump_walks_prev_then_next_across_blocks() {
        // AC1-HP (Change 1/2): from the live tail, prev lands on the newest
        // block, then steps older; next steps newer and returns to live.
        let mut pane = three_blocks();
        assert_eq!(pane.display_offset(), 0, "starts at the live tail");

        // Prev walks newest -> oldest (seqs 2, 1, 0), and the returned anchor row
        // is the landed block's first row: the view is scrolled so that row sits
        // at the viewport top (AC1-HP: "the returned row is block N's first row").
        let seqs: Vec<u64> = (0..3)
            .map(|_| match pane.block_jump(BlockDir::Prev) {
                BlockJumpOutcome::Moved { seq, abs_row } => {
                    assert_eq!(pane.viewport_top_abs(), abs_row, "block anchored at top");
                    seq
                }
                other => panic!("expected Moved, got {other:?}"),
            })
            .collect();
        assert_eq!(seqs, vec![2, 1, 0]);
        assert!(pane.display_offset() > 0, "scrolled into history");

        // Past the oldest, prev clamps on block 0 (AC1-EDGE: never a stale row).
        assert!(matches!(
            pane.block_jump(BlockDir::Prev),
            BlockJumpOutcome::Moved { seq: 0, .. }
        ));

        // Next steps back down newer, then snaps to the live bottom.
        assert!(matches!(
            pane.block_jump(BlockDir::Next),
            BlockJumpOutcome::Moved { seq: 1, .. }
        ));
        assert!(matches!(
            pane.block_jump(BlockDir::Next),
            BlockJumpOutcome::Moved { seq: 2, .. }
        ));
        assert_eq!(pane.block_jump(BlockDir::Next), BlockJumpOutcome::AtLive);
        assert_eq!(pane.display_offset(), 0, "back at the live tail");
    }

    #[test]
    fn block_jump_on_markerless_pane_reports_no_blocks() {
        // AC-ERR (Change 2): a pane that never emitted OSC 133 has no blocks; the
        // jump is a visible NoBlocks, never a crash or silent no-op.
        let mut pane = Pane::new(6, 40);
        pane.feed(b"plain shell output\r\nno markers here\r\n");
        assert_eq!(pane.block_jump(BlockDir::Prev), BlockJumpOutcome::NoBlocks);
        assert_eq!(pane.block_jump(BlockDir::Next), BlockJumpOutcome::NoBlocks);
    }

    #[test]
    fn block_jump_clamps_when_oldest_blocks_evicted() {
        // AC1-EDGE: with a tiny retained-byte budget the oldest blocks front-drop;
        // walking prev past the retained window clamps on the oldest RETAINED
        // block (never a dropped/stale row), no panic.
        let mut pane = Pane::with_limits(4, 40, 10_000, 8, 16);
        for i in 0..6 {
            run_named(&mut pane, &format!("cmd{i}"), &format!("out{i}\nx\ny"), 0);
        }
        // At most a couple of blocks survive the 16-byte retained cap.
        let mut seen = Vec::new();
        for _ in 0..8 {
            if let BlockJumpOutcome::Moved { seq, .. } = pane.block_jump(BlockDir::Prev) {
                seen.push(seq);
            }
        }
        assert!(!seen.is_empty(), "walked at least one retained block");
        // The walk floors at the oldest retained seq and never yields an evicted
        // one (all retained seqs are the highest few).
        let oldest = *seen.iter().min().unwrap();
        assert!(oldest >= 4, "evicted blocks are not navigable: {seen:?}");
    }

    #[test]
    fn block_select_walks_and_feeds_the_copy_chain() {
        // AC (Change 3): block-select sets a real selection whose text is the
        // whole block; walking prev moves to the older block; clear resets it.
        let mut pane = three_blocks();
        let newest = pane.block_select(BlockDir::Prev).unwrap();
        assert_eq!(newest, 2);
        let sel = pane.selection_text().unwrap_or_default();
        assert!(
            sel.contains("echo three"),
            "selection spans command: {sel:?}"
        );
        assert!(
            sel.contains('g') && sel.contains('i'),
            "and output: {sel:?}"
        );

        // Walk older.
        assert_eq!(pane.block_select(BlockDir::Prev), Some(1));
        let sel = pane.selection_text().unwrap_or_default();
        assert!(sel.contains("echo two"), "walked to block 1: {sel:?}");

        pane.selection_clear();
        assert!(!pane.has_selection());
        // After a clear the walk restarts at the newest.
        assert_eq!(pane.block_select(BlockDir::Prev), Some(2));
    }

    #[test]
    fn rerun_command_targets_selected_else_newest() {
        // AC-HP (Change 4): rerun resolves the command line to re-send - the
        // selected block's, else the newest.
        let mut pane = three_blocks();
        assert_eq!(pane.rerun_command().as_deref(), Some("echo three"));
        pane.block_select(BlockDir::Prev); // newest (2)
        pane.block_select(BlockDir::Prev); // block 1
        assert_eq!(pane.rerun_command().as_deref(), Some("echo two"));
    }

    #[test]
    fn rerun_command_heals_a_selected_block_evicted_since_selection() {
        // Select a block, then evict it under a tiny retained budget: rerun must
        // fall back to the newest command, not report "nothing to rerun" on the
        // stale ref.
        let mut pane = Pane::with_limits(4, 40, 10_000, 8, 16);
        run_named(&mut pane, "first", "aaa", 0);
        pane.block_select(BlockDir::Prev); // selects block 0 (the only one)
        assert_eq!(pane.selected_block, Some(0));
        // Push enough blocks to front-drop block 0 from the retained window.
        for i in 1..6 {
            run_named(&mut pane, &format!("cmd{i}"), "outputoutput\ncc\ndd", 0);
        }
        assert!(
            pane.read_block(BlockSel::Seq(0)).is_err(),
            "block 0 evicted precondition"
        );
        // The stale selected_block=0 no longer resolves; heal to the newest.
        assert_eq!(pane.rerun_command().as_deref(), Some("cmd5"));
    }

    #[test]
    fn search_releases_the_block_selection_for_rerun() {
        // Search and block-select share term.selection, so a search must not leave
        // a stale selected_block as the rerun target (incl. a co-viewer's rerun).
        let mut pane = three_blocks();
        pane.block_select(BlockDir::Prev); // newest (block 2)
        pane.block_select(BlockDir::Prev); // block 1
        assert_eq!(pane.rerun_command().as_deref(), Some("echo two"));
        // A matching search takes over the shared selection (apply_current_match):
        // rerun falls back to the newest block, not the now-invisible block 1.
        let (total, _) = pane.search_open("echo");
        assert!(total > 0, "precondition: 'echo' matches in scrollback");
        assert_eq!(
            pane.selected_block, None,
            "search released the block target"
        );
        assert_eq!(pane.rerun_command().as_deref(), Some("echo three"));
        // The no-match path (which drops the highlight via search_clear) also
        // releases the block target.
        pane.block_select(BlockDir::Prev);
        pane.block_select(BlockDir::Prev);
        assert_eq!(pane.rerun_command().as_deref(), Some("echo two"));
        assert_eq!(pane.search_open("zz-no-such-token"), (0, 0));
        assert_eq!(pane.selected_block, None);
        assert_eq!(pane.rerun_command().as_deref(), Some("echo three"));
    }

    #[test]
    fn is_pristine_idle_shell_gates_takeover() {
        // (x-fbb1) The `.`=here take-over reap gate. Only a shell that has drawn a prompt and run
        // nothing is safe to reap.
        // No markers yet (un-integrated / not-yet-prompted): not trustworthy -> refuse.
        let mut pane = Pane::new(6, 40);
        assert!(!pane.is_pristine_idle_shell(), "no OSC 133 markers seen");
        // Drew a prompt, ran nothing: the empty tab -> pristine idle. A bare `D` (first precmd,
        // no open block) creates no block, so `blocks` stays empty.
        pane.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07");
        assert!(pane.is_pristine_idle_shell(), "prompt drawn, nothing run");
        // A command is running now (open `C`, no `D`) - also the `exec nvim` case: refuse.
        let mut running = Pane::new(6, 40);
        running.feed(b"\x1b]133;A\x07\x1b]133;C\x07");
        assert!(!running.is_pristine_idle_shell(), "a command is running");
        // Ran a command and returned to a prompt (a completed block, e.g. a backgrounded or
        // stopped job left one): refuse - not pristine.
        let mut ran = Pane::new(6, 40);
        ran.feed(b"\x1b]133;C\x07out\x1b]133;D;0\x07\x1b]133;A\x07");
        assert!(!ran.is_pristine_idle_shell(), "a command has already run");
    }

    #[test]
    fn rerun_command_is_none_for_a_bare_c_block() {
        // A block from a bare-`C` emitter (no `B`, no captured command) has
        // nothing to rerun - None, so the server refuses rather than sending garbage.
        let mut pane = Pane::new(6, 40);
        pane.feed(b"$ \x1b]133;C\x07output\x1b]133;D;0\x07");
        assert!(pane.read_block(BlockSel::Last).is_ok());
        assert_eq!(pane.rerun_command(), None);
    }

    // -- Turn blocks (hook-emitted markers) -------------------------------------

    /// Feed one agent TURN the way the inside-leg hook emits it: a bare `C` at
    /// turn start (no A/B, no command echo), styled TUI-ish output, `D;0` at
    /// Stop. The scanner must treat these exactly like shell-emitted markers.
    fn run_turn(pane: &mut Pane, output: &str) {
        pane.feed(b"\x1b]133;C\x07");
        for line in output.lines() {
            pane.feed(format!("\x1b[1m{line}\x1b[0m\r\n").as_bytes());
        }
        pane.feed(b"\x1b]133;D;0\x07");
    }

    #[test]
    fn turn_markers_segment_pane_history_and_navigate() {
        // Hook-shaped markers segment a claude pane by turns: each block's span
        // is exactly one turn's output, idle repaints between turns land outside
        // every block, and BlockJump prev from the live tail anchors at the
        // newest turn's first row.
        let mut pane = Pane::new(4, 40);
        pane.feed(b"claude banner\r\n"); // pre-turn TUI noise: outside any block
        run_turn(&mut pane, "t1a\nt1b\nt1c");
        pane.feed(b"idle repaint\r\n"); // between turns: no open block
        run_turn(&mut pane, "t2a\nt2b\nt2c");
        run_turn(&mut pane, "t3a\nt3b\nt3c");

        // Pure turn output, ANSI-stripped, no command (no B echo -> cmd None).
        let b0 = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert_eq!(
            (b0.text.as_str(), b0.complete, b0.exit),
            ("t1a\r\nt1b\r\nt1c\r\n", true, Some(0))
        );
        let b1 = pane.read_block(BlockSel::Seq(1)).unwrap();
        assert_eq!(
            b1.text, "t2a\r\nt2b\r\nt2c\r\n",
            "idle repaint stays outside the turn"
        );
        assert_eq!(
            pane.rerun_command(),
            None,
            "a turn has no rerun-able command"
        );

        // BlockJump prev from the live tail of a 3-turn pane lands on turn 3,
        // anchored at its first row (AC happy).
        match pane.block_jump(BlockDir::Prev) {
            BlockJumpOutcome::Moved { seq, abs_row } => {
                assert_eq!(seq, 2);
                assert_eq!(
                    pane.viewport_top_abs(),
                    abs_row,
                    "turn anchored at viewport top"
                );
            }
            other => panic!("expected Moved, got {other:?}"),
        }

        // The copy chain extracts a full turn (prefix+y path).
        pane.selection_clear();
        assert_eq!(pane.block_select(BlockDir::Prev), Some(2));
        let sel = pane.selection_text().unwrap_or_default();
        assert!(
            sel.contains("t3a") && sel.contains("t3c"),
            "selection spans the turn: {sel:?}"
        );
    }

    #[test]
    fn shell_and_turn_blocks_navigate_the_union_in_stream_order() {
        // AC edge: a pane mixing SHELL blocks (auto-injected zsh) and TURN blocks
        // (claude launched from that shell) is one stream-ordered sequence - no
        // special-casing anywhere.
        let mut pane = Pane::new(4, 40);
        run_named(&mut pane, "ls", "shell-a\nshell-b\nshell-c", 0);
        run_turn(&mut pane, "turn-a\nturn-b\nturn-c");
        // Post-turn shell activity pushes both anchors into history so jumps scroll.
        pane.feed(b"$ back-at-shell\r\n\r\n");

        let shell = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert_eq!(shell.text, "shell-a\r\nshell-b\r\nshell-c\r\n");
        let turn = pane.read_block(BlockSel::Seq(1)).unwrap();
        assert_eq!(turn.text, "turn-a\r\nturn-b\r\nturn-c\r\n");

        // Navigation walks newest (turn) then older (shell), one union.
        assert!(matches!(
            pane.block_jump(BlockDir::Prev),
            BlockJumpOutcome::Moved { seq: 1, .. }
        ));
        assert!(matches!(
            pane.block_jump(BlockDir::Prev),
            BlockJumpOutcome::Moved { seq: 0, .. }
        ));
        // Selection walks the same union; the shell block keeps its rerun-able
        // command (the turn block, selected first, has none).
        assert_eq!(pane.block_select(BlockDir::Prev), Some(1));
        assert_eq!(pane.rerun_command(), None, "a selected turn has no command");
        assert_eq!(pane.block_select(BlockDir::Prev), Some(0));
        assert_eq!(pane.rerun_command().as_deref(), Some("ls"));
    }

    #[test]
    fn unbalanced_turn_markers_degrade_never_corrupt() {
        // The hook's known v1 limits all reduce to unbalanced marker streams;
        // the store must CONTAIN them. A C while a turn is open (nested claude,
        // or an interrupted turn's next prompt) finalizes the prior block with
        // unknown exit; a D with no open block (a blocked-Stop's later legs)
        // is a no-op, not a phantom block.
        let mut pane = Pane::new(6, 40);
        pane.feed(b"\x1b]133;C\x07outer-turn\r\n");
        pane.feed(b"\x1b]133;C\x07inner-spray\r\n"); // C-while-open
        pane.feed(b"\x1b]133;D;0\x07");

        let outer = pane.read_block(BlockSel::Seq(0)).unwrap();
        assert_eq!(outer.text, "outer-turn\r\n");
        assert_eq!(outer.exit, None, "early-finalized turn has unknown exit");
        assert!(outer.complete);
        let inner = pane.read_block(BlockSel::Seq(1)).unwrap();
        assert_eq!(
            (inner.text.as_str(), inner.exit),
            ("inner-spray\r\n", Some(0))
        );

        // Orphan D;0s (loop continuations re-attempting Stop): no open block,
        // no new block, no panic.
        pane.feed(b"loop continuation output\r\n\x1b]133;D;0\x07\x1b]133;D;0\x07");
        assert!(
            pane.read_block(BlockSel::Seq(2)).is_err(),
            "no phantom block"
        );
        assert_eq!(pane.read_block(BlockSel::Last).unwrap().seq, Some(1));
    }

    // -- In-scrollback search (x-e780, v12) ------------------------------------

    #[test]
    fn search_jumps_scrolls_and_highlights_case_insensitively() {
        // AC1-HP + AC3-EDGE: the match is found case-insensitively and as a
        // substring, the view scrolls back into history to reach it, and the
        // highlight spans exactly the matched columns on the single row.
        let mut pane = Pane::new(3, 40);
        for i in 0..20 {
            pane.feed(format!("filler {i}\r\n").as_bytes());
        }
        pane.feed(b"PostgreSQL Deadlock detected\r\n");
        for i in 0..5 {
            pane.feed(format!("more {i}\r\n").as_bytes());
        }
        let (total, current) = pane.search_open("deadlock");
        assert_eq!((total, current), (1, 1), "one match, positioned first");
        assert!(pane.display_offset() > 0, "scrolled up into history");
        assert!(pane.has_selection());
        assert_eq!(
            pane.selection_text().unwrap_or_default(),
            "Deadlock",
            "highlight spans exactly the 8 matched columns"
        );
    }

    #[test]
    fn search_no_match_is_zero_and_does_not_move() {
        // AC1-ERR: a query with no occurrence returns total 0, sets no selection,
        // and leaves the viewport where it was (a visible signal, never a jump).
        let mut pane = Pane::new(4, 40);
        for i in 0..10 {
            pane.feed(format!("line {i}\r\n").as_bytes());
        }
        let before = pane.display_offset();
        assert_eq!(pane.search_open("zzznope"), (0, 0));
        assert!(!pane.has_selection());
        assert!(!pane.has_search());
        assert_eq!(pane.display_offset(), before, "viewport did not move");
    }

    #[test]
    fn search_step_walks_and_rests_at_both_ends() {
        // AC2-HP + AC2-EDGE: n/N walk the matches and rest (do not wrap) at the
        // oldest and newest, with the counter tracking the position. 12 rows each
        // carry "error" - a match on the live tail and the oldest history row both
        // count (Boundaries: no off-by-one at the ends).
        let mut pane = Pane::new(3, 40);
        for i in 0..12 {
            pane.feed(format!("error {i}\r\n").as_bytes());
        }
        let (total, _) = pane.search_open("error");
        assert_eq!(total, 12);

        // Walk to the oldest; Prev rests at 1.
        let mut c = 0;
        for _ in 0..20 {
            (_, c) = pane.search_step(BlockDir::Prev).unwrap();
        }
        assert_eq!(c, 1, "Prev rests on the oldest match");

        // A single Next moves one newer.
        let (_, c2) = pane.search_step(BlockDir::Next).unwrap();
        assert_eq!(c2, 2);

        // Walk to the newest; Next rests at 12.
        for _ in 0..20 {
            (_, c) = pane.search_step(BlockDir::Next).unwrap();
        }
        assert_eq!(c, 12, "Next rests on the newest match");
    }

    #[test]
    fn search_empty_query_clears() {
        // Boundaries: an empty query is a clear, not a scan that matches every row.
        let mut pane = Pane::new(4, 40);
        pane.feed(b"hello world\r\n");
        pane.search_open("hello");
        assert!(pane.has_selection());
        assert_eq!(pane.search_open(""), (0, 0));
        assert!(!pane.has_selection(), "empty query drops the highlight");
        assert!(!pane.has_search());
    }

    #[test]
    fn search_step_without_active_search_is_none() {
        // Errors: a step on a pane with no active search is a clean None (the
        // server maps it to a no-op Notice), never a panic.
        let mut pane = Pane::new(4, 40);
        pane.feed(b"nothing to find here\r\n");
        assert_eq!(pane.search_step(BlockDir::Next), None);
        assert_eq!(pane.search_step(BlockDir::Prev), None);
        assert!(!pane.has_search());
    }

    #[test]
    fn resize_clears_active_search() {
        // AC3-FR: reflow stales the stored anchors, so a resize drops the search
        // snapshot and its highlight; a later step no-ops rather than mis-jumping.
        let mut pane = Pane::new(3, 20);
        for i in 0..8 {
            pane.feed(format!("match{i}\r\n").as_bytes());
        }
        pane.search_open("match");
        assert!(pane.has_search());
        pane.resize(3, 10);
        assert!(!pane.has_search(), "resize drops the search snapshot");
        assert!(!pane.has_selection());
        assert_eq!(pane.search_step(BlockDir::Next), None);
    }

    #[test]
    fn search_step_re_highlights_the_right_row_after_saturation() {
        // x-1e67 (AC edge): saturate a tiny scrollback so history is already at
        // cap, open a search, then stream output that evicts lines the match
        // survives. A re-highlight (search_step) must land on the real match, not
        // a row `E` away - the frozen-snapshot `abs_row` drifted by the eviction
        // count and highlighted a filler row before the re-scan fix.
        let mut pane = Pane::with_scrollback(3, 40, 8);
        for i in 0..40 {
            pane.feed(format!("filler{i}\r\n").as_bytes());
        }
        pane.feed(b"UNIQTOKEN here\r\n");
        for i in 0..2 {
            pane.feed(format!("tail{i}\r\n").as_bytes());
        }
        let (total, _) = pane.search_open("UNIQTOKEN");
        assert_eq!(total, 1, "exactly one match");
        assert_eq!(pane.selection_text().as_deref(), Some("UNIQTOKEN"));
        // Evict 3 lines; the match is still within the retained window.
        for i in 0..3 {
            pane.feed(format!("more{i}\r\n").as_bytes());
        }
        pane.search_step(BlockDir::Prev); // single match -> re-applies the same
        assert_eq!(
            pane.selection_text().as_deref(),
            Some("UNIQTOKEN"),
            "highlight must track the real match, not drift to a filler row"
        );
    }

    #[test]
    fn search_step_re_anchors_when_older_matches_age_out() {
        // x-1e67 (gemini/codex review): stepping must move relative to where the
        // current match IS, not a stale ordinal. On match B in [A,B,C,D], when A
        // ages out the fresh list is [B,C,D]; `Next` must land on C (2/3), not
        // skip to D (3/3) by reusing the old index 1.
        let mut pane = Pane::with_scrollback(3, 40, 8);
        for i in 0..30 {
            pane.feed(format!("filler{i}\r\n").as_bytes());
        }
        // Newest-last: A, sep, B, sep, C, sep, D. A is the 7th-newest line.
        for m in ["MARK A", "sep", "MARK B", "sep", "MARK C", "sep", "MARK D"] {
            pane.feed(format!("{m}\r\n").as_bytes());
        }
        assert_eq!(pane.search_open("MARK").0, 4, "four matches");
        // Land deterministically on B (index 1): walk to the oldest, then one Next.
        for _ in 0..8 {
            pane.search_step(BlockDir::Prev);
        }
        assert_eq!(pane.search_step(BlockDir::Next), Some((4, 2)), "on B");
        // Evict exactly A (7th-newest -> past the 11-line window); B,C,D stay.
        for i in 0..5 {
            pane.feed(format!("more{i}\r\n").as_bytes());
        }
        assert_eq!(
            pane.search_step(BlockDir::Next),
            Some((3, 2)),
            "re-anchored to B, stepped to C - not skipped to D (3/3)"
        );
    }

    #[test]
    fn search_step_drops_search_when_all_matches_age_out() {
        // Boundary: if every match scrolls out of retained scrollback between
        // steps, the re-scan finds nothing. The query now matches nothing, so the
        // step drops the highlight and reports the zero-match reply (0, 0) - the
        // same signal as a no-match open - rather than walking stale anchors or
        // returning None (which the server would not broadcast, leaving a stale
        // highlight). `None` stays reserved for "no active search".
        let mut pane = Pane::with_scrollback(3, 20, 6);
        pane.feed(b"NEEDLE row\r\n");
        assert_eq!(pane.search_open("NEEDLE"), (1, 1));
        assert!(pane.has_search());
        for i in 0..30 {
            pane.feed(format!("flood{i}\r\n").as_bytes());
        }
        assert_eq!(
            pane.search_step(BlockDir::Prev),
            Some((0, 0)),
            "aged-out matches report zero, not None"
        );
        assert!(!pane.has_search(), "dead search dropped");
        assert!(!pane.has_selection(), "highlight cleared for repaint");
        // A further step now has no active search: that is the real None case.
        assert_eq!(pane.search_step(BlockDir::Prev), None);
    }
}
