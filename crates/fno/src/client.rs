//! The mux client: chrome + a dumb compositor over the server's pane frames.
//!
//! Takes over the terminal (crossterm raw mode + alternate screen), attaches
//! to the session server (spawning one if absent), and renders three things:
//! a top tab bar, a left sideline (squads with caret dropdowns), and the
//! content area where per-pane `Frame`s are blitted into the rects the last
//! `Layout` assigned. The client never runs the layout algorithm and never
//! emulates VT (Locked Decision 3): rects and grids both come from the
//! server, which is what makes reattach exact.
//!
//! Input goes through the leader-key scanner (`keys.rs`): bare bytes forward
//! verbatim on the reliable channel (AC2-UI), chords become `Command`s.
//! Caret expansion, sideline visibility, and the selector are CLIENT-LOCAL
//! view state - never on the wire (Locked Decision 15).
//!
//! Every error surface while the compositor owns the terminal goes through
//! the rendered UI (tab-bar notice + BEL), never stderr (x-0175 pitfall).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::style::Color as CtColor;
use crossterm::{cursor, queue, style, terminal};
use tokio::sync::mpsc;

use crate::keys::{Event, Scanner};
use crate::proto::{
    self, cell_flags, read_msg, write_msg, AgentBadge, AgentRow, Cell, ClientMsg, Color, Command,
    Frame, MouseEvent, ProtoError, ServerMsg, SquadMeta, BUILD_VERSION, PROTO_VERSION,
};
use crate::tree::Rect;

/// How long to wait for a just-spawned server to accept.
const SPAWN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Sideline width in columns, divider column included. Client-local chrome:
/// the server sees only the content-area viewport.
const PANEL_W: u16 = 28;
/// Below this many content columns the sideline auto-hides (AC6-EDGE).
const MIN_CONTENT_COLS: u16 = 40;
/// The tab bar row.
const TAB_BAR_ROWS: u16 = 1;
/// The status row (US4): one always-on bottom line of client-local chrome.
const STATUS_ROWS: u16 = 1;
/// Below this many terminal rows the bottom chrome (status row + which-key
/// hint) auto-hides and the content area recovers the line (AC4-ERR).
const MIN_ROWS_FOR_STATUS: u16 = TAB_BAR_ROWS + STATUS_ROWS + 5;
/// How long a pending leader chord waits before the which-key hint paints
/// (US4, AC4-HP). `leader+?` shows the full table instantly instead.
const HINT_DELAY: Duration = Duration::from_millis(400);
/// Transient notice lifetime on the tab bar.
const NOTICE_TTL: Duration = Duration::from_secs(3);

/// Run the client for `session`. Returns the process exit code.
pub fn run(session: &str) -> i32 {
    match run_inner(session) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fno: {e}");
            1
        }
    }
}

fn run_inner(session: &str) -> Result<i32, String> {
    // Nested same-session guard (AC3-UI/EDGE): BEFORE any socket, spawn, or
    // terminal mode change. `FNO_SESSION` is set in every pane the server
    // spawns, so target == env means "attaching to the session I am already
    // inside" - an instant hall of mirrors. Different-session nesting is
    // allowed (the flag already beat the env in resolution).
    if std::env::var("FNO_SESSION").ok().as_deref() == Some(session) {
        return Err(format!(
            "already inside mux session {session:?} (FNO_SESSION is set). \
             Attach to another session with `fno --session <other>`, or \
             `unset FNO_SESSION` if this shell is not really inside a pane."
        ));
    }
    proto::ensure_mux_dir().map_err(|e| format!("cannot prepare the mux dir: {e}"))?;
    let path = proto::socket_path(session)?;
    let stream = connect_or_spawn(&path)?;

    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    runtime.block_on(attach_and_run(stream, &path))
}

/// Connect to a live server, or spawn one and connect. AC3-ERR: a dead
/// server's stale socket gets a one-line notice and a fresh server - never a
/// hang on a dead socket (the spawned server's bind unlinks it). Shared with
/// `mux_cli::pane run`, which must self-spawn a server for a script-only
/// session (AC1-EDGE).
pub(crate) fn connect_or_spawn(path: &Path) -> Result<std::os::unix::net::UnixStream, String> {
    if let Ok(s) = std::os::unix::net::UnixStream::connect(path) {
        return Ok(s);
    }
    if path.exists() {
        eprintln!("fno: previous session ended; starting a fresh one");
    }
    spawn_server(path)?;
    let deadline = Instant::now() + SPAWN_CONNECT_TIMEOUT;
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "server did not come up at {} ({e}); check {}",
                    path.display(),
                    log_path(path).display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(30)),
        }
    }
}

fn log_path(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}

/// Spawn `fno --server <socket>` detached: its own session (setsid) so the
/// server never receives the terminal's SIGHUP, stderr to a per-session log.
/// Two clients racing here both spawn; the bind is the lock, the losing
/// server exits 0, and both clients attach to the winner (AC4-EDGE).
fn spawn_server(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(path))
        .map_err(|e| format!("cannot open server log: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--server")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // Safety: setsid only detaches the child from our session/terminal; it is
    // async-signal-safe and touches no shared state.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot spawn the mux server: {e}"))
}

/// Restore the terminal on every exit path, including panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
        let mut out = std::io::stdout();
        // Surface an alt-screen failure instead of silently painting over the
        // user's scrollback. The guard exists from here, so raw mode is
        // restored by Drop on the error path.
        let guard = TerminalGuard;
        crossterm::execute!(out, terminal::EnterAlternateScreen)
            .map_err(|e| format!("alternate screen: {e}"))?;
        // Mouse capture stays on for the client's whole life (US1/US2/US3): the
        // server routes every pane-rect event by the pane's live mode. Drop's
        // MODE_RESET (which lists 1000/1002/1006 off) turns it back off on exit.
        out.write_all(crate::mouse::ENABLE)
            .and_then(|_| out.flush())
            .map_err(|e| format!("enable mouse: {e}"))?;
        Ok(guard)
    }
}

/// Every DEC/private mode `ModeSync` can set, reset. Emitted unconditionally
/// on exit (codex P2): a focused vim's mouse reporting or bracketed paste
/// must never survive onto the user's real terminal after `fno` exits, and
/// tracking exactly-what-was-set buys nothing over resetting the fixed set
/// `vt::mode_diff` can emit. Unknown sequences (kitty CSI-u on a plain
/// terminal) are ignored by terminals by design.
const MODE_RESET: &[u8] =
    b"\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1007l\x1b[?2004l\x1b[=0;1u";

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(MODE_RESET);
        let _ = crossterm::execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// View state + pure composition
// ---------------------------------------------------------------------------

/// The last `Layout` as the client holds it.
struct LayoutView {
    squads: Vec<SquadMeta>,
    active_squad: u64,
    panes: Vec<(u64, Rect)>,
    focus: u64,
    /// The clamped content-area the rects were computed for; a client whose
    /// own content area is larger letterboxes (3.5).
    area: (u16, u16),
    /// Sideline agent rows (4a-G2): registry-derived, fact-badged, rendered
    /// under their squads (display-only; never selectable).
    agents: Vec<AgentRow>,
}

/// One selectable sideline row: a squad, or one of its tabs when expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelRow {
    squad: u64,
    tab: Option<usize>,
}

/// Everything the client renders from. Pure state - `compose` turns it into
/// one full-terminal `Frame` the row-diffing `Compositor` draws.
struct View {
    term: (u16, u16), // full terminal (rows, cols)
    /// The session name, for the status row. Fixed for the connection's life
    /// (sessions cannot rename), so the row can never go stale.
    session: String,
    layout: LayoutView,
    frames: HashMap<u64, Frame>,
    /// Manual sideline toggle; narrow terminals override it (auto-hide).
    panel_on: bool,
    /// Manual status-row toggle (leader+s). Client-local and deliberately
    /// unpersisted: a reattach resets to on (AC4-FR).
    status_on: bool,
    /// The which-key hint line is painted over the bottom row (leader held
    /// past [`HINT_DELAY`]); any chord resolution clears it (AC4-HP).
    hint: bool,
    /// The full key-table overlay (leader+?); the next keypress dismisses it
    /// (AC4-EDGE).
    overlay: bool,
    /// Caret expansion per squad id - client-local, instant (AC6-UI).
    expanded: HashSet<u64>,
    /// Selector cursor into [`View::sel_rows`], when open.
    selector: Option<usize>,
    /// Pending escape bytes in selector mode, carried ACROSS reads so a
    /// split arrow sequence can never half-close the selector and leak its
    /// tail into the pane (gemini medium).
    sel_esc: Vec<u8>,
    notice: Option<(String, Instant)>,
}

impl View {
    fn new(term: (u16, u16), session: String, layout: LayoutView) -> Self {
        View {
            term,
            session,
            layout,
            frames: HashMap::new(),
            panel_on: true,
            status_on: true,
            hint: false,
            overlay: false,
            expanded: HashSet::new(),
            selector: None,
            sel_esc: Vec::new(),
            notice: None,
        }
    }

    fn panel_visible(&self) -> bool {
        self.panel_on && self.term.1 >= PANEL_W + MIN_CONTENT_COLS
    }

    fn panel_w(&self) -> u16 {
        if self.panel_visible() {
            PANEL_W
        } else {
            0
        }
    }

    /// Whether the bottom row belongs to chrome. Geometry beats the toggle:
    /// a too-short terminal recovers the line for content (AC4-ERR).
    fn status_visible(&self) -> bool {
        self.status_on && self.term.0 >= MIN_ROWS_FOR_STATUS
    }

    fn status_rows(&self) -> u16 {
        if self.status_visible() {
            STATUS_ROWS
        } else {
            0
        }
    }

    /// The CONTENT-AREA viewport reported to the server (terminal minus
    /// chrome). Never zero, so a degenerate terminal cannot wedge the server.
    fn content_dims(&self) -> (u16, u16) {
        (
            self.term
                .0
                .saturating_sub(TAB_BAR_ROWS + self.status_rows())
                .max(1),
            self.term.1.saturating_sub(self.panel_w()).max(1),
        )
    }

    /// Map an outer-terminal cell (0-based) to `(pane, pane_row, pane_col)` when
    /// it falls inside a pane's content rect. `None` for a chrome cell (tab bar,
    /// sideline) or a content divider, so the caller swallows it - a mouse event
    /// on chrome never forwards to a pane (AC3-UI). Rects are content-area
    /// relative; the content origin is `(TAB_BAR_ROWS, panel_w)`.
    fn hit_test(&self, row: u16, col: u16) -> Option<(u64, u16, u16)> {
        let panel_w = self.panel_w();
        if row < TAB_BAR_ROWS || col < panel_w {
            return None;
        }
        let cr = row - TAB_BAR_ROWS;
        let cc = col - panel_w;
        for (pid, rect) in &self.layout.panes {
            if cr >= rect.y && cr < rect.y + rect.rows && cc >= rect.x && cc < rect.x + rect.cols {
                return Some((*pid, cr - rect.y, cc - rect.x));
            }
        }
        None
    }

    fn set_layout(&mut self, layout: LayoutView) {
        // Frames for panes unknown to the new Layout are dead - drop them
        // (Concurrency: a frame is only ever drawn against the Layout
        // generation it belongs to).
        let live: HashSet<u64> = layout.panes.iter().map(|(id, _)| *id).collect();
        self.frames.retain(|id, _| live.contains(id));
        self.layout = layout;
        // Selector re-anchors to a live row on catalog change (AC6-FR).
        if let Some(cur) = self.selector {
            let n = self.sel_rows().len();
            self.selector = if n == 0 { None } else { Some(cur.min(n - 1)) };
        }
    }

    fn set_notice(&mut self, text: String) {
        self.notice = Some((text, Instant::now() + NOTICE_TTL));
    }

    /// The sideline rows in display order: each squad, then its tabs when
    /// expanded. The ids are (namespace, key)-style typed pairs from day one
    /// (x-fef5 lesson): a squad row and a tab row can never collide.
    fn sel_rows(&self) -> Vec<SelRow> {
        let mut rows = Vec::new();
        for s in &self.layout.squads {
            rows.push(SelRow {
                squad: s.id,
                tab: None,
            });
            if self.expanded.contains(&s.id) {
                for t in 0..s.tabs.len() {
                    rows.push(SelRow {
                        squad: s.id,
                        tab: Some(t),
                    });
                }
            }
        }
        rows
    }

    /// Compose the full-terminal frame: tab bar, sideline, dividers, panes.
    /// Pure - all the drawing machinery (row diff, styles, wide-spacer
    /// handling) stays in [`Compositor`].
    fn compose(&self) -> Frame {
        let (rows, cols) = self.term;
        let (rows, cols) = (rows.max(1) as usize, cols.max(1) as usize);
        let mut cells = vec![Cell::default(); rows * cols];
        let panel_w = self.panel_w() as usize;

        self.draw_tab_bar(&mut cells, cols);
        if panel_w > 0 {
            self.draw_sideline(&mut cells, rows, cols, panel_w);
        }

        // Content area: dividers first (uncovered cells), panes blitted over.
        let origin_r = TAB_BAR_ROWS as usize;
        let origin_c = panel_w;
        let mut covered = vec![false; rows * cols];
        for (pid, rect) in &self.layout.panes {
            let frame = self.frames.get(pid);
            for fr in 0..rect.rows as usize {
                let r = origin_r + rect.y as usize + fr;
                if r >= rows {
                    break;
                }
                for fc in 0..rect.cols as usize {
                    let c = origin_c + rect.x as usize + fc;
                    if c >= cols {
                        break;
                    }
                    covered[r * cols + c] = true;
                    if let Some(f) = frame {
                        if fr < f.rows as usize && fc < f.cols as usize {
                            cells[r * cols + c] = f.cells[fr * f.cols as usize + fc];
                        }
                    }
                }
            }
        }
        // Scroll indicator (US1, AC1-UI): a minimal `[+N]` at a scrolled pane's
        // top-right, inverse-video so it reads over content. Present iff the
        // pane's frame reports a non-zero offset (group 2's status row becomes
        // its canonical home). A pane too narrow to fit the label skips it.
        for (pid, rect) in &self.layout.panes {
            let Some(f) = self.frames.get(pid) else {
                continue;
            };
            if f.scroll_offset == 0 {
                continue;
            }
            let label = format!("[+{}]", f.scroll_offset);
            let w = label.chars().count();
            let r = origin_r + rect.y as usize;
            if (rect.cols as usize) < w || r >= rows {
                continue;
            }
            let start_c = origin_c + rect.x as usize + rect.cols as usize - w;
            for (k, ch) in label.chars().enumerate() {
                let c = start_c + k;
                if c < cols {
                    cells[r * cols + c] = Cell {
                        c: ch,
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::INVERSE,
                    };
                }
            }
        }
        // Letterbox (AC1-UI): the server tiled its rects into `Layout.area`
        // (the view-scoped clamp); content anchors top-left and everything
        // beyond `area` up to the local content edge is visibly-inert dim
        // filler, never divider glyphs. `(0, 0)` is the pre-Layout
        // placeholder: no filler until the first real Layout names a bound.
        let (a_rows, a_cols) = self.layout.area;
        let boxed = self.layout.area != (0, 0);
        // Divider glyphs for in-area content cells no pane covers: pick by
        // which neighbors are panes so vertical strips read '│', horizontal
        // '─', crossings '┼'. Dim so chrome never shouts over content.
        for r in origin_r..rows {
            for c in origin_c..cols {
                if covered[r * cols + c] {
                    continue;
                }
                if boxed && (r - origin_r >= a_rows as usize || c - origin_c >= a_cols as usize) {
                    cells[r * cols + c] = Cell {
                        c: '·',
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::DIM,
                    };
                    continue;
                }
                let horiz = c > origin_c && covered[r * cols + c - 1]
                    || c + 1 < cols && covered[r * cols + c + 1];
                let vert = r > origin_r && covered[(r - 1) * cols + c]
                    || r + 1 < rows && covered[(r + 1) * cols + c];
                cells[r * cols + c] = Cell {
                    c: match (horiz, vert) {
                        (true, true) => '┼',
                        (true, false) => '│',
                        (false, true) => '─',
                        (false, false) => ' ',
                    },
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: cell_flags::DIM,
                };
            }
        }

        self.draw_bottom_row(&mut cells, rows, cols);
        if self.overlay {
            draw_overlay(&mut cells, rows, cols);
        }

        // Terminal cursor: the FOCUSED pane's, offset into its rect - the
        // one place the cursor may sit (AC1-UI/AC5-UI).
        let (mut cur_r, mut cur_c, mut cur_vis) = (0u16, 0u16, false);
        if self.selector.is_none() {
            if let Some((_, rect)) = self
                .layout
                .panes
                .iter()
                .find(|(id, _)| *id == self.layout.focus)
            {
                if let Some(f) = self.frames.get(&self.layout.focus) {
                    cur_r = TAB_BAR_ROWS + rect.y + f.cursor_row.min(rect.rows.saturating_sub(1));
                    cur_c = self.panel_w() + rect.x + f.cursor_col.min(rect.cols.saturating_sub(1));
                    if boxed {
                        // Never in the filler (AC1-UI), even mid-race when a
                        // stale rect exceeds the just-shrunk area.
                        cur_r = cur_r.min(TAB_BAR_ROWS + a_rows.saturating_sub(1));
                        cur_c = cur_c.min(self.panel_w() + a_cols.saturating_sub(1));
                    }
                    cur_vis = f.cursor_visible;
                }
            }
        }
        Frame {
            rows: rows as u16,
            cols: cols as u16,
            cells,
            cursor_row: cur_r,
            cursor_col: cur_c,
            cursor_visible: cur_vis,
            // The composed full-terminal frame is not itself scrolled; the
            // per-pane indicator is drawn INTO the cells above from each pane
            // frame's own scroll_offset.
            scroll_offset: 0,
        }
    }

    /// The bottom chrome line (US4). While a leader chord is pending past
    /// [`HINT_DELAY`] it is the which-key hint (painted over whatever the row
    /// held - even with the status row toggled off, discoverability does not
    /// die with the toggle; tmux's message-line behavior). Otherwise it is
    /// the status row (AC4-UI): session name, focused pane cwd, the focused
    /// pane's scroll offset (the canonical `[+N]` home; the per-pane inline
    /// indicator stays so a scrolled UNFOCUSED pane is still observable),
    /// and `? for keys`. Too-short terminals draw neither (AC4-ERR).
    fn draw_bottom_row(&self, cells: &mut [Cell], rows: usize, cols: usize) {
        if self.term.0 < MIN_ROWS_FOR_STATUS {
            return;
        }
        let r = rows - 1;
        let put = |cells: &mut [Cell], c: usize, ch: char, flags: u8| {
            if c < cols {
                cells[r * cols + c] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags,
                };
            }
        };
        if self.hint {
            let text = " % \" split · hjkl focus · HJKL resize · x close · c tab \
                        · n/p cycle · 1-9 tab · & close-tab · w select · b sideline \
                        · s status · d detach · ? all keys";
            for (i, ch) in text.chars().take(cols).enumerate() {
                put(cells, i, ch, 0);
            }
            return;
        }
        if !self.status_visible() {
            return;
        }
        let mut c = 0usize;
        for ch in format!(" {} ", self.session).chars() {
            put(cells, c, ch, cell_flags::BOLD);
            c += 1;
        }
        let cwd = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .map(|s| abbrev_home(&s.canonical_cwd))
            .unwrap_or_default();
        for ch in format!("│ {cwd} ").chars() {
            put(cells, c, ch, cell_flags::DIM);
            c += 1;
        }
        if let Some(f) = self.frames.get(&self.layout.focus) {
            if f.scroll_offset != 0 {
                for ch in format!("[+{}] ", f.scroll_offset).chars() {
                    put(cells, c, ch, cell_flags::INVERSE);
                    c += 1;
                }
            }
        }
        let help = "? for keys ";
        let start = cols.saturating_sub(help.chars().count());
        if start > c {
            for (i, ch) in help.chars().enumerate() {
                put(cells, start + i, ch, cell_flags::DIM);
            }
        }
    }

    fn draw_tab_bar(&self, cells: &mut [Cell], cols: usize) {
        let active = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad);
        let mut spans: Vec<(String, u8)> = Vec::new(); // (text, flags)
        if let Some(s) = active {
            spans.push((format!(" {} ", s.name), cell_flags::BOLD));
            for (i, _) in s.tabs.iter().enumerate() {
                if i == s.active_tab {
                    spans.push((format!("[{}]", i + 1), cell_flags::INVERSE));
                } else {
                    spans.push((format!(" {} ", i + 1), 0));
                }
            }
        }
        let mut c = 0usize;
        for (text, flags) in spans {
            for ch in text.chars() {
                if c >= cols {
                    return;
                }
                cells[c] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags,
                };
                c += 1;
            }
        }
        // Transient notice, right-aligned, INVERSE (paired with the BEL the
        // event handler already sounded).
        if let Some((text, _)) = &self.notice {
            let text: String = text.chars().take(cols.saturating_sub(1)).collect();
            let start = cols.saturating_sub(text.chars().count() + 1);
            for (i, ch) in text.chars().enumerate() {
                let idx = start + i;
                if idx > c && idx < cols {
                    cells[idx] = Cell {
                        c: ch,
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::INVERSE,
                    };
                }
            }
        }
    }

    /// The sideline's display order (4a-G2): each squad's selectable rows
    /// (index-aligned with [`View::sel_rows`] so the selector highlight stays
    /// correct), that squad's agent rows, then a catch-all section for agents
    /// matched to no squad. Agent rows are display-only.
    fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        let mut out = Vec::new();
        let mut sel_idx = 0usize;
        for s in &self.layout.squads {
            out.push(DisplayRow::Sel {
                idx: sel_idx,
                row: SelRow {
                    squad: s.id,
                    tab: None,
                },
            });
            sel_idx += 1;
            if self.expanded.contains(&s.id) {
                for t in 0..s.tabs.len() {
                    out.push(DisplayRow::Sel {
                        idx: sel_idx,
                        row: SelRow {
                            squad: s.id,
                            tab: Some(t),
                        },
                    });
                    sel_idx += 1;
                }
            }
            out.extend(
                self.layout
                    .agents
                    .iter()
                    .filter(|a| a.squad == Some(s.id))
                    .map(DisplayRow::Agent),
            );
        }
        let orphans: Vec<&AgentRow> = self
            .layout
            .agents
            .iter()
            .filter(
                |a| !matches!(a.squad, Some(id) if self.layout.squads.iter().any(|s| s.id == id)),
            )
            .collect();
        if !orphans.is_empty() {
            out.push(DisplayRow::Header("~ agents"));
            out.extend(orphans.into_iter().map(DisplayRow::Agent));
        }
        out
    }

    fn draw_sideline(&self, cells: &mut [Cell], rows: usize, cols: usize, panel_w: usize) {
        let text_w = panel_w - 1; // last column is the divider
        for (i, drow) in self.display_rows().into_iter().enumerate() {
            let r = TAB_BAR_ROWS as usize + i;
            if r >= rows {
                break;
            }
            let (text, mut flags, selected) = match drow {
                DisplayRow::Sel { idx, row } => {
                    let squad = self.layout.squads.iter().find(|s| s.id == row.squad);
                    let Some(squad) = squad else { continue };
                    let is_active_squad = squad.id == self.layout.active_squad;
                    let (text, flags) = match row.tab {
                        None => {
                            let caret = if self.expanded.contains(&squad.id) {
                                '▾'
                            } else {
                                '▸'
                            };
                            (
                                format!("{caret} {}", squad.name),
                                if is_active_squad { cell_flags::BOLD } else { 0 },
                            )
                        }
                        Some(t) => {
                            let marker = if is_active_squad && t == squad.active_tab {
                                '*'
                            } else {
                                ' '
                            };
                            (format!("  {marker}{}", t + 1), 0)
                        }
                    };
                    (text, flags, self.selector == Some(idx))
                }
                DisplayRow::Agent(a) => {
                    // Fact-badge lattice glyphs (brief US2 state machine):
                    // exited beats badge beats liveness; a report reason
                    // rides inline while width allows.
                    let glyph = if a.exited {
                        '✗'
                    } else {
                        match a.badge {
                            Some(AgentBadge::Working) => '●',
                            Some(AgentBadge::Blocked) => '▲',
                            Some(AgentBadge::Done) => '✓',
                            None => '·',
                        }
                    };
                    let mut text = format!("  {glyph} {}", a.name);
                    if let Some(reason) = a.reason.as_deref().filter(|x| !x.is_empty()) {
                        text.push_str(": ");
                        text.push_str(reason);
                    }
                    let flags = if a.exited { cell_flags::DIM } else { 0 };
                    (text, flags, false)
                }
                DisplayRow::Header(h) => (h.to_string(), cell_flags::DIM, false),
            };
            if selected {
                flags |= cell_flags::INVERSE;
            }
            for (j, ch) in text.chars().take(text_w).enumerate() {
                cells[r * cols + j] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags,
                };
            }
            // Pad the highlight across the row so the cursor reads as a bar.
            if selected {
                for j in text.chars().count().min(text_w)..text_w {
                    cells[r * cols + j].flags |= cell_flags::INVERSE;
                }
            }
        }
        // The divider column, full height below the tab bar.
        for r in TAB_BAR_ROWS as usize..rows {
            cells[r * cols + (panel_w - 1)] = Cell {
                c: '│',
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::DIM,
            };
        }
    }
}

/// One rendered sideline line: a selectable squad/tab row (carrying its
/// [`View::sel_rows`] index so the selector highlight follows selection), a
/// display-only agent row, or the catch-all section header.
enum DisplayRow<'a> {
    Sel { idx: usize, row: SelRow },
    Agent(&'a AgentRow),
    Header(&'static str),
}

/// Abbreviate `$HOME` to `~` for the status row; only at a path-component
/// boundary so `/home/user2/...` never reads as `~2/...`.
fn abbrev_home(p: &str) -> String {
    abbrev_home_in(p, std::env::var("HOME").ok().as_deref())
}

fn abbrev_home_in(p: &str, home: Option<&str>) -> String {
    if let Some(h) = home.filter(|h| !h.is_empty()) {
        if let Some(rest) = p.strip_prefix(h) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    p.to_string()
}

/// The full key table (leader+?), inverse-video over the content area's
/// top-left. Any key dismisses it (AC4-EDGE). Cell-bounds-checked, so a tiny
/// terminal shows a clipped table rather than nothing.
const KEY_TABLE: &[&str] = &[
    " fno keys · leader = Ctrl-b              ",
    "  %  split horizontal   \"  split vertical ",
    "  hjkl / arrows  focus  HJKL / C-arrows  resize ",
    "  x  close pane         c  new tab        ",
    "  n/p  cycle tabs       1-9  select tab   ",
    "  &  close tab          w  panel selector ",
    "  b  toggle sideline    s  toggle status  ",
    "  d  detach             C-b C-b  literal  ",
    " any key dismisses                        ",
];

fn draw_overlay(cells: &mut [Cell], rows: usize, cols: usize) {
    let origin_r = TAB_BAR_ROWS as usize + 1;
    for (i, line) in KEY_TABLE.iter().enumerate() {
        let r = origin_r + i;
        if r >= rows {
            break;
        }
        for (j, ch) in line.chars().enumerate() {
            let c = 2 + j;
            if c >= cols {
                break;
            }
            cells[r * cols + c] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::INVERSE,
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Attach + main loop
// ---------------------------------------------------------------------------

async fn attach_and_run(
    stream: std::os::unix::net::UnixStream,
    socket: &Path,
) -> Result<i32, String> {
    // A server that dies between accept and Attach (e.g. no spawnable shell)
    // closes the connection without a reason; its stderr has the real cause.
    let log_hint = format!("check {}", log_path(socket).display());
    stream
        .set_nonblocking(true)
        .map_err(|e| format!("socket setup: {e}"))?;
    let stream = tokio::net::UnixStream::from_std(stream).map_err(|e| format!("socket: {e}"))?;
    let (mut sock_r, mut sock_w) = stream.into_split();

    let (cols, rows) = terminal::size().map_err(|e| format!("terminal size: {e}"))?;
    // The launch cwd keys squad selection server-side (squad.rs). An
    // unreadable cwd (deleted directory) degrades to "" - the server treats
    // it as a literal-path squad, never a refused attach.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Chrome is client-local: report the CONTENT area. A placeholder View
    // computes it before any Layout exists. The session name is the socket
    // stem by construction (`proto::socket_path`).
    let session = socket
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut view = View::new(
        (rows, cols),
        session,
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
        },
    );
    let (c_rows, c_cols) = view.content_dims();
    write_msg(
        &mut sock_w,
        &ClientMsg::Attach {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            rows: c_rows,
            cols: c_cols,
            cwd,
        },
    )
    .await
    .map_err(|e| format!("attach failed: {e}"))?;

    // The first Layout (or refusal) decides everything, BEFORE the terminal
    // is taken over, so a refusal prints as a plain one-liner (AC1-ERR,
    // version skew). ModeSync may precede it on the reliable channel - stash
    // and apply once the TUI owns the terminal.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stashed_modesync: Vec<u8> = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| format!("server did not answer the attach; {log_hint}"))?;
        let msg = tokio::time::timeout(remaining, read_msg::<_, ServerMsg>(&mut sock_r))
            .await
            .map_err(|_| format!("server did not answer the attach; {log_hint}"))?;
        match msg {
            Ok(ServerMsg::Layout {
                squads,
                active_squad,
                panes,
                focus,
                area,
                agents,
            }) => {
                view.set_layout(LayoutView {
                    squads,
                    active_squad,
                    panes,
                    focus,
                    area,
                    agents,
                });
                break;
            }
            Ok(ServerMsg::ModeSync { bytes }) => stashed_modesync.extend_from_slice(&bytes),
            Ok(ServerMsg::Bye { reason }) => return Err(reason),
            Ok(ServerMsg::Frame { pane_id, frame }) => {
                // Tolerated out-of-order preamble: keep it; the Layout names
                // its rect a message later. The wire trust boundary holds
                // even here: a geometry-inconsistent frame is refused loudly
                // (like a malformed message), never skipped or drawn.
                if !frame.geometry_ok() {
                    return Err(format!(
                        "malformed frame from server: {}x{} but {} cells",
                        frame.rows,
                        frame.cols,
                        frame.cells.len()
                    ));
                }
                view.frames.insert(pane_id, frame);
            }
            // Info answers a pre-Attach Query; the v4 control-verb replies
            // answer one-shot `fno mux pane` connections. Neither belongs on
            // an attached connection - ignore rather than desync.
            Ok(
                ServerMsg::Notice { .. }
                | ServerMsg::Info { .. }
                | ServerMsg::PaneList { .. }
                | ServerMsg::PaneText { .. }
                | ServerMsg::PaneSpawned { .. }
                | ServerMsg::Ok
                | ServerMsg::WaitDone { .. }
                | ServerMsg::Err { .. }
                // Copy answers a mouse-release, which can only follow attach:
                // stray in the preamble, ignore rather than desync.
                | ServerMsg::Copy { .. },
            ) => {}
            Err(e) => return Err(format!("attach failed: {e}; {log_hint}")),
        }
    }

    // Socket reads get their own task. `read_msg` is NOT cancellation-safe
    // (a select! that drops it between the length prefix and the body loses
    // the consumed bytes and desyncs the whole stream), so the select loop
    // below must never poll it directly - it drains this channel instead,
    // and mpsc recv IS cancel-safe.
    let (srv_tx, mut srv_rx) = mpsc::channel::<Result<ServerMsg, ProtoError>>(16);
    tokio::spawn(async move {
        loop {
            let msg = read_msg::<_, ServerMsg>(&mut sock_r).await;
            let is_err = msg.is_err();
            if srv_tx.send(msg).await.is_err() || is_err {
                break;
            }
        }
    });

    // Raw stdin -> channel; scanned by the leader layer below.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("fno-mux-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("stdin thread: {e}"))?;

    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|e| format!("signal setup: {e}"))?;

    let guard = TerminalGuard::enter()?;
    if !stashed_modesync.is_empty() {
        raw_out(&stashed_modesync).map_err(|e| format!("mode sync: {e}"))?;
    }
    let mut compositor = Compositor::new();
    let mut scanner = Scanner::default();
    // When the pending leader chord started, for the which-key hint timer
    // (US4). Client-local; the scanner state is the single source of truth
    // for WHETHER a chord is pending, this only remembers SINCE WHEN.
    let mut leader_since: Option<Instant> = None;
    // Carries a partial SGR mouse report split across reads (mouse.rs).
    let mut mouse_carry: Vec<u8> = Vec::new();
    // Clipboard delivery runs on a blocking thread and reports its outcome back
    // here, so a hanging helper (xclip on a dead X11 link) never parks the UI
    // select loop - the loop keeps draining stdin/frames while the copy lands.
    let (copy_tx, mut copy_rx) =
        tokio::sync::mpsc::unbounded_channel::<(usize, crate::clipboard::CopyOutcome)>();
    compositor
        .draw(&view.compose())
        .map_err(|e| format!("draw: {e}"))?;

    let exit: Result<i32, String> = loop {
        // Redraw-after-event; expiry of the transient notice needs a timer.
        let notice_deadline = view.notice.as_ref().map(|(_, d)| *d);
        // The which-key hint fires once per pending chord (US4, AC4-HP).
        let hint_deadline = if view.hint {
            None
        } else {
            leader_since.map(|t| t + HINT_DELAY)
        };
        tokio::select! {
            msg = srv_rx.recv() => match msg.unwrap_or(Err(ProtoError::Closed)) {
                Ok(ServerMsg::Frame { pane_id, frame }) => {
                    if !frame.geometry_ok() {
                        break Err(format!(
                            "malformed frame from server: {}x{} but {} cells",
                            frame.rows, frame.cols, frame.cells.len()
                        ));
                    }
                    // Frames for pane ids unknown to the current Layout are
                    // ignored (Concurrency: flush-then-re-emit ordering).
                    let known = view.layout.panes.iter().any(|(id, _)| *id == pane_id);
                    if known {
                        view.frames.insert(pane_id, frame);
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
                    }
                }
                Ok(ServerMsg::Layout { squads, active_squad, panes, focus, area, agents }) => {
                    view.set_layout(LayoutView { squads, active_squad, panes, focus, area, agents });
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
                Ok(ServerMsg::ModeSync { bytes }) => {
                    // Reliable-channel ordering guarantees these precede the
                    // Layout/frames that assume them; apply verbatim.
                    if let Err(e) = raw_out(&bytes) {
                        break Err(format!("mode sync: {e}"));
                    }
                }
                Ok(ServerMsg::Notice { text }) => {
                    view.set_notice(text);
                    let _ = raw_out(b"\x07");
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
                Ok(ServerMsg::Info { .. }) => {} // pre-Attach-only answer; stray here
                // v4 control-verb replies belong to one-shot `fno mux pane`
                // connections, never an attached client's stream: ignore.
                Ok(ServerMsg::PaneList { .. }
                    | ServerMsg::PaneText { .. }
                    | ServerMsg::PaneSpawned { .. }
                    | ServerMsg::Ok
                    | ServerMsg::WaitDone { .. }
                    | ServerMsg::Err { .. }) => {}
                Ok(ServerMsg::Copy { text }) => {
                    // Land the server-extracted selection on the clipboard: local
                    // exec first, OSC 52 to the outer terminal as fallback
                    // (Locked 5). The exec chain can hang (xclip on a slow X11
                    // link), so delivery runs on a blocking thread and reports its
                    // outcome back over `copy_tx` - NOT awaited here, so the select
                    // loop keeps draining stdin/frames meanwhile. The status flash
                    // (below, on the outcome arm) makes the copy observable.
                    let chars = text.chars().count();
                    let tx = copy_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let outcome = crate::clipboard::deliver(&text, raw_out);
                        let _ = tx.send((chars, outcome));
                    });
                }
                Ok(ServerMsg::Bye { reason }) => break Ok(exit_with_notice(reason)),
                Err(ProtoError::Closed) => {
                    break Ok(exit_with_notice("session ended (server closed)".into()));
                }
                Err(e) => break Err(format!("connection lost: {e}")),
            },
            bytes = stdin_rx.recv() => match bytes {
                Some(bytes) => {
                    match handle_stdin(&mut view, &mut scanner, &mut mouse_carry, &bytes, &mut sock_w).await {
                        Ok(StdinFlow::Continue) => {
                            // Sync the hint to the scanner: a chord pending
                            // arms the timer once; anything else clears both
                            // (resolving or abandoning clears the hint,
                            // AC4-HP).
                            if scanner.leader_pending() {
                                leader_since.get_or_insert_with(Instant::now);
                            } else {
                                leader_since = None;
                                view.hint = false;
                            }
                            if let Err(e) = compositor.draw(&view.compose()) {
                                break Err(format!("draw: {e}"));
                            }
                        }
                        Ok(StdinFlow::Detach) => {
                            let _ = write_msg(&mut sock_w, &ClientMsg::Detach).await;
                            break Ok(exit_with_notice("detached; run fno to reattach".into()));
                        }
                        Err(e) => break Err(e),
                    }
                }
                // The stdin thread breaks on EOF and on read error alike; by
                // the time we see None we cannot tell which, so say so.
                None => break Ok(exit_with_notice("stdin ended (closed or read error); detached".into())),
            },
            Some((chars, outcome)) = copy_rx.recv() => {
                // A clipboard delivery finished on its blocking thread: flash the
                // result (AC2-HP) or sound BEL on hard failure (AC2-ERR).
                let notice = match outcome {
                    crate::clipboard::CopyOutcome::Local(_)
                    | crate::clipboard::CopyOutcome::Osc52 { truncated: false } => {
                        format!("copied {chars} chars")
                    }
                    crate::clipboard::CopyOutcome::Osc52 { truncated: true } => {
                        format!("copied {chars} chars (truncated to clipboard limit)")
                    }
                    crate::clipboard::CopyOutcome::Failed => {
                        let _ = raw_out(b"\x07");
                        "copy failed: no clipboard tool and OSC 52 blocked".to_string()
                    }
                };
                view.set_notice(notice);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = winch.recv() => {
                if let Ok((cols, rows)) = terminal::size() {
                    view.term = (rows, cols);
                    let (c_rows, c_cols) = view.content_dims();
                    // The server resizes PTYs + grids off the content area
                    // and re-emits Layout + frames; the local redraw keeps
                    // chrome coherent meanwhile.
                    if let Err(e) = write_msg(&mut sock_w, &ClientMsg::Resize { rows: c_rows, cols: c_cols }).await {
                        break Err(format!("resize send failed: {e}"));
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
            }
            _ = async {
                match notice_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if notice_deadline.is_some() => {
                view.notice = None;
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match hint_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if hint_deadline.is_some() => {
                view.hint = true;
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
        }
    };
    drop(guard); // restore the terminal BEFORE printing the notice
    match exit {
        Ok(code) => {
            if let Some(n) = NOTICE.with(|n| n.borrow_mut().take()) {
                eprintln!("fno: {n}");
            }
            Ok(code)
        }
        Err(e) => Err(e),
    }
}

enum StdinFlow {
    Continue,
    Detach,
}

/// Route one stdin chunk: the selector consumes keys while open (AC6-FR
/// validates against the CURRENT layout before sending); otherwise the
/// leader scanner splits it into forwards and commands.
async fn handle_stdin(
    view: &mut View,
    scanner: &mut Scanner,
    mouse_carry: &mut Vec<u8>,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    // Mouse pre-pass (US1/US2/US3): pull SGR reports out first, then feed the
    // remaining bytes to the key scanner. A pane-rect event forwards for
    // server-side routing; a chrome click is swallowed (nothing reaches a pane,
    // AC3-UI); a Shift-modified event is dropped (native-selection, AC3-EDGE).
    let (reports, passthrough) = crate::mouse::extract_mouse(mouse_carry, bytes);
    for rep in reports {
        if rep.shift {
            continue;
        }
        if let Some((pane, prow, pcol)) = view.hit_test(rep.row, rep.col) {
            write_msg(
                sock_w,
                &ClientMsg::Mouse {
                    pane,
                    event: MouseEvent {
                        row: prow,
                        col: pcol,
                        kind: rep.kind,
                    },
                },
            )
            .await
            .map_err(|e| format!("mouse send failed: {e}"))?;
        }
    }
    if passthrough.is_empty() {
        return Ok(StdinFlow::Continue);
    }
    if view.overlay {
        // AC4-EDGE: one keypress dismisses the key table and does nothing
        // else. The WHOLE chunk is swallowed - splitting it could leak the
        // tail of an escape sequence into the pane, a worse bug than two
        // coalesced keypresses both dying with the overlay.
        view.overlay = false;
        return Ok(StdinFlow::Continue);
    }
    if view.selector.is_some() {
        return selector_keys(view, &passthrough, sock_w).await;
    }
    for event in scanner.scan(&passthrough) {
        match event {
            Event::Forward(chunk) => {
                // Reliable channel: awaited send, input is NEVER dropped.
                write_msg(sock_w, &ClientMsg::Input(chunk))
                    .await
                    .map_err(|e| format!("input send failed: {e}"))?;
            }
            Event::Cmd(cmd) => {
                write_msg(sock_w, &ClientMsg::Command(cmd))
                    .await
                    .map_err(|e| format!("command send failed: {e}"))?;
            }
            Event::SelectTabIdx(idx) => {
                // Resolve the digit's index to a stable TabId against the
                // last Layout; an out-of-range digit is a local BEL, never a
                // wire message the server would refuse anyway.
                let id = view
                    .layout
                    .squads
                    .iter()
                    .find(|s| s.id == view.layout.active_squad)
                    .and_then(|s| s.tabs.get(idx))
                    .map(|t| t.id);
                match id {
                    Some(id) => {
                        write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(id)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            Event::Detach => return Ok(StdinFlow::Detach),
            Event::OpenSelector => {
                if view.sel_rows().is_empty() || view.term.1 < PANEL_W + MIN_CONTENT_COLS {
                    let _ = raw_out(b"\x07");
                } else {
                    view.panel_on = true;
                    view.selector = Some(0);
                    view.sel_esc.clear();
                }
            }
            Event::TogglePanel => {
                view.panel_on = !view.panel_on;
                // Chrome changed size: report the new content area so rects
                // fill it (the reply Layout redraws everything).
                let (r, c) = view.content_dims();
                write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                    .await
                    .map_err(|e| format!("resize send failed: {e}"))?;
            }
            Event::ToggleStatus => {
                view.status_on = !view.status_on;
                // Same accounting as the sideline: the content area grew or
                // shrank by one row.
                let (r, c) = view.content_dims();
                write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                    .await
                    .map_err(|e| format!("resize send failed: {e}"))?;
            }
            Event::ShowKeys => {
                view.overlay = true;
            }
            Event::Bell => {
                let _ = raw_out(b"\x07");
            }
        }
    }
    Ok(StdinFlow::Continue)
}

/// Fold raw selector-mode bytes into simple key bytes, carrying escape state
/// in `esc` ACROSS reads (gemini medium: an arrow sequence split at a read
/// boundary must neither close the selector nor leak its tail into the
/// pane). Arrows map to their hjkl twins; unknown escape tails are
/// swallowed. A lone ESC stays pending until the next byte decides it - a
/// bare-Esc close lands on the following keypress (which is swallowed);
/// `q` closes instantly.
fn fold_selector_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    let mut keys = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            if esc.as_slice() == [0x1b] && b == b'[' {
                esc.push(b);
                continue;
            }
            if esc.as_slice() == [0x1b, b'['] {
                match b {
                    b'A' => keys.push(b'k'),
                    b'B' => keys.push(b'j'),
                    b'C' => keys.push(b'l'),
                    b'D' => keys.push(b'h'),
                    _ => {} // unknown sequence: swallowed whole
                }
                esc.clear();
                continue;
            }
            // Pending [ESC] + a non-'[' byte: that ESC was a bare Esc press.
            esc.clear();
            keys.push(0x1b);
            if b == 0x1b {
                esc.push(0x1b); // and a new one just started
            }
            continue;
        }
        if b == 0x1b {
            esc.push(0x1b);
            continue;
        }
        keys.push(b);
    }
    keys
}

/// Selector-mode keys: j/k (and arrows) move, h/l (and left/right) collapse/
/// expand, Enter selects (squad or tab), Esc/q closes. Rows and cursor are
/// re-read per key so a close mid-chunk swallows the remainder instead of
/// resurrecting the selector. Detach is leader+d from NORMAL mode only
/// (Locked 11): close the selector first.
async fn selector_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.sel_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.sel_esc = esc;
    for &k in &keys {
        let rows = view.sel_rows();
        let Some(cur) = view.selector else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        match k {
            b'j' => {
                if !rows.is_empty() {
                    view.selector = Some((cur + 1).min(rows.len() - 1));
                }
            }
            b'k' => view.selector = Some(cur.saturating_sub(1)),
            b'l' => {
                if let Some(row) = rows.get(cur) {
                    if row.tab.is_none() {
                        view.expanded.insert(row.squad);
                    }
                }
            }
            b'h' => {
                if let Some(row) = rows.get(cur) {
                    if row.tab.is_none() {
                        view.expanded.remove(&row.squad);
                    }
                }
            }
            b'\r' | b'\n' => {
                if let Some(row) = rows.get(cur) {
                    // Validate against the CURRENT catalog before sending
                    // (AC6-FR); the server refuses stale ids regardless.
                    let squad = view.layout.squads.iter().find(|s| s.id == row.squad);
                    match (squad, row.tab) {
                        (Some(_), None) => {
                            write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(row.squad)))
                                .await
                                .map_err(|e| format!("command send failed: {e}"))?;
                        }
                        (Some(s), Some(t)) if t < s.tabs.len() => {
                            if s.id != view.layout.active_squad {
                                write_msg(
                                    sock_w,
                                    &ClientMsg::Command(Command::SelectSquad(row.squad)),
                                )
                                .await
                                .map_err(|e| format!("command send failed: {e}"))?;
                            }
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::SelectTab(s.tabs[t].id)),
                            )
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                        }
                        _ => {
                            let _ = raw_out(b"\x07");
                        }
                    }
                }
                view.selector = None;
            }
            0x1b | b'q' => view.selector = None,
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Write raw bytes (BEL, ModeSync escapes) straight to the terminal.
fn raw_out(bytes: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

// The exit notice must print AFTER the alternate screen is left, or it is
// erased with the TUI. Thread-local because the select loop returns through
// several arms; a struct field would work too but this stays local to the file.
thread_local! {
    static NOTICE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn exit_with_notice(notice: String) -> i32 {
    NOTICE.with(|n| *n.borrow_mut() = Some(notice));
    0
}

/// Draws frames with a row-level diff against what was actually drawn last -
/// safe precisely because it diffs against its own output, never against a
/// prediction of server state.
struct Compositor {
    last: Option<Frame>,
}

impl Compositor {
    fn new() -> Self {
        Compositor { last: None }
    }

    fn draw(&mut self, frame: &Frame) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        let full = match &self.last {
            Some(prev) => prev.rows != frame.rows || prev.cols != frame.cols,
            None => true,
        };
        if full {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
        }
        queue!(out, cursor::Hide)?;
        for r in 0..frame.rows as usize {
            if !full {
                // Row unchanged since we drew it? Skip the write entirely.
                let prev = self.last.as_ref().unwrap();
                let w = frame.cols as usize;
                if prev.cells[r * w..(r + 1) * w] == frame.cells[r * w..(r + 1) * w] {
                    continue;
                }
            }
            self.draw_row(&mut out, frame, r)?;
        }
        queue!(out, cursor::MoveTo(frame.cursor_col, frame.cursor_row))?;
        if frame.cursor_visible {
            queue!(out, cursor::Show)?;
        } else {
            queue!(out, cursor::Hide)?;
        }
        out.flush()?;
        self.last = Some(frame.clone());
        Ok(())
    }

    fn draw_row(&self, out: &mut impl Write, frame: &Frame, r: usize) -> std::io::Result<()> {
        queue!(out, cursor::MoveTo(0, r as u16))?;
        let w = frame.cols as usize;
        let mut style_of: Option<(Color, Color, u8)> = None;
        for cell in &frame.cells[r * w..(r + 1) * w] {
            if cell.flags & proto::cell_flags::WIDE_SPACER != 0 {
                continue; // the wide glyph before it already covers this column
            }
            let key = (cell.fg, cell.bg, cell.flags);
            if style_of != Some(key) {
                apply_style(out, cell)?;
                style_of = Some(key);
            }
            queue!(out, style::Print(cell.c))?;
        }
        // Leave the line in a reset state so scrolling artifacts never bleed.
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
        Ok(())
    }
}

fn apply_style(out: &mut impl Write, cell: &Cell) -> std::io::Result<()> {
    use proto::cell_flags as cf;
    // Reset first: attribute REMOVAL (e.g. bold -> plain) has no incremental
    // form worth tracking at this scale.
    queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    if cell.flags & cf::BOLD != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if cell.flags & cf::ITALIC != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Italic))?;
    }
    if cell.flags & cf::UNDERLINE != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Underlined))?;
    }
    // A SELECTED cell (US2) toggles reverse-video: XOR with the cell's own
    // inverse so the selection is always a visible delta, even over already-
    // inverse text.
    if (cell.flags & cf::INVERSE != 0) ^ (cell.flags & cf::SELECTED != 0) {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    if cell.flags & cf::DIM != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Dim))?;
    }
    queue!(
        out,
        style::SetForegroundColor(map_color(cell.fg)),
        style::SetBackgroundColor(map_color(cell.bg))
    )?;
    Ok(())
}

fn map_color(c: Color) -> CtColor {
    match c {
        Color::Default => CtColor::Reset,
        Color::Indexed(i) => CtColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::TabMeta;
    use crate::vt::frame_text;

    fn meta(id: u64, name: &str, tabs: usize, active_tab: usize) -> SquadMeta {
        SquadMeta {
            id,
            name: name.into(),
            canonical_cwd: format!("/code/{name}"),
            tabs: (1..=tabs)
                .map(|i| TabMeta {
                    id: (i - 1) as u64,
                    name: i.to_string(),
                })
                .collect(),
            active_tab,
        }
    }

    fn text_frame(rows: u16, cols: u16, ch: char) -> Frame {
        Frame {
            rows,
            cols,
            cells: vec![
                Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: 0,
                };
                rows as usize * cols as usize
            ],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            scroll_offset: 0,
        }
    }

    fn two_pane_view() -> View {
        // 30x100 terminal, panel visible (100 >= 28+40). Content = 28x72
        // (tab bar + status row). Two panes split H: 35 + divider + 36 cols.
        let mut view = View::new(
            (30, 100),
            "main".into(),
            LayoutView {
                squads: vec![meta(1, "footnote", 2, 1), meta(2, "herdr", 1, 0)],
                active_squad: 1,
                panes: vec![
                    (
                        10,
                        Rect {
                            x: 0,
                            y: 0,
                            rows: 29,
                            cols: 35,
                        },
                    ),
                    (
                        11,
                        Rect {
                            x: 36,
                            y: 0,
                            rows: 29,
                            cols: 36,
                        },
                    ),
                ],
                focus: 11,
                area: (29, 72),
                agents: vec![],
            },
        );
        view.frames.insert(10, text_frame(29, 35, 'a'));
        view.frames.insert(11, text_frame(29, 36, 'b'));
        view
    }

    #[test]
    fn client_compose_places_panes_divider_and_chrome() {
        let view = two_pane_view();
        let frame = view.compose();
        assert!(frame.geometry_ok());
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        // Tab bar: active squad name + tabs with the active one bracketed.
        assert!(lines[0].contains("footnote"), "{:?}", lines[0]);
        assert!(lines[0].contains("[2]"), "{:?}", lines[0]);
        // Sideline: both squads listed with carets.
        assert!(lines[1].contains("▸ footnote"), "{:?}", lines[1]);
        assert!(lines[2].contains("▸ herdr"), "{:?}", lines[2]);
        // Content row: pane a, the 1-cell divider, pane b - at the offsets
        // implied by (tab bar 1 row, panel 28 cols).
        let row1: Vec<char> = lines[1].chars().collect();
        assert_eq!(row1[27], '│', "panel divider column");
        assert_eq!(row1[28], 'a', "pane 10 starts at content origin");
        assert_eq!(row1[28 + 35], '│', "pane divider between the panes");
        assert_eq!(row1[28 + 36], 'b', "pane 11 after the divider");
        // Cursor: focused pane 11's (0,0) offset by chrome + rect.
        assert_eq!(frame.cursor_row, 1);
        assert_eq!(frame.cursor_col, 28 + 36);
        assert!(frame.cursor_visible);
    }

    #[test]
    fn client_hit_test_maps_pane_and_swallows_chrome() {
        // US3 hit-test: content cells resolve to (pane, local row, local col);
        // chrome cells (tab bar, sideline) and dividers resolve to None so the
        // caller swallows them (AC3-UI: nothing forwards to a pane).
        let view = two_pane_view();
        // Inside pane 10 (content origin at outer (1, 28)).
        assert_eq!(view.hit_test(5, 30), Some((10, 4, 2)));
        // Inside pane 11 (content col 36 -> outer col 64), its top-left cell.
        assert_eq!(view.hit_test(3, 64), Some((11, 2, 0)));
        // Tab bar row is chrome.
        assert_eq!(view.hit_test(0, 40), None);
        // Sideline column (< panel_w 28) is chrome.
        assert_eq!(view.hit_test(5, 10), None);
        // The divider column between the panes covers no pane.
        assert_eq!(view.hit_test(5, 28 + 35), None);
    }

    #[test]
    fn client_compose_draws_scroll_indicator_when_pane_scrolled() {
        // AC1-UI: a `[+N]` indicator appears at a scrolled pane's top-right;
        // absent entirely when the pane is live (offset 0).
        let mut view = two_pane_view();
        assert!(!frame_text(&view.compose())
            .lines()
            .nth(1)
            .unwrap()
            .contains("[+"));
        let mut f = text_frame(29, 35, 'a');
        f.scroll_offset = 7;
        view.frames.insert(10, f);
        let frame = view.compose();
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        let row1: Vec<char> = lines[1].chars().collect();
        // start_c = origin_c(28) + rect.x(0) + rect.cols(35) - width("[+7]"=4).
        let seg: String = row1[59..63].iter().collect();
        assert_eq!(seg, "[+7]");
    }

    #[test]
    fn client_compose_status_row_shows_session_cwd_and_help() {
        // US4 AC4-UI: bottom row carries session name, active squad cwd, and
        // the `? for keys` affordance; the focused pane's scroll offset joins
        // it when non-zero (the canonical `[+N]` home).
        let mut view = two_pane_view();
        let text = frame_text(&view.compose());
        let bottom = text.lines().last().unwrap().to_string();
        assert!(bottom.contains("main"), "{bottom:?}");
        assert!(bottom.contains("/code/footnote"), "{bottom:?}");
        assert!(bottom.contains("? for keys"), "{bottom:?}");
        assert!(!bottom.contains("[+"), "no stale indicator: {bottom:?}");
        // Focused pane (11) scrolled -> [+N] in the status row.
        let mut f = text_frame(29, 36, 'b');
        f.scroll_offset = 3;
        view.frames.insert(11, f);
        let text = frame_text(&view.compose());
        assert!(text.lines().last().unwrap().contains("[+3]"));
    }

    #[test]
    fn client_status_row_accounting_and_auto_hide() {
        // AC4-ERR + the Domain Pitfall: the content area the server sees
        // shrinks by exactly the status row, and a too-short terminal
        // recovers the line (geometry beats the toggle).
        let mut view = two_pane_view();
        assert_eq!(view.content_dims(), (28, 72), "tab bar + status row");
        view.status_on = false;
        assert_eq!(view.content_dims(), (29, 72), "toggled off");
        view.status_on = true;
        view.term = (MIN_ROWS_FOR_STATUS - 1, 100);
        assert!(!view.status_visible(), "auto-hidden below min height");
        assert_eq!(view.content_dims(), (MIN_ROWS_FOR_STATUS - 2, 72));
        // And the bottom row is NOT painted over content when hidden.
        let text = frame_text(&view.compose());
        assert!(!text.lines().last().unwrap().contains("? for keys"));
    }

    #[test]
    fn client_compose_hint_paints_over_bottom_row() {
        // AC4-HP: the which-key hint lists live chords on the bottom row,
        // replacing the status content while a chord is pending - even with
        // the status row toggled off (discoverability survives the toggle).
        let mut view = two_pane_view();
        view.hint = true;
        let text = frame_text(&view.compose());
        let bottom = text.lines().last().unwrap().to_string();
        assert!(bottom.contains("hjkl focus"), "{bottom:?}");
        assert!(!bottom.contains("? for keys"), "{bottom:?}");
        view.status_on = false;
        let text = frame_text(&view.compose());
        assert!(text.lines().last().unwrap().contains("hjkl focus"));
    }

    #[test]
    fn client_compose_overlay_renders_key_table() {
        // AC4-EDGE: leader+? renders the full table over the content area.
        let mut view = two_pane_view();
        view.overlay = true;
        let text = frame_text(&view.compose());
        assert!(text.contains("fno keys"), "table header present");
        assert!(text.contains("any key dismisses"));
    }

    #[test]
    fn client_abbrev_home_only_at_component_boundary() {
        assert_eq!(
            abbrev_home_in("/home/u/code", Some("/home/u")),
            "~/code".to_string()
        );
        assert_eq!(abbrev_home_in("/home/u", Some("/home/u")), "~".to_string());
        // /home/u2 must never read as ~2.
        assert_eq!(
            abbrev_home_in("/home/u2/code", Some("/home/u")),
            "/home/u2/code".to_string()
        );
        assert_eq!(abbrev_home_in("/code", None), "/code".to_string());
        assert_eq!(abbrev_home_in("/code", Some("")), "/code".to_string());
    }

    #[test]
    fn client_apply_style_reverses_selected_cell() {
        // US2 render: a SELECTED cell emits reverse-video (XOR with the cell's
        // own inverse so selection is always a visible delta).
        let sel = Cell {
            flags: cell_flags::SELECTED,
            ..Cell::default()
        };
        let mut buf = Vec::new();
        apply_style(&mut buf, &sel).unwrap();
        assert!(
            buf.windows(4).any(|w| w == b"\x1b[7m"),
            "reverse SGR emitted"
        );
        // SELECTED over already-inverse text cancels back to non-reverse.
        let both = Cell {
            flags: cell_flags::SELECTED | cell_flags::INVERSE,
            ..Cell::default()
        };
        let mut buf2 = Vec::new();
        apply_style(&mut buf2, &both).unwrap();
        assert!(
            !buf2.windows(4).any(|w| w == b"\x1b[7m"),
            "double-inverse cancels"
        );
    }

    #[test]
    fn client_compose_agent_rows_render_under_squads_with_badges() {
        // 4a-G2 (AC1-UI/AC2 render side): agent rows appear under their
        // squad with the fact-badge glyph; exited rows dim with the exit
        // marker over any badge; orphans land under the catch-all header;
        // the selector highlight still tracks SELECTABLE rows only.
        let mut view = two_pane_view();
        let panes = view.layout.panes.clone();
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 2, 1), meta(2, "herdr", 1, 0)],
            active_squad: 1,
            panes,
            focus: 11,
            area: (29, 72),
            agents: vec![
                AgentRow {
                    squad: Some(1),
                    name: "peer".into(),
                    pane_id: Some(10),
                    badge: Some(AgentBadge::Blocked),
                    reason: Some("perm prompt".into()),
                    exited: false,
                },
                AgentRow {
                    squad: Some(1),
                    name: "dead".into(),
                    pane_id: Some(99),
                    badge: None,
                    reason: None,
                    exited: true,
                },
                AgentRow {
                    squad: None,
                    name: "bg-watch".into(),
                    pane_id: None,
                    badge: Some(AgentBadge::Working),
                    reason: None,
                    exited: false,
                },
            ],
        });
        let frame = view.compose();
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        // Row order: footnote, its two agent rows, herdr, catch-all header,
        // the orphan row.
        assert!(lines[1].contains("\u{25b8} footnote"), "{:?}", lines[1]);
        assert!(
            lines[2].contains("\u{25b2} peer: perm prompt"),
            "{:?}",
            lines[2]
        );
        assert!(lines[3].contains("\u{2717} dead"), "{:?}", lines[3]);
        assert!(lines[4].contains("\u{25b8} herdr"), "{:?}", lines[4]);
        assert!(lines[5].contains("~ agents"), "{:?}", lines[5]);
        assert!(lines[6].contains("\u{25cf} bg-watch"), "{:?}", lines[6]);
        // The exited row is DIM (fact beats badge, visually too).
        let cols = frame.cols as usize;
        let dead_cell = frame.cells[3 * cols + 2];
        assert_eq!(dead_cell.flags & cell_flags::DIM, cell_flags::DIM);
        // Selector index 1 = the second SELECTABLE row (herdr), even though
        // agent rows render between them.
        let mut sel_view = view;
        sel_view.selector = Some(1);
        let sel_frame = sel_view.compose();
        let herdr_row = 4usize;
        let sel_cell = sel_frame.cells[herdr_row * cols + 2];
        assert_eq!(
            sel_cell.flags & cell_flags::INVERSE,
            cell_flags::INVERSE,
            "selector highlight must land on the selectable herdr row"
        );
    }

    #[test]
    fn client_compose_panel_autohides_below_min_width() {
        let mut view = two_pane_view();
        // AC6-EDGE: 60 < 28 + 40 -> panel hidden, content takes full width.
        view.term = (30, 60);
        assert!(!view.panel_visible());
        // 30 rows minus tab bar + status row (both visible at this height).
        assert_eq!(view.content_dims(), (28, 60));
        let frame = view.compose();
        let text = frame_text(&frame);
        let row1 = text.lines().nth(1).unwrap();
        assert!(
            row1.starts_with('a'),
            "content must start at column 0 when the panel hides: {row1:?}"
        );
    }

    #[test]
    fn client_compose_expanded_squad_lists_tabs_and_selector_highlights() {
        let mut view = two_pane_view();
        view.expanded.insert(1);
        view.selector = Some(1); // first tab row of squad 1
        let rows = view.sel_rows();
        assert_eq!(
            rows,
            vec![
                SelRow {
                    squad: 1,
                    tab: None
                },
                SelRow {
                    squad: 1,
                    tab: Some(0)
                },
                SelRow {
                    squad: 1,
                    tab: Some(1)
                },
                SelRow {
                    squad: 2,
                    tab: None
                },
            ]
        );
        let frame = view.compose();
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("▾ footnote"), "{:?}", lines[1]);
        assert!(lines[2].contains('1'), "{:?}", lines[2]);
        assert!(lines[3].contains("*2"), "active tab marked: {:?}", lines[3]);
        // The selector row's cells carry INVERSE.
        let cols = frame.cols as usize;
        assert!(
            frame.cells[2 * cols].flags & cell_flags::INVERSE != 0,
            "selector cursor row must be highlighted"
        );
        // While the selector is open the terminal cursor hides.
        assert!(!frame.cursor_visible);
    }

    #[test]
    fn client_compose_ignores_stale_frames_and_clips_overflow() {
        let mut view = two_pane_view();
        // A frame bigger than its rect (resize in flight) must clip, not
        // panic or bleed into the divider.
        view.frames.insert(10, text_frame(40, 60, 'X'));
        let frame = view.compose();
        let text = frame_text(&frame);
        let row1: Vec<char> = text.lines().nth(1).unwrap().chars().collect();
        assert_eq!(row1[28 + 34], 'X', "last in-rect column draws");
        assert_eq!(row1[28 + 35], '│', "divider survives an oversized frame");
        // set_layout drops frames for panes the new Layout does not know.
        let mut view = two_pane_view();
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 1, 0)],
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
        });
        assert!(view.frames.contains_key(&10));
        assert!(
            !view.frames.contains_key(&11),
            "frames for dead panes are dropped at Layout"
        );
    }

    #[test]
    fn client_compose_letterboxes_beyond_the_clamped_area() {
        // AC1-UI: a 29x72 local content area showing a tab clamped to 20x50
        // - content anchors top-left, everything beyond is dim '·' filler,
        // and the cursor never enters the filler.
        let mut view = View::new(
            (30, 100),
            "main".into(),
            LayoutView {
                squads: vec![meta(1, "footnote", 1, 0)],
                active_squad: 1,
                panes: vec![(
                    10,
                    Rect {
                        x: 0,
                        y: 0,
                        rows: 20,
                        cols: 50,
                    },
                )],
                focus: 10,
                area: (20, 50),
                agents: vec![],
            },
        );
        view.frames.insert(10, text_frame(20, 50, 'a'));
        let frame = view.compose();
        let cols = frame.cols as usize;
        // In-area content cell.
        assert_eq!(frame.cells[1 * cols + 28].c, 'a');
        // One column beyond the area: filler, dim.
        let beyond_col = &frame.cells[1 * cols + 28 + 50];
        assert_eq!(beyond_col.c, '·', "beyond-area column must be filler");
        assert!(beyond_col.flags & cell_flags::DIM != 0);
        // One row beyond the area (content row 20): filler too.
        let beyond_row = &frame.cells[(1 + 20) * cols + 28];
        assert_eq!(beyond_row.c, '·', "beyond-area row must be filler");
        // Cursor confined to content even against a lying frame cursor.
        assert!(frame.cursor_row < 1 + 20 && frame.cursor_col < 28 + 50);
    }

    #[test]
    fn client_selector_fold_handles_split_escape_sequences() {
        // Gemini medium: an arrow sequence split across reads must fold into
        // one nav key - never a bare-Esc close plus leaked tail bytes.
        let mut esc = Vec::new();
        let mut keys = Vec::new();
        for chunk in [&b"\x1b"[..], &b"["[..], &b"B"[..]] {
            keys.extend(fold_selector_keys(&mut esc, chunk));
        }
        assert_eq!(keys, b"j".to_vec());
        assert!(esc.is_empty());
        // Whole-chunk arrows and hjkl mix.
        let mut esc = Vec::new();
        assert_eq!(fold_selector_keys(&mut esc, b"\x1b[Aj\x1b[C"), b"kjl");
        // A bare Esc resolves on the NEXT byte (which is swallowed).
        let mut esc = Vec::new();
        assert_eq!(fold_selector_keys(&mut esc, b"\x1b"), b"");
        assert_eq!(esc, vec![0x1b], "lone ESC stays pending");
        assert_eq!(fold_selector_keys(&mut esc, b"x"), vec![0x1b]);
        assert!(esc.is_empty());
        // Unknown sequences are swallowed whole, selector unaffected.
        let mut esc = Vec::new();
        assert_eq!(fold_selector_keys(&mut esc, b"\x1b[Z"), b"");
    }

    #[test]
    fn client_selector_rows_reanchor_on_catalog_shrink() {
        let mut view = two_pane_view();
        view.expanded.insert(1);
        view.selector = Some(3);
        // AC6-FR: the catalog shrinks (squad 1 gone); the cursor re-anchors
        // to a live row instead of pointing off the end.
        view.set_layout(LayoutView {
            squads: vec![meta(2, "herdr", 1, 0)],
            active_squad: 2,
            panes: vec![(
                20,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 29,
                    cols: 72,
                },
            )],
            focus: 20,
            area: (29, 72),
            agents: vec![],
        });
        assert_eq!(view.selector, Some(0), "cursor clamped to the live rows");
    }
}
