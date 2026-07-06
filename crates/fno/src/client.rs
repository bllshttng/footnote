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
    self, cell_flags, read_msg, write_msg, AgentBadge, AgentRow, BacklogCard, BlockDir, CardState,
    Cell, ClientMsg, Color, Command, Frame, MouseButton, MouseEvent, MouseKind, ProtoError,
    ServerMsg, SquadMeta, BUILD_VERSION, PROTO_VERSION,
};
use crate::tree::{Rect, TabId};

/// How long to wait for a just-spawned server to accept.
const SPAWN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect bound for the attach path. Longer than the scriptable verbs'
/// probe (a human is willing to wait a beat) but never infinite: a wedged
/// server must produce a clear line, not a hang.
const ATTACH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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

/// How long the pointer must settle on one new pane before focus follows it
/// (x-a496). 1003 reports every crossed cell, so a fast sweep produces a burst;
/// only a pane that stays under the pointer this long steals focus, coalescing
/// the burst to one `FocusPane` for the pane the pointer lands on.
const HOVER_DEBOUNCE: Duration = Duration::from_millis(50);

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
    // spawn_server opens a log file in the mux dir, so the dir must exist first.
    // pane run reaches here without going through run_inner's ensure (AC1-EDGE).
    proto::ensure_mux_dir().map_err(|e| format!("cannot prepare the mux dir: {e}"))?;
    match proto::connect_unix_timeout(path, ATTACH_CONNECT_TIMEOUT) {
        Ok(s) => return Ok(s),
        // A connect timeout means something holds the socket but never
        // accepted: a wedged server. Spawning over it would just lose the
        // bind race, so report instead - never hang, never clobber.
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            return Err(format!(
                "server at {} is not accepting connections (connect timed out); it is \
                 wedged. kill-server needs an accepted connection and cannot recover it - \
                 kill the server process directly (its log is at {}), then retry.",
                path.display(),
                log_path(path).display()
            ));
        }
        Err(_) => {}
    }
    if path.exists() {
        eprintln!("fno: previous session ended; starting a fresh one");
    }
    spawn_server(path)?;
    let deadline = Instant::now() + SPAWN_CONNECT_TIMEOUT;
    loop {
        match proto::connect_unix_timeout(path, ATTACH_CONNECT_TIMEOUT) {
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
    // Config->env bridge for the interactive path (x-6165). The pure-Rust mux
    // server reads no settings.yaml, so `config.mux.shell_integration: off` was
    // a silent no-op here (the Python spawn front-half already bridges
    // dispatched panes, x-b63b). Latch it at server birth: an explicit env
    // export wins (inherited naturally, never overwritten); otherwise a single
    // bounded `fno config get` decides. Only `off` needs materializing - the
    // server reads absent/anything-else as on (the default).
    if std::env::var_os("FNO_MUX_SHELL_INTEGRATION").is_none() && shell_integration_off() {
        cmd.env("FNO_MUX_SHELL_INTEGRATION", "off");
    }
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

/// Shell `fno config get mux.shell_integration` once, bounded, to learn whether
/// the interactive path must disable OSC 133 injection. Bounded + fail-open (the
/// `run_dispatch_one` idiom): any spawn/read error, a non-`off` value, or a read
/// that overruns the budget all leave injection on (the default). The bound
/// matters because this runs synchronously inside `spawn_server`, *before* the
/// client's spawn-connect wait loop exists - nothing downstream would rescue an
/// unbounded read, so a slow or wedged config read would freeze `fno` startup
/// with no notice.
///
/// Capture stdout to a FILE, not a pipe. A pipe read blocks until EOF (every
/// write-end closed), so a descendant of `fno config get` that inherits stdout
/// and outlives the direct child would hang the read even after `try_wait`
/// reports the child gone - re-introducing the very freeze the bound exists to
/// prevent (peer + sigma review). A file read never blocks on EOF; the bounded
/// try_wait/kill still caps the child's own runtime.
fn shell_integration_off() -> bool {
    const CONFIG_READ_TIMEOUT: Duration = Duration::from_secs(3);
    // 0700 per-user dir (never world-writable /tmp); pid-unique name for this
    // one-shot at server birth. Removed on every return path.
    let dir = crate::proto::mux_dir();
    if crate::proto::ensure_private_dir(&dir).is_err() {
        return false;
    }
    let out_path = dir.join(format!("shell-integration-{}.out", std::process::id()));
    let out_file = match std::fs::File::create(&out_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut child = match std::process::Command::new(crate::server::fno_bin())
        .args(["config", "get", "mux.shell_integration"])
        .stdin(std::process::Stdio::null())
        .stdout(out_file)
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&out_path);
            return false;
        }
    };
    let deadline = Instant::now() + CONFIG_READ_TIMEOUT;
    let off = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break status.success()
                    && std::fs::read_to_string(&out_path)
                        .map(|s| config_says_off(&s))
                        .unwrap_or(false);
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break false,
        }
    };
    let _ = std::fs::remove_file(&out_path);
    off
}

/// The one off-switch, matched exactly like the Rust pane-spawn side
/// (`pty::integration_disabled`): only a trimmed `off` disables injection.
fn config_says_off(stdout: &str) -> bool {
    stdout.trim() == "off"
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
    /// (v10) The focused pane's `FNO_NODE` provenance, for the status-row
    /// `⚑ <node>` cell (x-66e8). `None` for an ad-hoc pane.
    focus_node: Option<String>,
    /// (v11, x-6f77) Board-ordered work-queue cards for the sideline backlog
    /// lane; empty when the graph is unreadable or has no ready/blocked/in-flight
    /// work (the lane then renders nothing - the agents section is unaffected).
    backlog: Vec<BacklogCard>,
}

/// One selectable sideline row: a squad, or one of its tabs when expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelRow {
    squad: u64,
    tab: Option<usize>,
}

/// A live pane badged `blocked` - the answer-queue membership test (x-c929).
/// Shared by every queue read so counting/emptiness checks never clone the rows.
fn is_blocked_row(a: &AgentRow) -> bool {
    !a.exited && a.badge == Some(AgentBadge::Blocked)
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
    /// Selector cursor into [`View::display_rows`], when open (x-260a: one
    /// index space shared with painting, hover, and mouse hit-testing).
    selector: Option<usize>,
    /// Pending escape bytes in selector mode, carried ACROSS reads so a
    /// split arrow sequence can never half-close the selector and leak its
    /// tail into the pane (gemini medium).
    sel_esc: Vec<u8>,
    /// Answer-overlay cursor into [`View::blocked_queue`] (x-c929), when open;
    /// the index of the selected blocked pane in `Layout.agents` order.
    answers: Option<usize>,
    /// Pending escape bytes in answer-overlay mode (same split-arrow safety as
    /// [`View::sel_esc`]).
    ans_esc: Vec<u8>,
    /// Catch-up "while you were gone" digest lines (x-4e2d), set on attach after
    /// an absence; the next keypress dismisses it (like [`View::overlay`]).
    digest: Option<Vec<String>>,
    notice: Option<(String, Instant)>,
    /// (v12, x-e780) Active in-scrollback search (leader+/), when open. While
    /// `Some`, stdin diverts to [`search_keys`] and the bottom chrome shows the
    /// input line / counter. Client-local: opening never sends a message and
    /// never reserves a row (no Resize -> no reflow -> no dropped highlight).
    search: Option<SearchView>,
    /// Pending escape bytes in search mode, carried ACROSS reads so a split
    /// arrow sequence can never half-close the search or leak its tail into the
    /// pane (same split-arrow safety as [`View::sel_esc`]).
    search_esc: Vec<u8>,
    /// (x-a496) `config.mux.hover_focus`: focus-follows-mouse over panes.
    /// Latched once at startup (default on); false disables the hover pre-pass.
    hover_focus: bool,
    /// (x-a496) Focus-follows-mouse debounce: the pane the pointer is settling on
    /// and when it first landed there. `FocusPane` fires once the same pane holds
    /// for [`HOVER_DEBOUNCE`]; a different pane or chrome resets it.
    hover_pending: Option<(u64, Instant)>,
    /// (x-a496) The `display_rows()` index the pointer is hovering in the
    /// sideline, painted with the selector's INVERSE bar. Highlight-only - never
    /// switches the viewed squad/tab. `None` off the panel.
    hover_row: Option<usize>,
    /// (x-a496) A pending click-a-card confirm: the node to dispatch and its
    /// display label. While `Some`, keys route to the confirm (Enter dispatches,
    /// any other key cancels) and the bottom row shows the prompt.
    confirm: Option<ConfirmAction>,
    /// (x-9e5e) The pending new-workspace name buffer, `Some` while the `+`
    /// create overlay is open. Keys divert to [`create_keys`]: printable append,
    /// Backspace pops, Enter sends [`Command::NewSquad`] (empty keeps it open),
    /// Esc cancels. Client-local like `search`: opening reserves no row.
    create: Option<String>,
    /// Pending escape bytes in create-overlay mode (same split-arrow safety as
    /// [`View::search_esc`]).
    create_esc: Vec<u8>,
}

/// A pending work-queue card dispatch awaiting the operator's one-keypress
/// confirm (x-a496): `node` is the id `Command::DispatchNode` targets, `label`
/// the slug/id shown in the prompt.
struct ConfirmAction {
    node: String,
    label: String,
}

/// Client-local in-scrollback search state (v12, x-e780).
struct SearchView {
    /// The pane the search opened on (captured so every step/clear targets it,
    /// even if focus shifts server-side mid-search).
    pane: u64,
    /// The input buffer (ASCII printable; typed in typing mode).
    query: String,
    /// `false` while typing (Enter submits), `true` while browsing (n/N step).
    submitted: bool,
    /// Latest `(total, current)` from the server, `None` until the first
    /// `SearchResult`. `total == 0` renders "no matches".
    result: Option<(u32, u32)>,
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
            answers: None,
            ans_esc: Vec::new(),
            digest: None,
            notice: None,
            search: None,
            search_esc: Vec::new(),
            hover_focus: true,
            hover_pending: None,
            hover_row: None,
            confirm: None,
            create: None,
            create_esc: Vec::new(),
        }
    }

    /// The blocked-pane queue for the answer overlay (x-c929): live rows badged
    /// `blocked`, in `Layout.agents` order (the documented, deterministic cycle
    /// order - AC2-UI). Owned clones so a per-key mutation of `answers` does not
    /// alias the borrow (the same reason the selector materializes owned data
    /// per key); blocked panes are few, so the clone is cheap.
    fn blocked_queue(&self) -> Vec<AgentRow> {
        self.layout
            .agents
            .iter()
            .filter(|a| is_blocked_row(a))
            .cloned()
            .collect()
    }

    /// Open the new-workspace name overlay modally (x-9e5e): clear any
    /// keyboard-opened overlay first. `create_keys` is routed AFTER
    /// selector/answers in `handle_stdin`, so a lingering selector would
    /// otherwise swallow the typed name (codex peer review).
    fn open_create(&mut self) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.create = Some(String::new());
        self.create_esc.clear();
    }

    /// Arm the card-dispatch confirm modally (x-a496) with the same
    /// overlay-clearing discipline as [`View::open_create`]: the confirm wins
    /// the stdin routing, so a selector left open behind it would swallow the
    /// keystrokes that follow the confirm's resolution (sigma review x-260a -
    /// reachable by mouse-clicking a card while leader+w is open).
    fn open_confirm(&mut self, action: ConfirmAction) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        // A half-typed workspace name is dropped too (gemini review): the
        // confirm owns the bottom row, and resuming a hidden create overlay
        // after the confirm resolves reads as a stuck client.
        self.create = None;
        self.create_esc.clear();
        self.confirm = Some(action);
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

    /// Map a left-click on chrome (the tab bar or the sideline) to what it does:
    /// switch tab/squad, focus an agent's pane, open a new tab, or a local hint
    /// for a row that isn't directly actionable (a work-only agent, a card).
    /// `None` = not a chrome cell (the caller falls through to [`hit_test`]), so
    /// clicking anywhere off the panel still reaches the pane underneath.
    fn chrome_hit(&self, row: u16, col: u16) -> Option<ChromeHit> {
        // Tab bar (top row): walk the same spans the renderer paints. Widths are
        // usize to match the renderer (`draw_tab_bar` accumulates in usize).
        if row < TAB_BAR_ROWS {
            let col = col as usize;
            let mut c = 0usize;
            for span in self.tab_bar_spans() {
                let w = span.text.chars().count();
                if col >= c && col < c + w {
                    return match span.hit? {
                        TabHit::Tab(tid) => Some(ChromeHit::Cmds(vec![Command::SelectTab(tid)])),
                        TabHit::NewTab => Some(ChromeHit::Cmds(vec![Command::NewTab])),
                    };
                }
                c += w;
            }
            return None;
        }
        // Sideline: the panel column minus its divider. Off/narrow => no panel.
        let panel_w = self.panel_w();
        if panel_w == 0 || col >= panel_w - 1 {
            return None;
        }
        // The bottom row is overlaid by the status / which-key / search chrome
        // (draw_bottom_row paints last), so a click there belongs to that chrome,
        // not the sideline row drawn underneath it (codex P2).
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        // Row i of display_rows() is painted at TAB_BAR_ROWS + i (draw_sideline).
        self.row_action((row - TAB_BAR_ROWS) as usize)
    }

    /// What acting on sideline display row `i` does - the single resolver both
    /// a mouse click ([`View::chrome_hit`]) and the leader+w selector's Enter
    /// route through (x-260a), so the two inputs can never diverge. `None` only
    /// for an out-of-range index or an inert [`DisplayRow::Header`].
    fn row_action(&self, i: usize) -> Option<ChromeHit> {
        match self.display_rows().get(i)? {
            DisplayRow::Sel(row) => match row.tab {
                None => Some(ChromeHit::Cmds(vec![Command::SelectSquad(row.squad)])),
                Some(t) => {
                    let squad = self.layout.squads.iter().find(|s| s.id == row.squad)?;
                    let tid = squad.tabs.get(t)?.id;
                    // SelectTab already resolves the squad server-side (find_tab
                    // -> set_view), so one command switches squad+tab in a single
                    // layout push - sending SelectSquad first would flicker
                    // through the squad's previously-active tab (gemini review).
                    Some(ChromeHit::Cmds(vec![Command::SelectTab(tid)]))
                }
            },
            // A pane-hosted agent focuses its pane. A watch-only (bg/headless)
            // row has no pane here, but a claude bg row carries an attach id -
            // a click attaches it into a fresh pane. A watch-only row with no
            // attach target (non-claude, or no jobId) can only say so.
            DisplayRow::Agent(a) => match a.pane_id {
                Some(pid) => Some(ChromeHit::Cmds(vec![Command::FocusPane(pid)])),
                None => match &a.attach_id {
                    Some(id) => Some(ChromeHit::Cmds(vec![Command::AttachAgent(id.clone())])),
                    None => Some(ChromeHit::Notice("agent has no pane here")),
                },
            },
            // Only a READY card starts a session (x-a496) - the same nodes
            // leader+g would pick. Dispatching a blocked card (unmet deps) or an
            // in-flight one (already being worked) is work leader+g never selects,
            // so those say why instead of opening the confirm (codex peer review).
            // Still too costly for a stray tap, so a ready card opens a
            // one-keypress confirm rather than dispatching now.
            DisplayRow::Card(c) => match c.state {
                // A terminal too short to render the bottom-row prompt refuses
                // instead of arming an INVISIBLE confirm that would capture
                // keys and could dispatch blind (sigma review x-260a). Same
                // guard for the create overlay below.
                CardState::Ready if self.term.0 < MIN_ROWS_FOR_STATUS => Some(ChromeHit::Notice(
                    "terminal too short for the dispatch prompt",
                )),
                CardState::Ready => Some(ChromeHit::Confirm(ConfirmAction {
                    node: c.id.clone(),
                    label: if c.slug.is_empty() {
                        c.id.clone()
                    } else {
                        c.slug.clone()
                    },
                })),
                CardState::Blocked => Some(ChromeHit::Notice("card blocked - unmet deps")),
                CardState::InFlight => Some(ChromeHit::Notice("card already in flight")),
            },
            DisplayRow::Header(_) => None,
            // The `+` footer opens the name-input overlay (x-9e5e).
            DisplayRow::NewSquad if self.term.0 < MIN_ROWS_FOR_STATUS => {
                Some(ChromeHit::Notice("terminal too short for the name prompt"))
            }
            DisplayRow::NewSquad => Some(ChromeHit::OpenCreate),
        }
    }

    /// The `display_rows()` index a hover cell falls on in the sideline, or
    /// `None` when the cell is not a sideline text cell - a pane, the divider
    /// column, the tab bar, or the bottom chrome row. Mirrors [`chrome_hit`]'s
    /// sideline geometry exactly so the highlight lands where a click would
    /// (x-a496).
    fn sideline_row_at(&self, row: u16, col: u16) -> Option<usize> {
        let panel_w = self.panel_w();
        if panel_w == 0 || col >= panel_w - 1 || row < TAB_BAR_ROWS {
            return None;
        }
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        let i = (row - TAB_BAR_ROWS) as usize;
        (i < self.display_rows().len()).then_some(i)
    }

    /// Fold one bare-motion (hover) report into the sideline highlight and the
    /// focus-follows-mouse debounce state (x-a496). Does NOT fire focus - it only
    /// records which pane the pointer is settling on and when it first landed
    /// there; the select loop's settle timer commits the focus once the pointer
    /// rests past [`HOVER_DEBOUNCE`]. Firing CANNOT be reactive here: ?1003 stops
    /// reporting the instant the pointer stops, so "land in a pane and rest" (the
    /// primary gesture) emits no further event to fire on - only a timer can.
    /// `now` records the landing instant for that timer's deadline.
    fn on_hover(&mut self, row: u16, col: u16, now: Instant) {
        // Highlight is highlight-only and always on (never switches the view);
        // a cell off the sideline text column clears it.
        self.hover_row = self.sideline_row_at(row, col);

        // Focus-follows-mouse rides the off-switch. hit_test resolves a PANE
        // (chrome/divider/sideline => None), so hovering the sideline never
        // steals focus - only moving over pane content does.
        if !self.hover_focus {
            self.hover_pending = None;
            return;
        }
        match self.hit_test(row, col).map(|(p, _, _)| p) {
            // Over chrome, or already on the focused pane: nothing to settle onto.
            None => self.hover_pending = None,
            Some(p) if p == self.layout.focus => self.hover_pending = None,
            // Keep the original landing instant while the pointer stays on the
            // same pane, so continued motion WITHIN it doesn't keep pushing the
            // settle deadline forward (that would starve a slow drag of focus);
            // only a NEW pane restarts the clock, which also coalesces a fast
            // sweep - each pane crossed replaces the last, so only the pane the
            // pointer rests on survives to the timer.
            Some(p) => {
                if !matches!(self.hover_pending, Some((pending, _)) if pending == p) {
                    self.hover_pending = Some((p, now));
                }
            }
        }
    }

    /// The settle timer fired (x-a496): if a pane is still pending and is not
    /// already the focus, claim it (clearing the pending state) and return it for
    /// the caller to `FocusPane`. `None` when the pointer left the pane before
    /// the deadline or it already became the focus.
    fn take_settled_hover(&mut self) -> Option<u64> {
        let (pane, _) = self.hover_pending.take()?;
        (pane != self.layout.focus).then_some(pane)
    }

    fn set_layout(&mut self, layout: LayoutView) {
        // Frames for panes unknown to the new Layout are dead - drop them
        // (Concurrency: a frame is only ever drawn against the Layout
        // generation it belongs to).
        let live: HashSet<u64> = layout.panes.iter().map(|(id, _)| *id).collect();
        self.frames.retain(|id, _| live.contains(id));
        self.layout = layout;
        // Selector re-anchors to a live, actionable row on catalog change
        // (AC6-FR): clamp into the unified rows, then step off an inert Header
        // so the cursor never rests on a label (x-260a).
        if let Some(cur) = self.selector {
            self.selector = self.selector_anchor(cur);
        }
        // Answer overlay re-clamps when a scrape tick drops the selected blocked
        // pane (x-c929, AC2-EDGE): stay in place if a later entry now occupies
        // the slot, move to the new last entry if the removed one was last, and
        // close when the queue empties (AC2-ERR).
        if let Some(cur) = self.answers {
            let n = self
                .layout
                .agents
                .iter()
                .filter(|a| is_blocked_row(a))
                .count();
            self.answers = if n == 0 { None } else { Some(cur.min(n - 1)) };
        }
        // Hover highlight re-anchors to a live display row on a layout push
        // (x-a496, AC3-FR): a dropped row must not leave the bar on a stale index.
        // Clear (not clamp) - a re-clamp would slide the highlight to an unrelated
        // row the pointer isn't over; the next Move re-establishes it.
        if let Some(hr) = self.hover_row {
            if hr >= self.display_rows().len() {
                self.hover_row = None;
            }
        }
        // Drop a pending focus-follow whose target pane vanished, so a settle can
        // never fire `FocusPane` at a dead id (the server would refuse it anyway).
        if let Some((pane, _)) = self.hover_pending {
            if !self.layout.panes.iter().any(|(id, _)| *id == pane) {
                self.hover_pending = None;
            }
        }
    }

    fn set_notice(&mut self, text: String) {
        self.notice = Some((text, Instant::now() + NOTICE_TTL));
    }

    /// Clamp a selector cursor into the current [`View::display_rows`] and
    /// step it off an inert Header row (forward first, else backward), so the
    /// cursor never rests on a label (x-260a invariant). `None` only for an
    /// empty list, unreachable in practice: the `+ new workspace` footer keeps
    /// the rows non-empty.
    fn selector_anchor(&self, cur: usize) -> Option<usize> {
        let rows = self.display_rows();
        if rows.is_empty() {
            return None;
        }
        let cur = cur.min(rows.len() - 1);
        if !matches!(rows[cur], DisplayRow::Header(_)) {
            return Some(cur);
        }
        (cur + 1..rows.len())
            .chain((0..cur).rev())
            .find(|&i| !matches!(rows[i], DisplayRow::Header(_)))
    }

    /// The next selector stop below `cur`: the nearest following display row
    /// that is not a Header. Clamps at the end (no wrap) - `cur` itself when
    /// nothing actionable follows.
    fn selector_down(&self, cur: usize) -> usize {
        let rows = self.display_rows();
        (cur + 1..rows.len())
            .find(|&i| !matches!(rows[i], DisplayRow::Header(_)))
            .unwrap_or(cur)
    }

    /// The next selector stop above `cur` (nearest first); `cur` at the top.
    fn selector_up(&self, cur: usize) -> usize {
        let rows = self.display_rows();
        (0..cur.min(rows.len()))
            .rev()
            .find(|&i| !matches!(rows[i], DisplayRow::Header(_)))
            .unwrap_or(cur)
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
        if let Some(lines) = &self.digest {
            // x-4e2d catch-up overlay: reuse the inverse-video chrome; any key
            // dismisses (handled in handle_stdin, like the key-table overlay).
            draw_lines_overlay(&mut cells, rows, cols, lines);
        } else if self.overlay {
            draw_overlay(&mut cells, rows, cols);
        } else if let Some(sel) = self.answers {
            // x-c929 answer overlay: reuse the inverse-video overlay chrome with
            // computed lines (blocked queue + the selected pane's prompt/options).
            let blocked = self.blocked_queue();
            if !blocked.is_empty() {
                let lines = answer_overlay_lines(&blocked, sel.min(blocked.len() - 1));
                draw_lines_overlay(&mut cells, rows, cols, &lines);
            }
        }

        // Terminal cursor: the FOCUSED pane's, offset into its rect - the
        // one place the cursor may sit (AC1-UI/AC5-UI).
        let (mut cur_r, mut cur_c, mut cur_vis) = (0u16, 0u16, false);
        if self.selector.is_none() && self.answers.is_none() && self.digest.is_none() {
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
    /// The bottom terminal row is chrome (search line / which-key hint / status
    /// row, painted last by `draw_bottom_row`) rather than content or a sideline
    /// row drawn underneath. Below minimum geometry both auto-hide (AC4-ERR) and
    /// the row is content (`content_dims` handed the server the full height, a
    /// pane tiled into it, so blanking would erase it). The single truth shared
    /// by the renderer and `chrome_hit` so a click matches what's painted
    /// (codex P2).
    fn bottom_row_is_chrome(&self) -> bool {
        self.term.0 >= MIN_ROWS_FOR_STATUS
            && (self.confirm.is_some()
                || self.create.is_some()
                || self.search.is_some()
                || self.hint
                || self.status_on)
    }

    fn draw_bottom_row(&self, cells: &mut [Cell], rows: usize, cols: usize) {
        if !self.bottom_row_is_chrome() {
            return;
        }
        // A card-dispatch confirm is modal - it owns the row above everything
        // else while the operator decides (x-a496).
        if let Some(c) = &self.confirm {
            self.draw_confirm_line(cells, rows, cols, &c.label);
            return;
        }
        // The new-workspace name input overlays the row while open (x-9e5e),
        // above search/hint/status - the operator is mid-entry.
        if let Some(name) = &self.create {
            let r = rows - 1;
            for c in 0..cols {
                cells[r * cols + c] = Cell::default();
            }
            let text = format!(" new workspace: {name}_");
            for (i, ch) in text.chars().take(cols).enumerate() {
                cells[r * cols + i] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: cell_flags::BOLD,
                };
            }
            return;
        }
        // Search line takes the bottom row when active (precedence: search >
        // which-key hint > status row). It OVERLAYS whatever held the row - no
        // reserved row, so opening search never triggered a Resize/reflow.
        if let Some(sv) = &self.search {
            self.draw_search_line(cells, rows, cols, sv);
            return;
        }
        let r = rows - 1;
        // We own the row: blank it first so the divider-fill pass in `compose`
        // (which treats this uncovered row as content and paints '─' glyphs)
        // cannot bleed through the gaps between the segments below.
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
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
                        · g grab · / search · s status · d detach · ? all keys";
            for (i, ch) in text.chars().take(cols).enumerate() {
                put(cells, i, ch, 0);
            }
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
        // Provenance cell for the focused pane (x-66e8): config-free `⚑ <node>`,
        // shown only when the focused pane was node-driven. Absent for an ad-hoc
        // pane, so a plain shell reads clean.
        if let Some(node) = &self.layout.focus_node {
            for ch in format!("⚑ {node} ").chars() {
                put(cells, c, ch, cell_flags::BOLD);
                c += 1;
            }
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

    /// Paint the card-dispatch confirm prompt over the bottom row (x-a496).
    /// Blank first (the x-5041 divider-bleed gotcha), then the BOLD prompt.
    fn draw_confirm_line(&self, cells: &mut [Cell], rows: usize, cols: usize, label: &str) {
        let r = rows - 1;
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
        let text = format!(" start session on {label}? ⏎/esc");
        for (i, ch) in text.chars().take(cols).enumerate() {
            cells[r * cols + i] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::BOLD,
            };
        }
    }

    /// Paint the in-scrollback search line over the bottom row (v12, x-e780).
    /// Blank first so the divider-fill pass cannot bleed through (x-5041 gotcha).
    /// Typing shows `/query_`; browsing shows `[i/n] /query` or `/query - no
    /// matches` (total 0).
    fn draw_search_line(&self, cells: &mut [Cell], rows: usize, cols: usize, sv: &SearchView) {
        let r = rows - 1;
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
        let text = match sv.result {
            Some((0, _)) => format!(" /{} - no matches", sv.query),
            Some((total, current)) => format!(" [{current}/{total}] /{}", sv.query),
            // Typing, or submitted but awaiting the first reply: show the input.
            None => format!(" /{}_", sv.query),
        };
        for (i, ch) in text.chars().take(cols).enumerate() {
            cells[r * cols + i] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::BOLD,
            };
        }
    }

    /// The tab-bar spans in paint order: the active squad's name (inert), its
    /// tabs, then a `+` new-tab affordance. The single source both `draw_tab_bar`
    /// and `chrome_hit` walk, so a click always lands on the glyph under it.
    fn tab_bar_spans(&self) -> Vec<TabSpan> {
        let mut spans = Vec::new();
        let Some(s) = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
        else {
            return spans;
        };
        spans.push(TabSpan {
            text: format!(" {} ", s.name),
            flags: cell_flags::BOLD,
            hit: None,
        });
        for (i, t) in s.tabs.iter().enumerate() {
            let (text, flags) = if i == s.active_tab {
                (format!("[{}]", i + 1), cell_flags::INVERSE)
            } else {
                (format!(" {} ", i + 1), 0)
            };
            spans.push(TabSpan {
                text,
                flags,
                hit: Some(TabHit::Tab(t.id)),
            });
        }
        spans.push(TabSpan {
            text: " + ".to_string(),
            flags: cell_flags::DIM,
            hit: Some(TabHit::NewTab),
        });
        spans
    }

    fn draw_tab_bar(&self, cells: &mut [Cell], cols: usize) {
        let mut c = 0usize;
        'spans: for span in self.tab_bar_spans() {
            for ch in span.text.chars() {
                if c >= cols {
                    break 'spans;
                }
                cells[c] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: span.flags,
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

    /// The sideline's display order (4a-G2): each squad's squad/tab rows, that
    /// squad's agent rows, then a catch-all section for agents matched to no
    /// squad, the work-queue lane, and the `+ new workspace` footer. The ONE
    /// row enumeration (x-260a): painting, hover, mouse hit-testing, and the
    /// leader+w selector all index into it.
    fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        let mut out = Vec::new();
        for s in &self.layout.squads {
            out.push(DisplayRow::Sel(SelRow {
                squad: s.id,
                tab: None,
            }));
            if self.expanded.contains(&s.id) {
                for t in 0..s.tabs.len() {
                    out.push(DisplayRow::Sel(SelRow {
                        squad: s.id,
                        tab: Some(t),
                    }));
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
        // The `+` create-workspace affordance sits directly under the squad list
        // (x-9e5e), above the agents/work-queue sections.
        out.push(DisplayRow::NewSquad);
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
        // The work-queue lane (x-6f77): board-ordered ready/blocked/in-flight
        // cards under their own header. Empty (unreadable/no-work graph) renders
        // nothing - the agents section above is unaffected (AC-edge fail-open).
        if !self.layout.backlog.is_empty() {
            out.push(DisplayRow::Header("~ work queue"));
            out.extend(self.layout.backlog.iter().map(DisplayRow::Card));
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
            let is_header = matches!(drow, DisplayRow::Header(_));
            let (text, mut flags) = match drow {
                DisplayRow::Sel(row) => {
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
                    (text, flags)
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
                    (text, flags)
                }
                DisplayRow::Card(c) => {
                    // Ready hollow, in-flight filled, blocked triangle - a glyph
                    // vocabulary distinct from the agent badges above.
                    let glyph = match c.state {
                        CardState::Ready => '○',
                        CardState::InFlight => '●',
                        CardState::Blocked => '▲',
                    };
                    let label = if c.slug.is_empty() { &c.id } else { &c.slug };
                    // Blocked cards read dim; ready/in-flight are the actionable
                    // foreground of the queue.
                    let flags = if c.state == CardState::Blocked {
                        cell_flags::DIM
                    } else {
                        0
                    };
                    (format!("  {glyph} {label} {}", c.priority), flags)
                }
                DisplayRow::Header(h) => (h.to_string(), cell_flags::DIM),
                DisplayRow::NewSquad => ("+ new workspace".to_string(), cell_flags::DIM),
            };
            // The selector cursor OR the mouse hover paints the INVERSE bar
            // (x-a496); both are display indices now (x-260a), so the bar can
            // never drift from the painted row. Hover is highlight-only, and
            // neither bar lands on an inert Header (the cursor skips them; the
            // hover check here keeps a label from reading as actionable -
            // gemini review).
            let highlit = !is_header && (self.selector == Some(i) || self.hover_row == Some(i));
            if highlit {
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
            if highlit {
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

/// One rendered sideline line. Every variant except `Header` is actionable:
/// the selector's Enter and a mouse click resolve it through
/// [`View::row_action`] (x-260a).
enum DisplayRow<'a> {
    Sel(SelRow),
    Agent(&'a AgentRow),
    /// A work-queue backlog card (x-6f77), display-only in v1.
    Card(&'a BacklogCard),
    Header(&'static str),
    /// The `+` create-workspace affordance (x-9e5e), a footer under the squad
    /// list. A click opens the name-input overlay.
    NewSquad,
}

/// One clickable span in the tab bar (label + render flags + what a click does;
/// `None` = inert, e.g. the squad-name label).
struct TabSpan {
    text: String,
    flags: u8,
    hit: Option<TabHit>,
}

#[derive(Clone, Copy)]
enum TabHit {
    Tab(TabId),
    NewTab,
}

/// What a left-click on chrome resolves to: server commands to send, a local
/// one-line hint for a row that isn't directly actionable, or a pending confirm
/// (a work-queue card, x-a496 - dispatch is too costly for a silent tap).
enum ChromeHit {
    Cmds(Vec<Command>),
    Notice(&'static str),
    Confirm(ConfirmAction),
    /// Open the new-workspace name-input overlay (x-9e5e); the `+` footer.
    OpenCreate,
}

/// Abbreviate `$HOME` to `~` for the status row; only at a path-component
/// boundary so `/home/user2/...` never reads as `~2/...`.
fn abbrev_home(p: &str) -> String {
    // var_os, not var: HOME is a path, and the idiomatic read for a path env
    // var avoids assuming UTF-8 up front. A non-UTF-8 HOME yields None and the
    // path renders unabbreviated. Cached in a OnceLock: this runs on every
    // frame compose (a hot path during output floods) and HOME is fixed for
    // the process lifetime, so the env lookup + global env lock happens once
    // (gemini).
    static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| std::env::var_os("HOME").and_then(|s| s.into_string().ok()));
    abbrev_home_in(p, home.as_deref())
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
    "     selector ⏎ acts on the row: squad/tab ",
    "     · agent focus/attach · card dispatch · + create ",
    "  a  answer queue       b  toggle sideline ",
    "  s  toggle status      ?  this key table  ",
    "  [ ]  jump block       v  select block   ",
    "  y  copy selection     r  rerun block    ",
    "  g  grab work (dispatch next ready)     ",
    "  /  search scrollback  n/N older/newer  ",
    "  d  detach             C-b C-b  literal  ",
    " any key dismisses                        ",
];

/// Draw inverse-video overlay lines at the content area's top-left, one line
/// per row, cell-bounds-checked (a tiny terminal clips rather than panics).
/// Shared by the key-table overlay and the x-c929 answer overlay.
fn draw_lines_overlay<S: AsRef<str>>(cells: &mut [Cell], rows: usize, cols: usize, lines: &[S]) {
    let origin_r = TAB_BAR_ROWS as usize + 1;
    for (i, line) in lines.iter().enumerate() {
        let r = origin_r + i;
        if r >= rows {
            break;
        }
        for (j, ch) in line.as_ref().chars().enumerate() {
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

fn draw_overlay(cells: &mut [Cell], rows: usize, cols: usize) {
    draw_lines_overlay(cells, rows, cols, KEY_TABLE);
}

/// The answer-overlay content width; lines truncate to it (AC3-UI: a long
/// option label truncates for display while the daemon fingerprints the full
/// region text) and pad to it so the inverse block is a clean rectangle.
const ANSWER_OVERLAY_W: usize = 54;

/// Build the answer-overlay lines from the blocked-pane queue + the selected
/// pane's prompt/options (x-c929). `sel` is pre-clamped by the caller. A `▸`
/// marks the selected row (AC1-UI); an answerable row lists its numbered
/// options, a focus-only row shows the "⏎ to focus" affordance (AC1-EDGE).
fn answer_overlay_lines(blocked: &[AgentRow], sel: usize) -> Vec<String> {
    let mut lines = vec![pad_to(
        " answer queue · digit answers · n/N cycle · ⏎ focus · esc close",
        ANSWER_OVERLAY_W,
    )];
    for (i, a) in blocked.iter().enumerate() {
        let marker = if i == sel { '▸' } else { ' ' };
        let tag = if a.answerable.is_some() {
            ""
        } else {
            "  ⚠ focus"
        };
        lines.push(pad_to(
            &format!(" {marker} {}{}", a.name, tag),
            ANSWER_OVERLAY_W,
        ));
    }
    lines.push(pad_to("", ANSWER_OVERLAY_W)); // divider row
    if let Some(a) = blocked.get(sel) {
        match &a.answerable {
            Some(ans) => {
                if !ans.prompt.is_empty() {
                    lines.push(pad_to(
                        &format!("   {}", ans.prompt.replace('\n', " ")),
                        ANSWER_OVERLAY_W,
                    ));
                }
                for o in &ans.options {
                    lines.push(pad_to(
                        &format!("     {}. {}", o.idx, o.label),
                        ANSWER_OVERLAY_W,
                    ));
                }
            }
            None => lines.push(pad_to("   ⚠ needs you — ⏎ to focus", ANSWER_OVERLAY_W)),
        }
    }
    lines
}

/// Truncate `s` to `w` display chars (ellipsizing) and pad with spaces to `w`,
/// so an overlay line is a fixed-width inverse block that fully overwrites the
/// content beneath it.
fn pad_to(s: &str, w: usize) -> String {
    let count = s.chars().count();
    if count > w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        let mut t = s.to_string();
        t.push_str(&" ".repeat(w - count));
        t
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
            focus_node: None,
            backlog: Vec::new(),
        },
    );
    // Latch the focus-follows-mouse off-switch once (x-a496); a direct
    // settings.yaml read (fail-open to on), the digest_overlay idiom.
    view.hover_focus = crate::digest_overlay::hover_focus_enabled(Path::new(&cwd));
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
                focus_node,
                backlog,
            }) => {
                view.set_layout(LayoutView {
                    squads,
                    active_squad,
                    panes,
                    focus,
                    area,
                    agents,
                    focus_node,
                    backlog,
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
                // Copy answers a mouse-release, and SearchResult answers a
                // search - both can only follow attach: stray in the preamble,
                // ignore rather than desync.
                | ServerMsg::Copy { .. }
                | ServerMsg::SearchResult { .. },
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

    // x-4e2d: after an absence, fold a "while you were gone" digest for the
    // focused pane's node and show it on the FIRST frame. Fully fail-open (a
    // disabled knob, a too-recent detach, or a slow/absent `fno-agents` all
    // leave `digest` None), so it can never break the attach. It runs before the
    // first paint and is bounded by the 800ms shell-out timeout, so the worst
    // case is first paint delayed by that budget - never an indefinite hang.
    let focused_cwd = view
        .layout
        .squads
        .iter()
        .find(|s| s.id == view.layout.active_squad)
        .map(|s| s.canonical_cwd.clone())
        .unwrap_or_default();
    view.digest = crate::digest_overlay::on_attach(&view.session, &focused_cwd).await;

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
        // Focus-follows-mouse settle (x-a496): a pending hover target commits at
        // its landing time + the debounce, re-armed each loop from the latest
        // pending, so a fast sweep's earlier panes are dropped before they fire.
        let hover_deadline = view.hover_pending.map(|(_, t0)| t0 + HOVER_DEBOUNCE);
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
                Ok(ServerMsg::Layout { squads, active_squad, panes, focus, area, agents, focus_node, backlog }) => {
                    view.set_layout(LayoutView { squads, active_squad, panes, focus, area, agents, focus_node, backlog });
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
                Ok(ServerMsg::SearchResult {
                    pane_id,
                    total,
                    current,
                }) => {
                    // Land the counter on the active search line. A lost reply
                    // never wedges the client (Esc exits locally); a reply for a
                    // search we already closed is simply dropped. Total 0 = no
                    // matches: a BEL makes the empty result audible (AC1-ERR).
                    // Filter on pane_id AND submitted: results only answer a
                    // submit/step, so a stale reply from a superseded search must
                    // not paint its counter (or a zero-match BEL) onto a different
                    // pane's search, nor onto a new query still being typed.
                    if let Some(sv) = view
                        .search
                        .as_mut()
                        .filter(|sv| sv.pane == pane_id && sv.submitted)
                    {
                        sv.result = Some((total, current));
                        // BEL only while the search is still open: a late
                        // zero-result reply arriving after a local Esc must not
                        // sound a confusing bell.
                        if total == 0 {
                            let _ = raw_out(b"\x07");
                        }
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
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
                            // x-4e2d: stamp the detach time so the next attach can
                            // gate the catch-up digest on how long we were away.
                            crate::digest_overlay::record_detach(&view.session);
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
            _ = async {
                match hover_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if hover_deadline.is_some() => {
                // The pointer rested on the pending pane past the debounce
                // (x-a496): commit focus. The server replies with a Layout that
                // redraws, so no local compose is needed here.
                if let Some(pane) = view.take_settled_hover() {
                    if let Err(e) = write_msg(&mut sock_w, &ClientMsg::Command(Command::FocusPane(pane))).await {
                        break Err(format!("hover-focus send failed: {e}"));
                    }
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
        // A card-dispatch confirm is modal (x-a496): while it is open, any mouse
        // click / scroll cancels it and is SWALLOWED - it must never leak to a
        // pane underneath (the confirm prompt spans the full-width bottom row) nor
        // silently open a second card's confirm (codex peer review). Hover (Move)
        // still falls through to update the highlight beneath the prompt.
        if view.confirm.is_some() && !matches!(rep.kind, MouseKind::Move) {
            view.confirm = None;
            continue;
        }
        // Bare motion is hover (x-a496): record the sideline highlight + the
        // focus-follows-mouse settle target, and swallow it - a Move is never
        // forwarded to a pane. The actual FocusPane is committed by the select
        // loop's settle timer (a rested pointer emits no further motion event).
        if matches!(rep.kind, MouseKind::Move) {
            view.on_hover(rep.row, rep.col, Instant::now());
            continue;
        }
        // A left click on chrome (tab bar / sideline) switches tab/squad, focuses
        // an agent's pane, opens a tab, or opens a card-dispatch confirm - it
        // never reaches the pane underneath.
        if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
            if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                apply_hit(view, hit, sock_w).await?;
                continue;
            }
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
    if view.digest.is_some() {
        // x-4e2d: any key dismisses the catch-up digest into the normal view.
        // Same whole-chunk swallow as the key-table overlay below.
        view.digest = None;
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
    if view.confirm.is_some() {
        return confirm_keys(view, &passthrough, sock_w).await;
    }
    if view.selector.is_some() {
        return selector_keys(view, &passthrough, sock_w).await;
    }
    if view.answers.is_some() {
        return answer_keys(view, &passthrough, sock_w).await;
    }
    if view.create.is_some() {
        return create_keys(view, &passthrough, sock_w).await;
    }
    if view.search.is_some() {
        return search_keys(view, &passthrough, sock_w).await;
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
                // The unified rows are never empty - the `+ new workspace`
                // footer is always present - so an empty session opens on it
                // (x-260a AC3-EDGE) instead of a BEL. Only the width gate stays.
                if view.term.1 < PANEL_W + MIN_CONTENT_COLS {
                    let _ = raw_out(b"\x07");
                } else {
                    view.panel_on = true;
                    // Row 0 is a squad row (or the footer, never a Header).
                    view.selector = Some(0);
                    view.sel_esc.clear();
                }
            }
            Event::OpenAnswers => {
                // Nothing blocked -> a local BEL, the overlay never opens on an
                // empty queue (AC2-ERR). `.any` over the rows, no clone.
                if !view.layout.agents.iter().any(is_blocked_row) {
                    let _ = raw_out(b"\x07");
                } else {
                    view.answers = Some(0);
                    view.ans_esc.clear();
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
            Event::BlockJump(dir) => {
                write_msg(
                    sock_w,
                    &ClientMsg::BlockJump {
                        pane: view.layout.focus,
                        dir,
                    },
                )
                .await
                .map_err(|e| format!("block-jump send failed: {e}"))?;
            }
            Event::BlockSelect(dir) => {
                write_msg(
                    sock_w,
                    &ClientMsg::BlockSelect {
                        pane: view.layout.focus,
                        dir,
                    },
                )
                .await
                .map_err(|e| format!("block-select send failed: {e}"))?;
            }
            Event::BlockRerun => {
                write_msg(
                    sock_w,
                    &ClientMsg::BlockRerun {
                        pane: view.layout.focus,
                    },
                )
                .await
                .map_err(|e| format!("block-rerun send failed: {e}"))?;
            }
            Event::DispatchNext => {
                write_msg(sock_w, &ClientMsg::DispatchNext)
                    .await
                    .map_err(|e| format!("dispatch-next send failed: {e}"))?;
            }
            Event::SearchOpen => {
                // Enter client-local typing mode over the focused pane; keystrokes
                // divert to search_keys on the next read (no message sent yet, no
                // Resize - the input line overlays the bottom chrome). Break so no
                // same-chunk bytes after the chord leak to the pane.
                view.search = Some(SearchView {
                    pane: view.layout.focus,
                    query: String::new(),
                    submitted: false,
                    result: None,
                });
                view.search_esc.clear();
                break;
            }
            Event::Bell => {
                let _ = raw_out(b"\x07");
            }
        }
    }
    Ok(StdinFlow::Continue)
}

/// Apply one resolved [`ChromeHit`] - the single consumer both input paths
/// share (x-260a): the mouse press path and the selector's Enter. Cmds go to
/// the wire; Notice is a local one-liner; Confirm arms the one-keypress
/// dispatch prompt (x-a496); OpenCreate opens the name-input overlay (x-9e5e).
async fn apply_hit(
    view: &mut View,
    hit: ChromeHit,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match hit {
        ChromeHit::Cmds(cmds) => {
            for cmd in cmds {
                write_msg(sock_w, &ClientMsg::Command(cmd))
                    .await
                    .map_err(|e| format!("command send failed: {e}"))?;
            }
        }
        ChromeHit::Notice(msg) => view.set_notice(msg.to_string()),
        // A card hit opens the confirm (x-a496); the next keypress (Enter
        // dispatches, else cancels) resolves it via confirm_keys.
        ChromeHit::Confirm(action) => view.open_confirm(action),
        // The `+` footer opens the name-input overlay (x-9e5e); the next keys
        // route to create_keys (Enter sends NewSquad, Esc cancels).
        ChromeHit::OpenCreate => view.open_create(),
    }
    Ok(())
}

/// Card-dispatch confirm keys (x-a496): Enter (CR/LF) as the first byte sends
/// the targeted `DispatchNode`; any other key cancels. The whole chunk is
/// swallowed (like the overlay dismiss) so an arrow's escape tail can't leak
/// into a pane. `take()` clears the confirm either way, so a stale prompt can
/// never resurrect a second dispatch.
async fn confirm_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(action) = view.confirm.take() else {
        return Ok(StdinFlow::Continue);
    };
    if matches!(bytes.first(), Some(b'\r') | Some(b'\n')) {
        write_msg(
            sock_w,
            &ClientMsg::Command(Command::DispatchNode(action.node)),
        )
        .await
        .map_err(|e| format!("dispatch-node send failed: {e}"))?;
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

/// Selector-mode keys: j/k (and arrows) move over the unified display rows,
/// skipping inert Headers; h/l (and left/right) collapse/expand squad rows;
/// Enter acts on the row through [`View::row_action`] + [`apply_hit`] - the
/// same resolver a mouse click uses (x-260a), so squad/tab switch, agent
/// focus/attach, card dispatch-confirm, and workspace-create are all keyboard
/// reachable. A refusal (Notice) keeps the selector open; Esc/q closes. Rows
/// and cursor are re-read per key so a close mid-chunk swallows the remainder
/// instead of resurrecting the selector. Detach is leader+d from NORMAL mode
/// only (Locked 11): close the selector first.
async fn selector_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.sel_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.sel_esc = esc;
    for &k in &keys {
        // Rows are re-read per key (via the View helpers below) so a layout
        // push or a close mid-chunk acts on the CURRENT catalog, never a stale
        // snapshot.
        let Some(cur) = view.selector else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        match k {
            b'j' => view.selector = Some(view.selector_down(cur)),
            b'k' => view.selector = Some(view.selector_up(cur)),
            b'l' | b'h' => {
                // Expand/collapse applies to squad rows; every other variant
                // no-ops (matching today's tab rows). Materialize the owned id
                // before mutating - display_rows() borrows the layout.
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                if let Some(sq) = squad {
                    if k == b'l' {
                        view.expanded.insert(sq);
                    } else {
                        view.expanded.remove(&sq);
                    }
                }
            }
            b'\r' | b'\n' => {
                // row_action resolves against the CURRENT catalog (AC6-FR) and
                // returns an OWNED hit, so applying it can mutate the view.
                match view.row_action(cur) {
                    // A refusal keeps the selector open (x-260a locked 3): the
                    // operator stays in the list to pick another row.
                    Some(ChromeHit::Notice(msg)) => view.set_notice(msg.to_string()),
                    Some(hit) => {
                        view.selector = None;
                        apply_hit(view, hit, sock_w).await?;
                    }
                    // Out of range / Header: unreachable via j/k, but a stale
                    // cursor gets a BEL, never a silent close.
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            0x1b | b'q' => view.selector = None,
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// One folded search-input token (v12, x-e780). A printable/control byte for the
/// query, or a bare Esc press. Complete arrow sequences are swallowed by the fold
/// (cursor motion is discretionary polish, Discretion 4).
#[derive(Debug, PartialEq, Eq)]
enum SearchKey {
    Byte(u8),
    Esc,
}

/// Fold raw search-mode bytes, carrying escape state in `esc` ACROSS reads so an
/// ESC-prefixed sequence broken at a read boundary never exits the search or
/// leaks its tail into the query. A whole CSI sequence (`ESC [ ` params `x`) is
/// consumed up to and including its final byte (`0x40..=0x7e`) and swallowed, so
/// a MULTI-byte sequence - PageUp `ESC [ 5 ~`, Ctrl-Arrow `ESC [ 1 ; 5 A` - never
/// leaks its param/final tail into the typed query (gemini review, HIGH). A bare
/// Esc surfaces as [`SearchKey::Esc`]; everything else is a [`SearchKey::Byte`].
/// A lone trailing ESC stays pending until the next byte disambiguates it.
/// Query-length ceiling. Far above any real search term; only bounds the scan
/// cost against a held key or a paste. (gemini review, MEDIUM)
const MAX_SEARCH_QUERY: usize = 256;

fn fold_search_input(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<SearchKey> {
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(SearchKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b); // CSI introducer: start accumulating the sequence
                } else {
                    // A lone [ESC] then a non-'[' byte: that ESC was a bare Esc
                    // press. Surface it, then reprocess `b`.
                    esc.clear();
                    keys.push(SearchKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b); // a new ESC just started
                    } else {
                        keys.push(SearchKey::Byte(b));
                    }
                }
            }
            // Inside a CSI (`ESC [ ...`): keep eating param/intermediate bytes,
            // swallowing the whole sequence at its final byte. Bounded so a
            // pathological stream can never grow `esc` without limit.
            // ponytail: 16-byte ceiling; real CSI sequences are far shorter.
            _ => {
                if b == 0x1b {
                    // ESC aborts any in-progress sequence and starts a fresh
                    // one (standard VT semantics). Without this, an ESC arriving
                    // mid-CSI (a split sequence in the buffer) would be eaten as
                    // a param byte, so pressing Esc to cancel search would
                    // silently fail. (gemini review, HIGH)
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) || esc.len() >= 16 {
                    esc.clear();
                } else {
                    esc.push(b);
                }
            }
        }
    }
    keys
}

/// Search-mode keys (v12, x-e780). Typing: printable append, Backspace pops,
/// Enter submits (send [`ClientMsg::SearchOpen`]), Esc cancels locally. Browsing
/// (post-submit): `n`/`N` send [`ClientMsg::SearchStep`] (older/newer), Esc sends
/// [`ClientMsg::SearchClear`] and exits. Esc ALWAYS exits the mode locally even
/// if the server never replied (AC1-FR: a lost `SearchResult` never wedges the
/// input line). The mode is re-read per key so an Esc mid-chunk swallows the rest.
async fn search_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.search_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.search_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest of
        // the chunk must be swallowed, never forwarded.
        let Some(sv) = view.search.as_ref() else {
            break;
        };
        let (pane, submitted) = (sv.pane, sv.submitted);
        match key {
            SearchKey::Esc => {
                view.search = None;
                view.search_esc.clear();
                if submitted {
                    // Browsing: drop the shared server-side highlight + state.
                    write_msg(sock_w, &ClientMsg::SearchClear { pane })
                        .await
                        .map_err(|e| format!("search-clear send failed: {e}"))?;
                }
                break;
            }
            SearchKey::Byte(b) if !submitted => match b {
                b'\r' | b'\n' => {
                    if let Some(sv) = view.search.as_mut() {
                        sv.submitted = true;
                        let query = sv.query.clone();
                        write_msg(sock_w, &ClientMsg::SearchOpen { pane, query })
                            .await
                            .map_err(|e| format!("search-open send failed: {e}"))?;
                    }
                }
                0x7f | 0x08 => {
                    if let Some(sv) = view.search.as_mut() {
                        sv.query.pop();
                    }
                }
                // ASCII printable appends (other control bytes ignored; query is
                // ASCII in v1). Capped so a held key / paste can't grow it unbounded
                // and drive an O(len * scrollback) server scan.
                0x20..=0x7e => {
                    if let Some(sv) = view.search.as_mut() {
                        if sv.query.len() < MAX_SEARCH_QUERY {
                            sv.query.push(b as char);
                        }
                    }
                }
                _ => {}
            },
            SearchKey::Byte(b) => match b {
                b'n' => write_msg(
                    sock_w,
                    &ClientMsg::SearchStep {
                        pane,
                        dir: BlockDir::Prev,
                    },
                )
                .await
                .map_err(|e| format!("search-step send failed: {e}"))?,
                b'N' => write_msg(
                    sock_w,
                    &ClientMsg::SearchStep {
                        pane,
                        dir: BlockDir::Next,
                    },
                )
                .await
                .map_err(|e| format!("search-step send failed: {e}"))?,
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// New-workspace name-input keys (x-9e5e). Reuses the search input's split-arrow
/// folding: printable ASCII appends, Backspace pops, Enter sends
/// [`Command::NewSquad`] with the typed name (an empty name keeps the overlay
/// open - the server would reject it, and keeping it open avoids the round trip),
/// Esc cancels locally. The whole chunk is swallowed so an arrow's escape tail
/// never leaks into a pane.
async fn create_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.create_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.create_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest of
        // the chunk must be swallowed, never forwarded.
        if view.create.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.create = None;
                view.create_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    // Validate on a reference; only allocate when actually sending.
                    if let Some(name) = view.create.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::NewSquad {
                                    name: name.to_string(),
                                    origin: None,
                                }),
                            )
                            .await
                            .map_err(|e| format!("new-squad send failed: {e}"))?;
                            view.create = None;
                            view.create_esc.clear();
                            break;
                        }
                    }
                    // Empty name: keep the overlay open (AC2-FR shape - a failed
                    // create leaves the input intact).
                }
                0x7f | 0x08 => {
                    if let Some(buf) = view.create.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some(buf) = view.create.as_mut() {
                        if buf.len() < MAX_SEARCH_QUERY {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Answer-overlay keys (x-c929): a digit answers the selected blocked pane
/// (sending [`ClientMsg::PaneAnswer`] with the daemon-pinned option keystroke -
/// focus unchanged), `n`/`N` (and j/k/arrows) cycle the blocked queue, Enter
/// focuses+closes, Esc/q closes. The blocked queue is re-read per key so a
/// scrape tick that drops the selected pane re-clamps instead of indexing a
/// dropped row (AC2-EDGE); an emptied queue closes the overlay (AC2-ERR).
async fn answer_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.ans_esc);
    let keys = fold_selector_keys(&mut esc, bytes); // arrows -> hjkl twins
    view.ans_esc = esc;
    for &k in &keys {
        let blocked = view.blocked_queue();
        if blocked.is_empty() {
            view.answers = None; // drained mid-chunk: close, swallow the rest
            break;
        }
        let Some(cur0) = view.answers else {
            break; // closed mid-chunk
        };
        let cur = cur0.min(blocked.len() - 1);
        view.answers = Some(cur);
        match k {
            // Cycle: n/N are the documented keys; j/k and folded arrows too.
            // Wraps deterministically in Layout.agents order (AC2-UI).
            b'n' | b'j' => view.answers = Some((cur + 1) % blocked.len()),
            b'N' | b'k' => view.answers = Some((cur + blocked.len() - 1) % blocked.len()),
            b'0'..=b'9' => {
                let sel = &blocked[cur];
                let picked = sel
                    .answerable
                    .as_ref()
                    .and_then(|a| {
                        a.options
                            .iter()
                            .find(|o| o.idx.as_bytes().first() == Some(&k))
                            .map(|o| (a, o))
                    })
                    .zip(sel.pane_id);
                match picked {
                    Some(((ans, o), pane)) => {
                        // Only ever the daemon-pinned keystroke; focus unchanged.
                        // The answered pane drops from the queue on the next
                        // scrape tick; the overlay stays open to cycle onward.
                        write_msg(
                            sock_w,
                            &ClientMsg::PaneAnswer {
                                pane,
                                fingerprint: ans.fingerprint,
                                region_lines: ans.region_lines as u16,
                                keystroke: o.keystroke.clone(),
                            },
                        )
                        .await
                        .map_err(|e| format!("answer send failed: {e}"))?;
                    }
                    // AC1-ERR: a digit with no matching option (or a focus-only
                    // row) is a local BEL, never a stray key sent to any pane.
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'\r' | b'\n' => {
                // Focus affordance (AC1-EDGE, best-effort v1): navigate to the
                // pane's squad, then close. Pinning focus to the exact pane needs
                // a FocusPane(id) primitive that does not exist yet (carveout).
                if let Some(squad) = blocked[cur].squad {
                    write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(squad)))
                        .await
                        .map_err(|e| format!("command send failed: {e}"))?;
                }
                view.answers = None;
            }
            0x1b | b'q' => view.answers = None,
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
    use crate::proto::{AnswerOption, AnswerablePrompt, TabMeta};
    use crate::vt::frame_text;

    #[test]
    fn config_says_off_matches_only_trimmed_off() {
        // Bridges settings.yaml -> the env the interactive server latches
        // (x-6165). Must mirror `pty::integration_disabled`: exactly `off`.
        assert!(config_says_off("off"));
        assert!(config_says_off("off\n")); // config get trailing newline
        assert!(config_says_off("  off  "));
        assert!(!config_says_off("mux-panes\n")); // the default -> stays on
        assert!(!config_says_off("OFF")); // case-sensitive, like the Rust side
        assert!(!config_says_off("")); // unknown key / empty -> default on
    }

    #[test]
    fn fold_search_input_swallows_multibyte_csi_without_leaking() {
        // gemini review (HIGH): a multi-byte CSI must be consumed whole, never
        // leak its param/final tail into the typed query.
        let mut esc = Vec::new();
        // Printables pass through 1:1.
        assert_eq!(
            fold_search_input(&mut esc, b"ab"),
            vec![SearchKey::Byte(b'a'), SearchKey::Byte(b'b')]
        );
        assert!(esc.is_empty());
        // Arrow (3-byte), PageUp (`ESC [ 5 ~`), Ctrl-Arrow (`ESC [ 1 ; 5 A`):
        // fully swallowed, nothing reaches the query.
        assert!(fold_search_input(&mut esc, b"\x1b[A").is_empty());
        assert!(fold_search_input(&mut esc, b"\x1b[5~").is_empty());
        assert!(fold_search_input(&mut esc, b"\x1b[1;5A").is_empty());
        assert!(esc.is_empty(), "each CSI sequence consumed whole");
        // Split across reads: the tail in the next chunk still never leaks.
        assert!(fold_search_input(&mut esc, b"\x1b[").is_empty());
        assert!(fold_search_input(&mut esc, b"5~").is_empty());
        assert!(esc.is_empty());
        // A bare Esc then a printable: Esc surfaces, the printable is NOT eaten.
        assert_eq!(
            fold_search_input(&mut esc, b"\x1bx"),
            vec![SearchKey::Esc, SearchKey::Byte(b'x')]
        );
        // gemini review (HIGH): an ESC arriving mid-CSI aborts the sequence and
        // restarts, so Esc-to-cancel works even with a split CSI pending. A
        // partial CSI (`ESC [ 1 ;`) then ESC then `x` must yield exactly one Esc
        // and the `x`, never swallow the cancel as a CSI param byte.
        assert!(fold_search_input(&mut esc, b"\x1b[1;").is_empty());
        assert_eq!(
            fold_search_input(&mut esc, b"\x1bx"),
            vec![SearchKey::Esc, SearchKey::Byte(b'x')]
        );
        assert!(esc.is_empty());
    }

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
                squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
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
                focus_node: None,
                backlog: Vec::new(),
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
        assert!(lines[2].contains("▸ notes"), "{:?}", lines[2]);
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

    // A three-pane layout over two_pane_view's geometry (focus on pane 10, so
    // 11 and 12 are both hover targets). Panes tile the 72-col content area:
    // 10 -> outer 28.., 11 -> outer 52.., 12 -> outer 76...
    fn three_pane_view() -> View {
        let mut view = two_pane_view();
        let rect = |x| Rect {
            x,
            y: 0,
            rows: 29,
            cols: 23,
        };
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 2, 1)],
            active_squad: 1,
            panes: vec![(10, rect(0)), (11, rect(24)), (12, rect(48))],
            focus: 10,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
        });
        view
    }

    #[test]
    fn hover_focus_settles_on_a_landed_pane() {
        // AC1-HP: land in a non-focused pane and rest. on_hover records the
        // pending target on the SINGLE landing event (the land-and-stop gesture
        // emits nothing further); the settle timer then commits it once, and
        // take_settled_hover clears so it cannot re-fire.
        let mut view = two_pane_view(); // focus 11; pane 10 at outer col 28..
        let t0 = Instant::now();
        view.on_hover(5, 30, t0);
        assert_eq!(
            view.hover_pending.map(|(p, _)| p),
            Some(10),
            "landing recorded on one event"
        );
        assert_eq!(
            view.take_settled_hover(),
            Some(10),
            "timer commits the pane"
        );
        assert_eq!(view.take_settled_hover(), None, "cleared: no re-fire");
    }

    #[test]
    fn hover_focus_keeps_landing_time_while_on_same_pane() {
        // Continued motion WITHIN the pane must not push the settle deadline
        // forward (else a slow drag never settles): the landing instant is kept.
        let mut view = two_pane_view();
        let t0 = Instant::now();
        view.on_hover(5, 30, t0);
        view.on_hover(5, 31, t0 + Duration::from_millis(40)); // still moving in 10
        assert_eq!(
            view.hover_pending,
            Some((10, t0)),
            "same pane -> original landing time preserved"
        );
    }

    #[test]
    fn hover_focus_coalesces_fast_sweep_to_settled_pane() {
        // AC2-FR: a fast sweep across three panes leaves ONLY the pane the pointer
        // rests on pending, so the timer fires one FocusPane - not one per pane.
        // Each new pane replaces the last before its deadline; 11 is dropped.
        let mut view = three_pane_view(); // focus 10; sweep 11 -> 12
        let t0 = Instant::now();
        view.on_hover(5, 55, t0); // land on 11
        view.on_hover(5, 80, t0 + Duration::from_millis(10)); // sweep to 12: 11 dropped
        assert_eq!(
            view.hover_pending.map(|(p, _)| p),
            Some(12),
            "only 12 survives the sweep"
        );
        assert_eq!(view.take_settled_hover(), Some(12), "one FocusPane, to 12");
    }

    #[test]
    fn hover_focus_off_switch_disables_follow() {
        // AC3-EDGE: config.mux.hover_focus=false -> nothing ever becomes pending,
        // so the timer has nothing to commit. The sideline highlight is
        // unaffected (it is independent of the focus-follows switch).
        let mut view = two_pane_view();
        view.hover_focus = false;
        let t0 = Instant::now();
        view.on_hover(5, 30, t0);
        assert_eq!(view.hover_pending, None, "no settle target while disabled");
        assert_eq!(view.take_settled_hover(), None);
        // Highlight still tracks the sideline.
        view.on_hover(2, 5, t0);
        assert_eq!(view.hover_row, Some(1));
    }

    #[test]
    fn hover_focus_does_not_settle_on_the_focused_pane() {
        // Hovering the already-focused pane is a no-op: no pending target, so the
        // timer never fires a redundant FocusPane to the current focus.
        let mut view = two_pane_view(); // focus 11 at outer col 64..
        view.on_hover(5, 70, Instant::now());
        assert_eq!(view.hover_pending, None);
        assert_eq!(view.take_settled_hover(), None);
    }

    #[test]
    fn hover_highlights_sideline_row_without_switching_squad() {
        // change #3 AC1-UI + AC2-EDGE: hovering a sideline row sets hover_row and
        // the active squad/tab never change; moving off the panel clears it.
        let mut view = two_pane_view(); // rows: 0 footnote (active), 1 notes
        let before = view.layout.active_squad;
        view.on_hover(2, 5, Instant::now()); // outer row 2 = notes row (index 1)
        assert_eq!(view.hover_row, Some(1));
        assert_eq!(
            view.layout.active_squad, before,
            "hover never switches squad"
        );
        view.on_hover(5, 40, Instant::now()); // onto pane content
        assert_eq!(view.hover_row, None, "off the panel clears the highlight");
    }

    #[test]
    fn open_create_is_modal_over_keyboard_overlays() {
        // codex peer review: create_keys routes AFTER selector/answers, so
        // opening the create overlay while one is open must clear it - else the
        // typed workspace name drives the selector instead.
        let mut view = two_pane_view();
        view.selector = Some(0);
        view.answers = Some(0);
        view.open_create();
        assert!(view.selector.is_none(), "create clears an open selector");
        assert!(
            view.answers.is_none(),
            "create clears an open answer overlay"
        );
        assert!(view.search.is_none());
        assert_eq!(
            view.create.as_deref(),
            Some(""),
            "the create overlay opens with an empty buffer"
        );
    }

    #[test]
    fn layout_push_clears_stale_hover_row() {
        // change #3 AC3-FR: a layout push that drops the hovered row must not
        // leave the highlight on a now-out-of-range index.
        let mut view = two_pane_view();
        // With one squad, display_rows is [squad, + new workspace] (len 2), so a
        // hover on index 2 is now stale and must be cleared by the push.
        view.hover_row = Some(2);
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 2, 1)], // second squad dropped
            active_squad: 1,
            panes: vec![(
                11,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 29,
                    cols: 72,
                },
            )],
            focus: 11,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
        });
        assert_eq!(view.hover_row, None);
    }

    #[test]
    fn chrome_hit_card_opens_confirm_with_node() {
        // change #4: a work-queue card click resolves to a Confirm carrying the
        // node id and its display label (slug preferred), not a silent dispatch.
        let mut view = two_pane_view();
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 2, 1)],
            active_squad: 1,
            panes: vec![(
                11,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 29,
                    cols: 72,
                },
            )],
            focus: 11,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: vec![BacklogCard {
                id: "x-a496".into(),
                slug: "hover-cards".into(),
                priority: "p2".into(),
                state: CardState::Ready,
            }],
        });
        // display_rows: [footnote squad, + new workspace, Header, Card] -> the
        // card is index 3, painted at outer row TAB_BAR_ROWS + 3 = 4.
        match view.chrome_hit(4, 5) {
            Some(ChromeHit::Confirm(a)) => {
                assert_eq!(a.node, "x-a496");
                assert_eq!(a.label, "hover-cards");
            }
            other => panic!("expected Confirm, got {}", chrome_hit_label(&other)),
        }
    }

    #[test]
    fn chrome_hit_non_ready_card_is_notice_not_confirm() {
        // A blocked/in-flight card is NOT dispatchable (codex peer review): the
        // click is a local notice, never a Confirm that would start work leader+g
        // would skip. Two cards: blocked (index 2), in-flight (index 3).
        let mut view = two_pane_view();
        let card = |id: &str, state| BacklogCard {
            id: id.into(),
            slug: String::new(),
            priority: "p2".into(),
            state,
        };
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 2, 1)],
            active_squad: 1,
            panes: vec![(
                11,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 29,
                    cols: 72,
                },
            )],
            focus: 11,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: vec![
                card("x-blk", CardState::Blocked),
                card("x-fly", CardState::InFlight),
            ],
        });
        // display_rows: [squad, + new workspace, Header, blocked, in-flight]
        // -> the cards paint at outer rows 4, 5.
        assert!(
            matches!(view.chrome_hit(4, 5), Some(ChromeHit::Notice(_))),
            "blocked card -> notice, not confirm"
        );
        assert!(
            matches!(view.chrome_hit(5, 5), Some(ChromeHit::Notice(_))),
            "in-flight card -> notice, not confirm"
        );
    }

    fn cmds(hit: Option<ChromeHit>) -> Vec<Command> {
        match hit {
            Some(ChromeHit::Cmds(c)) => c,
            other => panic!("expected Cmds, got {}", chrome_hit_label(&other)),
        }
    }

    fn chrome_hit_label(hit: &Option<ChromeHit>) -> &'static str {
        match hit {
            None => "None",
            Some(ChromeHit::Cmds(_)) => "Cmds",
            Some(ChromeHit::Notice(_)) => "Notice",
            Some(ChromeHit::Confirm(_)) => "Confirm",
            Some(ChromeHit::OpenCreate) => "OpenCreate",
        }
    }

    // A left click on the tab bar switches to the clicked tab, opens a new one on
    // the `+`, and does nothing on the inert squad-name label.
    #[test]
    fn chrome_hit_tab_bar_routes_tabs_and_new_tab() {
        let view = two_pane_view(); // active squad 1 "footnote", tabs 0 & 1, +.
                                    // " footnote "=0..9, " 1 "=10..12, "[2]"=13..15, " + "=16..18.
        assert_eq!(cmds(view.chrome_hit(0, 11)), vec![Command::SelectTab(0)]);
        assert_eq!(cmds(view.chrome_hit(0, 14)), vec![Command::SelectTab(1)]);
        assert_eq!(cmds(view.chrome_hit(0, 17)), vec![Command::NewTab]);
        // The squad-name label is inert.
        assert!(view.chrome_hit(0, 5).is_none());
    }

    // A left click on a sideline squad row switches to that squad.
    #[test]
    fn chrome_hit_sideline_squad_rows() {
        let view = two_pane_view(); // no agents/cards: rows are the two squads.
        assert_eq!(cmds(view.chrome_hit(1, 4)), vec![Command::SelectSquad(1)]);
        assert_eq!(cmds(view.chrome_hit(2, 4)), vec![Command::SelectSquad(2)]);
        // The divider column and the pane content beyond it are not chrome hits.
        assert!(view.chrome_hit(1, 27).is_none());
        assert!(view.chrome_hit(1, 40).is_none());
    }

    // A pane-hosted agent row focuses its pane; a watch-only row (no pane in this
    // session) can only surface a hint.
    #[test]
    fn chrome_hit_agent_rows_focus_or_hint() {
        let hosted = AgentRow {
            squad: Some(1),
            name: "worker".into(),
            pane_id: Some(10),
            badge: Some(AgentBadge::Working),
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
        };
        // A watch-only bg row with a claude jobId: a click attaches it.
        let bg_attach = AgentRow {
            squad: None,
            name: "bg-claude".into(),
            pane_id: None,
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: Some("c19cd2c3".into()),
        };
        // A watch-only row with no attach target: a click can only hint.
        let bg_plain = AgentRow {
            squad: None,
            name: "bg-other".into(),
            pane_id: None,
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
        };
        let view = view_with_agents(vec![hosted, bg_attach, bg_plain]);
        // display order: squad 1 (row1), its agent "worker" (row2), squad 2
        // (row3), "+ new workspace" footer (row4), "~ agents" header (row5),
        // orphan "bg-claude" (row6), orphan "bg-other" (row7).
        assert_eq!(cmds(view.chrome_hit(2, 4)), vec![Command::FocusPane(10)]);
        assert_eq!(
            cmds(view.chrome_hit(6, 4)),
            vec![Command::AttachAgent("c19cd2c3".into())]
        );
        assert!(matches!(view.chrome_hit(7, 4), Some(ChromeHit::Notice(_))));
        // The "~ agents" header row is inert.
        assert!(view.chrome_hit(5, 4).is_none());
        // The "+ new workspace" footer opens the create overlay.
        assert!(matches!(view.chrome_hit(4, 4), Some(ChromeHit::OpenCreate)));
    }

    // A click on the bottom row belongs to the status/which-key/search chrome
    // painted over it, never the sideline row drawn underneath (codex P2).
    #[test]
    fn chrome_hit_bottom_chrome_row_is_swallowed() {
        // Enough agents that display_rows() reaches the last terminal row.
        let agents: Vec<AgentRow> = (0..40)
            .map(|i| AgentRow {
                squad: Some(1),
                name: format!("a{i}"),
                pane_id: Some(100 + i),
                badge: None,
                reason: None,
                exited: false,
                answerable: None,
                attach_id: None,
            })
            .collect();
        let view = view_with_agents(agents);
        let bottom = view.term.0 - 1; // last terminal row
        assert!(view.bottom_row_is_chrome(), "status row on by default");
        assert!(
            view.display_rows().len() > (bottom - TAB_BAR_ROWS) as usize,
            "sideline is long enough to underlie the bottom row"
        );
        // The row under the cursor maps to a real display row, yet the click is
        // swallowed because the bottom row is chrome.
        assert!(view.chrome_hit(bottom, 4).is_none());
        // With the status row toggled off, that same row is a live sideline hit.
        let mut view = view;
        view.status_on = false;
        assert!(!view.bottom_row_is_chrome());
        assert!(view.chrome_hit(bottom, 4).is_some());
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
        // The row is blanked first, so no divider glyphs bleed through the
        // gaps between segments.
        assert!(!bottom.contains('\u{2500}'), "no '─' bleed: {bottom:?}");
        assert!(!bottom.contains('\u{253c}'), "no '┼' bleed: {bottom:?}");
        // Focused pane (11) scrolled -> [+N] in the status row.
        let mut f = text_frame(29, 36, 'b');
        f.scroll_offset = 3;
        view.frames.insert(11, f);
        let text = frame_text(&view.compose());
        assert!(text.lines().last().unwrap().contains("[+3]"));
    }

    #[test]
    fn client_status_row_shows_focus_node_provenance() {
        // x-66e8 AC (happy): a node-driven focused pane -> `⚑ <node>` cell.
        let mut view = two_pane_view();
        view.layout.focus_node = Some("x-66e8".into());
        let bottom = frame_text(&view.compose())
            .lines()
            .last()
            .unwrap()
            .to_string();
        assert!(bottom.contains("⚑ x-66e8"), "provenance cell: {bottom:?}");
        // AC (edge): an ad-hoc pane (no node) shows no provenance cell.
        view.layout.focus_node = None;
        let bottom = frame_text(&view.compose())
            .lines()
            .last()
            .unwrap()
            .to_string();
        assert!(!bottom.contains('⚑'), "no cell for ad-hoc pane: {bottom:?}");
        // AC (edge): the which-key hint still fully overrides the row.
        view.layout.focus_node = Some("x-66e8".into());
        view.hint = true;
        let bottom = frame_text(&view.compose())
            .lines()
            .last()
            .unwrap()
            .to_string();
        assert!(bottom.contains("hjkl focus"), "hint takeover: {bottom:?}");
        assert!(!bottom.contains('⚑'), "hint hides the cell: {bottom:?}");
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
    fn client_status_off_leaves_bottom_row_as_content() {
        // codex P2: with the status row toggled off and no hint pending, the
        // bottom row belongs to content (content_dims gave the server the full
        // height) - draw_bottom_row must NOT blank it. The fixture's panes are
        // 29 rows tall from y=0, so pane content reaches the last terminal row.
        let mut view = two_pane_view();
        view.status_on = false;
        let text = frame_text(&view.compose());
        let bottom = text.lines().last().unwrap().to_string();
        assert!(
            bottom.contains('a') || bottom.contains('b'),
            "bottom row must keep pane content when status is off: {bottom:?}"
        );
        // A pending hint still transiently paints over that content row.
        view.hint = true;
        let text = frame_text(&view.compose());
        assert!(text.lines().last().unwrap().contains("hjkl focus"));
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
            squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
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
                    answerable: None,
                    attach_id: None,
                },
                AgentRow {
                    squad: Some(1),
                    name: "dead".into(),
                    pane_id: Some(99),
                    badge: None,
                    reason: None,
                    exited: true,
                    answerable: None,
                    attach_id: None,
                },
                AgentRow {
                    squad: None,
                    name: "bg-watch".into(),
                    pane_id: None,
                    badge: Some(AgentBadge::Working),
                    reason: None,
                    exited: false,
                    answerable: None,
                    attach_id: None,
                },
            ],
            focus_node: None,
            backlog: Vec::new(),
        });
        let frame = view.compose();
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        // Row order: footnote, its two agent rows, notes squad, the "+ new
        // workspace" footer, catch-all header, the orphan row.
        assert!(lines[1].contains("\u{25b8} footnote"), "{:?}", lines[1]);
        assert!(
            lines[2].contains("\u{25b2} peer: perm prompt"),
            "{:?}",
            lines[2]
        );
        assert!(lines[3].contains("\u{2717} dead"), "{:?}", lines[3]);
        assert!(lines[4].contains("\u{25b8} notes"), "{:?}", lines[4]);
        assert!(lines[5].contains("+ new workspace"), "{:?}", lines[5]);
        assert!(lines[6].contains("~ agents"), "{:?}", lines[6]);
        assert!(lines[7].contains("\u{25cf} bg-watch"), "{:?}", lines[7]);
        // The exited row is DIM (fact beats badge, visually too).
        let cols = frame.cols as usize;
        let dead_cell = frame.cells[3 * cols + 2];
        assert_eq!(dead_cell.flags & cell_flags::DIM, cell_flags::DIM);
        // The selector indexes display rows directly (x-260a): index 3 = the
        // notes squad row, after footnote and its two agent rows.
        let mut sel_view = view;
        sel_view.selector = Some(3);
        let sel_frame = sel_view.compose();
        let notes_row = 4usize;
        let sel_cell = sel_frame.cells[notes_row * cols + 2];
        assert_eq!(
            sel_cell.flags & cell_flags::INVERSE,
            cell_flags::INVERSE,
            "selector highlight must land on the selectable notes row"
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
        view.selector = Some(1); // first tab row of squad 1 (display index)
        let sel: Vec<SelRow> = view
            .display_rows()
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Sel(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(
            sel,
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
            focus_node: None,
            backlog: Vec::new(),
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
                focus_node: None,
                backlog: Vec::new(),
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
            squads: vec![meta(2, "notes", 1, 0)],
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
            focus_node: None,
            backlog: Vec::new(),
        });
        // Display rows are now [notes squad, + new workspace]: the cursor
        // clamps to the last live row (the footer, an actionable stop).
        assert_eq!(view.selector, Some(1), "cursor clamped to the live rows");
    }

    // ---- x-260a: unified selector rows (keyboard reaches every actionable row) ----

    /// A sideline with every row kind: squad 1 + its hosted agent, squad 2,
    /// the footer, an orphan-agents section (attachable bg + watch-only), and
    /// a work-queue lane (ready + blocked cards).
    ///
    /// Display rows: 0 sq1 · 1 hosted agent · 2 sq2 · 3 "+ new workspace" ·
    /// 4 "~ agents" · 5 bg-attach · 6 bg-plain · 7 "~ work queue" ·
    /// 8 ready card · 9 blocked card · 10 in-flight card.
    fn unified_rows_view() -> View {
        let agent = |squad: Option<u64>, name: &str, pane_id, attach_id: Option<&str>| AgentRow {
            squad,
            name: name.into(),
            pane_id,
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: attach_id.map(Into::into),
        };
        let card = |id: &str, state| BacklogCard {
            id: id.into(),
            slug: String::new(),
            priority: "p2".into(),
            state,
        };
        let mut v = view_with_agents(vec![
            agent(Some(1), "worker", Some(10), None),
            agent(None, "bg-claude", None, Some("c19cd2c3")),
            agent(None, "bg-other", None, None),
        ]);
        v.layout.backlog = vec![
            card("x-rdy", CardState::Ready),
            card("x-blk", CardState::Blocked),
            card("x-fly", CardState::InFlight),
        ];
        v
    }

    #[test]
    fn selector_nav_skips_headers_and_clamps() {
        // AC2-UI + Boundaries: j/k stop on every actionable row, skip the two
        // section headers, and clamp (no wrap) at both ends.
        let v = unified_rows_view();
        assert_eq!(v.selector_down(3), 5, "j from the footer skips '~ agents'");
        assert_eq!(v.selector_down(6), 8, "j skips '~ work queue'");
        assert_eq!(v.selector_down(10), 10, "clamp at the last row");
        assert_eq!(v.selector_up(5), 3, "k skips '~ agents' upward");
        assert_eq!(v.selector_up(8), 6, "k skips '~ work queue' upward");
        assert_eq!(v.selector_up(0), 0, "clamp at the top");
    }

    #[test]
    fn selector_anchor_steps_off_headers() {
        // AC1-FR / AC2-EDGE: a re-anchored cursor never rests on a Header -
        // forward first, and an out-of-range index clamps to the last row.
        let v = unified_rows_view();
        assert_eq!(v.selector_anchor(4), Some(5), "header steps forward");
        assert_eq!(v.selector_anchor(7), Some(8), "header steps forward");
        assert_eq!(v.selector_anchor(50), Some(10), "stale index clamps");
        assert_eq!(v.selector_anchor(0), Some(0), "actionable row stays put");
    }

    #[test]
    fn display_rows_footer_keeps_empty_session_actionable() {
        // AC3-EDGE: zero squads/agents/cards still yields the footer, so
        // leader+w always has a row to open on and Enter opens the create
        // overlay.
        let v = View::new(
            (30, 100),
            "main".into(),
            LayoutView {
                squads: vec![],
                active_squad: 0,
                panes: vec![],
                focus: 0,
                area: (28, 72),
                agents: vec![],
                focus_node: None,
                backlog: Vec::new(),
            },
        );
        assert_eq!(v.display_rows().len(), 1, "footer only");
        assert!(matches!(v.row_action(0), Some(ChromeHit::OpenCreate)));
    }

    #[tokio::test]
    async fn selector_enter_focuses_hosted_agent_pane() {
        // AC1-HP: Enter on a pane-hosted agent row sends FocusPane and closes.
        let mut v = unified_rows_view();
        v.selector = Some(1);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync(&mut cur).unwrap() {
            ClientMsg::Command(Command::FocusPane(10)) => {}
            other => panic!("expected FocusPane(10), got {other:?}"),
        }
        assert_eq!(v.selector, None, "acting closes the selector");
    }

    #[tokio::test]
    async fn selector_enter_attaches_bg_agent() {
        // AC1-EDGE: Enter on a claude bg row with an attach id sends
        // AttachAgent.
        let mut v = unified_rows_view();
        v.selector = Some(5);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync(&mut cur).unwrap() {
            ClientMsg::Command(Command::AttachAgent(id)) => assert_eq!(id, "c19cd2c3"),
            other => panic!("expected AttachAgent, got {other:?}"),
        }
        assert_eq!(v.selector, None);
    }

    #[tokio::test]
    async fn selector_enter_refusal_keeps_selector_open() {
        // AC1-ERR + AC2-ERR (locked 3): a refusal row (paneless agent, blocked
        // card, in-flight card) shows a notice, sends nothing, and the
        // selector stays open.
        let mut v = unified_rows_view();
        for row in [6usize, 9, 10] {
            v.selector = Some(row);
            v.notice = None;
            let mut buf: Vec<u8> = Vec::new();
            selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
            assert!(buf.is_empty(), "refusal sends nothing (row {row})");
            assert!(v.notice.is_some(), "refusal explains itself (row {row})");
            assert_eq!(v.selector, Some(row), "selector stays open (row {row})");
        }
    }

    #[tokio::test]
    async fn selector_enter_ready_card_opens_confirm() {
        // AC2-HP: Enter on a Ready card closes the selector and arms the
        // one-keypress dispatch confirm - nothing on the wire yet; the second
        // Enter (confirm_keys) sends the DispatchNode (AC2-FR: the confirm
        // takes the action, so one dispatch at most).
        let mut v = unified_rows_view();
        v.selector = Some(8);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "confirm first, dispatch on the next Enter");
        assert_eq!(v.selector, None);
        assert_eq!(v.confirm.as_ref().map(|c| c.node.as_str()), Some("x-rdy"));
    }

    #[tokio::test]
    async fn selector_enter_footer_opens_create_overlay() {
        // AC3-HP: Enter on "+ new workspace" opens the name-input overlay.
        let mut v = unified_rows_view();
        v.selector = Some(3);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert!(buf.is_empty());
        assert_eq!(v.selector, None, "open_create clears the selector");
        assert_eq!(v.create.as_deref(), Some(""));
    }

    #[test]
    fn open_confirm_is_modal_over_keyboard_overlays() {
        // sigma review x-260a: a mouse click arming the confirm while the
        // keyboard selector (or answer overlay) is open must clear it - the
        // confirm wins stdin routing, so a lingering selector would swallow
        // the keystrokes after the confirm resolves. Same discipline as
        // open_create.
        let mut view = unified_rows_view();
        view.selector = Some(8);
        view.answers = Some(0);
        view.create = Some("half-typed".into());
        view.open_confirm(ConfirmAction {
            node: "x-rdy".into(),
            label: "x-rdy".into(),
        });
        assert!(view.selector.is_none(), "confirm clears an open selector");
        assert!(view.answers.is_none(), "confirm clears the answer overlay");
        assert!(view.create.is_none(), "confirm drops a half-typed create");
        assert!(view.search.is_none());
        assert_eq!(
            view.confirm.as_ref().map(|c| c.node.as_str()),
            Some("x-rdy")
        );
    }

    #[test]
    fn short_terminal_degrades_prompts_to_notices() {
        // sigma review x-260a: below MIN_ROWS_FOR_STATUS the bottom-row
        // prompt cannot render, so a Ready card and the footer refuse with a
        // notice instead of arming an invisible modal (which could dispatch
        // blind on the next Enter).
        let mut v = unified_rows_view();
        v.term.0 = MIN_ROWS_FOR_STATUS - 1;
        assert!(
            matches!(v.row_action(8), Some(ChromeHit::Notice(_))),
            "ready card refuses on a too-short terminal"
        );
        assert!(
            matches!(v.row_action(3), Some(ChromeHit::Notice(_))),
            "footer refuses on a too-short terminal"
        );
        // At the minimum height both act normally again.
        v.term.0 = MIN_ROWS_FOR_STATUS;
        assert!(matches!(v.row_action(8), Some(ChromeHit::Confirm(_))));
        assert!(matches!(v.row_action(3), Some(ChromeHit::OpenCreate)));
    }

    #[tokio::test]
    async fn selector_keys_navigate_unified_rows() {
        // AC1-UI / AC2-UI: j/k through selector_keys land on agent and card
        // rows, skipping headers, without sending anything.
        let mut v = unified_rows_view();
        v.selector = Some(3);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"j", &mut buf).await.unwrap();
        assert_eq!(v.selector, Some(5), "j skips the '~ agents' header");
        selector_keys(&mut v, b"k", &mut buf).await.unwrap();
        assert_eq!(v.selector, Some(3), "k skips it back");
        assert!(buf.is_empty(), "navigation sends nothing");
    }

    // ---- x-c929: answer overlay + next-blocked cycle ----

    fn answerable(idx_labels: &[(&str, &str)], fp: u8) -> AnswerablePrompt {
        AnswerablePrompt {
            prompt: "Do you want to proceed?".into(),
            options: idx_labels
                .iter()
                .map(|(i, l)| AnswerOption {
                    idx: (*i).into(),
                    label: (*l).into(),
                    keystroke: i.as_bytes().to_vec(),
                })
                .collect(),
            fingerprint: [fp; 32],
            region_lines: 8,
        }
    }

    fn blocked_row(name: &str, pane: u64, ans: Option<AnswerablePrompt>) -> AgentRow {
        AgentRow {
            squad: Some(1),
            name: name.into(),
            pane_id: Some(pane),
            badge: Some(AgentBadge::Blocked),
            reason: None,
            exited: false,
            answerable: ans,
            attach_id: None,
        }
    }

    fn view_with_agents(agents: Vec<AgentRow>) -> View {
        let mut v = two_pane_view();
        v.layout.agents = agents;
        v
    }

    #[test]
    fn xc929_pad_to_truncates_and_pads() {
        assert_eq!(pad_to("hi", 5), "hi   ");
        assert_eq!(pad_to("hello", 5), "hello");
        assert_eq!(pad_to("hello world", 5), "hell…");
    }

    // AC1-UI + AC1-EDGE: the selected row is marked, an answerable row lists its
    // numbered options, a focus-only row shows the "⏎ to focus" affordance.
    #[test]
    fn xc929_answer_overlay_lines_marks_selection_and_renders_focus_only() {
        let rows = vec![
            blocked_row("peer", 4, Some(answerable(&[("1", "Yes"), ("2", "No")], 7))),
            blocked_row("other", 5, None),
        ];
        let lines = answer_overlay_lines(&rows, 0);
        assert!(
            lines[1].trim_start().starts_with("▸ peer"),
            "{:?}",
            lines[1]
        );
        assert!(lines[2].contains("other") && lines[2].contains("⚠ focus"));
        assert!(lines.iter().any(|l| l.contains("1. Yes")));
        assert!(lines.iter().any(|l| l.contains("2. No")));
        // Selecting the focus-only row shows the affordance, no digits.
        let lines = answer_overlay_lines(&rows, 1);
        assert!(lines.iter().any(|l| l.contains("needs you")));
        assert!(!lines.iter().any(|l| l.contains("1. Yes")));
    }

    #[test]
    fn xc929_blocked_queue_filters_to_live_blocked_rows() {
        let mut working = blocked_row("working", 2, None);
        working.badge = Some(AgentBadge::Working);
        let mut dead = blocked_row("dead", 3, None);
        dead.exited = true;
        let v = view_with_agents(vec![
            blocked_row("a", 1, None),
            working,
            dead,
            blocked_row("b", 4, None),
        ]);
        let q = v.blocked_queue();
        assert_eq!(
            q.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "only live blocked rows, in Layout.agents order"
        );
    }

    // AC2-EDGE: a scrape tick that drops the selected pane re-clamps the overlay
    // cursor instead of indexing a dropped row; an emptied queue closes it.
    #[test]
    fn xc929_answers_reclamp_and_close_on_layout_change() {
        let mut v = view_with_agents(vec![blocked_row("a", 1, None), blocked_row("b", 2, None)]);
        v.answers = Some(1);
        // The last blocked pane drops -> cursor clamps to the new last (0).
        v.set_layout(LayoutView {
            squads: v.layout.squads.clone(),
            active_squad: 1,
            panes: v.layout.panes.clone(),
            focus: v.layout.focus,
            area: v.layout.area,
            agents: vec![blocked_row("a", 1, None)],
            focus_node: None,
            backlog: Vec::new(),
        });
        assert_eq!(v.answers, Some(0));
        // The queue empties -> the overlay closes (AC2-ERR).
        v.set_layout(LayoutView {
            squads: v.layout.squads.clone(),
            active_squad: 1,
            panes: v.layout.panes.clone(),
            focus: v.layout.focus,
            area: v.layout.area,
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
        });
        assert_eq!(v.answers, None);
    }

    // AC1-HP: a digit on an answerable pane sends PaneAnswer with the exact
    // daemon-pinned keystroke/fingerprint/region_lines; the overlay stays open.
    #[tokio::test]
    async fn xc929_answer_keys_digit_sends_pinned_paneanswer() {
        let mut v = view_with_agents(vec![blocked_row(
            "peer",
            4,
            Some(answerable(&[("1", "Yes"), ("2", "No")], 9)),
        )]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"1", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        match msg {
            ClientMsg::PaneAnswer {
                pane,
                fingerprint,
                region_lines,
                keystroke,
            } => {
                assert_eq!(pane, 4);
                assert_eq!(fingerprint, [9u8; 32]);
                assert_eq!(region_lines, 8);
                assert_eq!(keystroke, b"1");
            }
            other => panic!("expected PaneAnswer, got {other:?}"),
        }
        assert_eq!(v.answers, Some(0), "overlay stays open to cycle onward");
    }

    // AC1-ERR: a digit with no matching option never sends a keystroke.
    #[tokio::test]
    async fn xc929_answer_keys_no_matching_option_sends_nothing() {
        let mut v = view_with_agents(vec![blocked_row(
            "peer",
            4,
            Some(answerable(&[("1", "Yes"), ("2", "No")], 9)),
        )]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"7", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "no-such-option sends nothing (AC1-ERR)");
    }

    // AC2-HP + AC2-UI: n/N cycle the blocked queue and wrap deterministically;
    // Esc closes.
    #[tokio::test]
    async fn xc929_answer_keys_cycle_wraps_and_esc_closes() {
        let mut v = view_with_agents(vec![blocked_row("a", 1, None), blocked_row("b", 2, None)]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"n", &mut buf).await.unwrap();
        assert_eq!(v.answers, Some(1));
        answer_keys(&mut v, b"n", &mut buf).await.unwrap();
        assert_eq!(v.answers, Some(0), "n wraps to the first");
        answer_keys(&mut v, b"N", &mut buf).await.unwrap();
        assert_eq!(v.answers, Some(1), "N wraps backward to the last");
        // `q` closes instantly (a lone Esc pends until the next byte, the shared
        // fold_selector_keys behavior; `q` is the unambiguous close).
        answer_keys(&mut v, b"q", &mut buf).await.unwrap();
        assert_eq!(v.answers, None, "q closes");
        assert!(
            buf.is_empty(),
            "cycling and closing send nothing to the pane"
        );
    }
}
