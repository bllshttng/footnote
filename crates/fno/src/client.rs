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

use crate::keys::{key_bindings, meta_rows, resolve_chord, Event, KeySection, Scanner};
use crate::popup::{self, Anchor, GridCell, NavDir, Popup, PopupRow};
use crate::proto::{
    self, cell_flags, read_msg, write_msg, AgentBadge, AgentRow, AnswerablePrompt, BacklogCard,
    BlockDir, CardState, Cell, ClientMsg, Color, Command, Frame, MouseButton, MouseEvent,
    MouseKind, PanePlacement, PaneTarget, ProtoError, ServerMsg, SquadMeta, BUILD_VERSION,
    MAX_SQUAD_NAME, MAX_TAB_NAME, PROTO_VERSION,
};
use crate::tree::{Dir, Rect, TabId};

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
/// The sideline footer's `+ new` and `☰ menu` labels (x-8ccf US4). The menu
/// button rides the existing new-workspace footer row's right edge when the
/// panel is wide enough (see [`View::footer_menu_range`]).
const FOOTER_NEW_LABEL: &str = "+ new workspace";
const FOOTER_MENU: &str = "☰ menu";
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
        Ok(s) => {
            e2e_client_log(format_args!(
                "connected to live server at {}",
                path.display()
            ));
            return Ok(s);
        }
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
        Err(e) => {
            e2e_client_log(format_args!("connect failed ({e}); spawning a server"));
        }
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

/// x-0296 CI diagnostics: connect-path breadcrumbs, FNO_E2E-gated, appended
/// to `<mux_dir>/client-<pid>.log` (the e2e harness dumps every `*.log` in
/// its scratch on a timeout). NEVER stderr: pre-TUI stderr reaches the
/// client's PTY, and any byte there trips the harness's screen-not-empty
/// gates before the client has actually attached.
fn e2e_client_log(msg: std::fmt::Arguments<'_>) {
    if std::env::var_os("FNO_E2E").is_none() {
        return;
    }
    let path = proto::mux_dir().join(format!("client-{}.log", std::process::id()));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        use std::io::Write as _;
        let _ = writeln!(f, "[{ms} pid {}] {msg}", std::process::id());
    }
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
    // server reads no config.toml, so `config.mux.shell_integration: off` was
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

/// The squad-row rollup glyph for a folded `PaneState` (x-d140). Idle yields
/// `None` (the row renders byte-identically to a pre-rollup row); the rest
/// reuse the navigator's `nav_glyph` so a squad reads in its members' language.
fn rollup_glyph(state: PaneState) -> Option<char> {
    match state {
        PaneState::Idle => None,
        PaneState::Blocked | PaneState::Working | PaneState::DoneUnseen => Some(nav_glyph(state)),
    }
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
    /// The which-key keybinds modal (leader+?, x-8ccf US3): a centered popup
    /// built from the leader-chord table. While open, a bound key executes
    /// through the SAME dispatch as a direct chord (which-key); arrows/pgup
    /// scroll+select, Enter runs the selected row, Esc/unbound closes. `None`
    /// when closed. Replaces the old static top-left key-table poster.
    keys_modal: Option<KeysModal>,
    /// Pending escape bytes in modal mode (arrow/pgup folding), same split-arrow
    /// safety as [`View::sel_esc`].
    keys_modal_esc: Vec<u8>,
    /// (x-8ccf US2) The right-click / `m` row context menu over a sideline agent
    /// row: an anchored popup whose entries route to existing commands. `None`
    /// when closed. The target is pinned by name so a layout reshuffle can only
    /// stale-refuse an action, never redirect it.
    row_menu: Option<RowMenu>,
    /// Pending escape bytes in row-menu mode (arrow folding).
    row_menu_esc: Vec<u8>,
    /// (x-8ccf US4/US5) The sideline MENU popup or the settings modal (they share
    /// one slot; MENU chains into settings). `None` when closed.
    aux: Option<AuxPopup>,
    /// Pending escape bytes in aux-popup mode (arrow folding).
    aux_esc: Vec<u8>,
    /// Caret expansion per squad id - client-local, instant (AC6-UI).
    expanded: HashSet<u64>,
    /// Selector cursor into [`View::display_rows`], when open (x-260a: one
    /// index space shared with painting, hover, and mouse hit-testing).
    selector: Option<usize>,
    /// Pending escape bytes in selector mode, carried ACROSS reads so a
    /// split arrow sequence can never half-close the selector and leak its
    /// tail into the pane (gemini medium).
    sel_esc: Vec<u8>,
    /// (x-a621) First-visible [`View::display_rows`] index in the sideline:
    /// follow-the-cursor scroll offset so rows below the fold render and take
    /// the mouse. 0 (top-anchored) whenever the catalog fits the height.
    sideline_offset: usize,
    /// Answer-overlay cursor into [`View::blocked_queue`] (x-c929), when open;
    /// the index of the selected blocked pane in `Layout.agents` order.
    answers: Option<usize>,
    /// Pending escape bytes in answer-overlay mode (same split-arrow safety as
    /// [`View::sel_esc`]).
    ans_esc: Vec<u8>,
    /// (x-feec) The event-derived needs-me leg: the last `fno-agents needs` fold
    /// result while the overlay is open (`None` = live-only, not yet fetched
    /// this open). Merged with the live badge leg by [`View::needs_view`].
    needs_fold: Option<Vec<crate::needs_overlay::FoldItem>>,
    /// (x-feec) When `needs_fold` was last fetched, for the short re-open cache.
    needs_fold_at: Option<Instant>,
    /// (x-feec) The last fold shell-out failed/timed out: render the loud
    /// degraded notice (AC2-ERR) instead of a silent partial queue.
    needs_degraded: bool,
    /// (x-feec) Set by OpenAnswers when a fresh fold is wanted; the run loop
    /// spawns the shell-out and clears it, keeping the channel sender out of the
    /// deep stdin handler.
    needs_want: bool,
    /// (x-feec) A fold shell-out is running; bounds concurrent folds to one so
    /// mashing leader+a on a stale cache cannot spawn a pile of children (P2-5).
    needs_inflight: bool,
    /// (x-feec) Generation token, bumped on every open/close so a fold result
    /// landing after the overlay closed or re-opened is discarded (AC6-FR).
    needs_gen: u64,
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
    /// (x-c150; widened x-96e8) The pending rename buffer: `(target captured at
    /// open, typed name)`, `Some` while the `leader+,` (tab) or selector `r`
    /// (squad) overlay is open. Keys divert to [`rename_keys`]. Enter on an
    /// EMPTY buffer still sends (blank = clear back to the derived label),
    /// unlike `create`.
    rename: Option<(RenameTarget, String)>,
    /// Pending escape bytes in rename-overlay mode (same split-arrow safety
    /// as [`View::create_esc`]).
    rename_esc: Vec<u8>,
    /// (x-8f11) Multi-select marks for bulk recruit: the `attach_id`s toggled
    /// with `space` in the sideline selector. Client-local ephemera keyed by id,
    /// so a marked row surviving a filter/scroll keeps its mark and a vanished
    /// row simply drops it (never a stale index). Cleared on a recruit submit.
    marks: std::collections::HashSet<String>,
    /// (x-8f11) The pending recruit workspace-name buffer, `Some` while the `R`
    /// recruit overlay is open. Enter sends [`Command::RecruitAgents`] with the
    /// marked ids (empty keeps it open, like `create`); Esc cancels, marks kept.
    recruit: Option<String>,
    /// Pending escape bytes in recruit-overlay mode (split-arrow safety).
    recruit_esc: Vec<u8>,
    /// (x-96e8) The move-a-tab-to-another-squad picker: `(tab captured at open,
    /// candidate squad ids in the numbered order shown)`, `Some` while the
    /// selector `m` overlay is open. A digit sends [`Command::MoveTab`]; the id
    /// is re-validated against the current catalog before it goes on the wire.
    move_pick: Option<(TabId, Vec<u64>)>,
    /// Pending target and geometry for selector `p` placement.
    attach_place: Option<AttachPlace>,
    /// (x-96e8) The squad the selector cursor is tracking across a `J`/`K`
    /// reorder: the next `Layout` re-points the cursor at this squad's row so it
    /// visually follows the moved workspace. Cleared by any non-reorder key or a
    /// selector close.
    sel_follow: Option<u64>,
    /// (x-653d) The session-navigator overlay (leader+f): a global goto picker
    /// over a flat catalog of every squad/tab/agent/card, filtered by typed text
    /// AND by agent state. `Some` while open; stdin diverts to [`nav_keys`].
    /// Client-local like `search` - opening never sends a message and reserves
    /// no row (it draws over the content top-left, not the bottom chrome).
    nav: Option<NavView>,
    /// Pending escape bytes in navigator mode, carried ACROSS reads (same
    /// split-arrow safety as [`View::search_esc`]).
    nav_esc: Vec<u8>,
    /// (x-c376) The read-only peek overlay (Space on a selector agent row),
    /// `Some` while open. Sits ON TOP of the selector; stdin diverts to
    /// [`peek_keys`] BEFORE selector routing. Client-local like `nav` - opening
    /// sends one `PeekAgent` and reserves no row.
    peek: Option<PeekView>,
    /// Pending escape bytes in peek mode (j/k arrow folding), same split-arrow
    /// safety as [`View::sel_esc`].
    peek_esc: Vec<u8>,
    /// (x-c376) Monotonic `PeekAgent` request counter, bumped per open/move so a
    /// body landing after a newer request is dropped by seq (AC1-FR).
    peek_seq: u64,
    /// (x-84d7) The Connections modal (MENU -> connections): a stateful overlay
    /// listing provider accounts + combos, driving the `fno providers` CLI.
    /// `Some` while open; stdin diverts to [`connections_keys`]. Its reads run
    /// off the UI loop via the `conn_*` triad below (the needs-fold idiom).
    connections: Option<crate::connections_view::ConnectionsView>,
    /// Pending escape bytes in connections mode (arrow folding, split-arrow safe).
    conn_esc: Vec<u8>,
    /// (x-84d7) A connections read (list/combos fold) is wanted; the run loop
    /// spawns it at loop top and clears this, keeping the sender out of the deep
    /// stdin handler (the needs_want idiom).
    conn_want: bool,
    /// (x-84d7) A connections read is in flight; bounds concurrent folds to one.
    conn_inflight: bool,
    /// (x-84d7) Generation token, bumped per open/refresh so a read landing after
    /// the modal closed or refreshed again is discarded.
    conn_gen: u64,
    /// (x-84d7) A mutation/login verb wanted by a keypress; the run loop spawns
    /// it at loop top (the sender lives there, out of the stdin handler) and
    /// clears this. `(argv, child-env, is_login)`: `is_login` runs `fno mux pane
    /// run` (opens the login pane, keeps the pending notice), else a single-flight
    /// mutation guarded by `ConnectionsView::acting`.
    conn_action: Option<(Vec<String>, Vec<(String, String)>, bool)>,
}

/// A pending destructive/costly action awaiting the operator's one-keypress
/// confirm. `label` is the entity name shown in the prompt; `action` is what
/// Enter commits (x-a496 dispatch, extended by x-96e8 with squad removal).
struct ConfirmAction {
    action: ConfirmKind,
    label: String,
}

/// What a confirmed [`ConfirmAction`] sends on Enter.
enum ConfirmKind {
    /// Start a targeted session on a work-queue card's node (x-a496).
    Dispatch { node: String },
    /// Close a whole workspace (x-96e8). `panes` is the blast radius named in
    /// the prompt; `last` warns that removing the session's only squad ends it.
    RemoveSquad {
        squad: u64,
        panes: usize,
        last: bool,
    },
    /// Stop a live agent row (x-76ea). The captured `name`, not the row index,
    /// commits - a row that raced out between confirm and Enter resolves to the
    /// server's stale-name refusal.
    StopAgent { name: String },
    /// Remove an exited agent row (x-76ea). Same captured-name commit.
    RemoveAgent { name: String },
    /// Bulk-reap every exited fno-agent registry row (x-7561, uppercase `X`).
    /// No payload - the server's reap verb owns the candidate set.
    ReapAgents,
    /// Stop a live external claude-daemon row by stable `attach_id` (x-7561).
    /// The captured attach id, not the row index, commits; `name` is cosmetic.
    StopExternal { attach_id: String, name: String },
    /// Remove a stopped external tombstone by `attach_id` (x-7561). Same
    /// captured-id commit; the server gates rm on a persisted `stopped` state.
    RemoveExternal { attach_id: String, name: String },
}

/// The entity a rename overlay is editing (x-96e8 widened x-c150's tab-only
/// overlay to also rename a squad): one buffer, one key handler, one esc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameTarget {
    Tab(TabId),
    Squad(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachPlace {
    id: String,
    target: u64,
    squads: Vec<u64>,
    esc: Vec<u8>,
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

/// Client-local session-navigator overlay state (x-653d). The rows are NOT
/// stored here - they are recomputed from the live layout each keypress (the
/// same per-key re-read discipline as the selector/search), so a layout push
/// under an open navigator is reflected at once.
struct NavView {
    /// Incremental text filter (substring, case-insensitive) over row labels.
    query: String,
    /// The active state chip; `None` = all states. `Tab` cycles it.
    state_filter: Option<PaneState>,
    /// Cursor into the CURRENTLY filtered rows (clamped per key, no wrap).
    cursor: usize,
}

/// (x-c376) The read-only peek overlay over a sideline agent row: its full
/// status sentence + recent transcript + (for a blocked row) the x-c929
/// answerable prompt. Opens ON TOP of the selector (which stays open
/// underneath); Esc drops back into it. The row is re-read from the live
/// `display_rows()` per frame (navigator-style), so only the index, the request
/// seq, and the fetched body live here - never a stale row snapshot.
struct PeekView {
    /// A `display_rows()` index, always kept on a `DisplayRow::Agent` row.
    cursor: usize,
    /// The seq of the last `PeekAgent` sent; a `PeekBody` with any other seq is
    /// dropped (A->B->A cycling defeats a name-only guard, AC1-FR).
    seq: u64,
    /// The fetched transcript: `None` = still loading (renders " loading…");
    /// `Some(lines)` = loaded (error/timeout text arrives in-band as lines).
    body: Option<Vec<String>>,
    /// The peeked row's name at fetch time. A layout shift that lands a
    /// DIFFERENT agent on `cursor` refetches instead of redrawing the new
    /// header over the old transcript (codex review): the seq guard covers a
    /// late body under the same request, this covers a changed row identity.
    name: String,
}

/// (x-8ccf US3) The which-key keybinds modal: a centered [`Popup`] built from
/// the single-source leader-chord table, plus the [`Event`] each selectable row
/// runs (`None` for headers, rules, and display-only meta rows). Keeping the
/// events beside the popup lets Enter/click on the SELECTED row dispatch through
/// the exact path a typed chord would, so help can never advertise an action it
/// cannot run (Locked 3).
struct KeysModal {
    popup: Popup,
    row_events: Vec<Option<Event>>,
}

/// Build the modal's rows from [`key_bindings`] (the dispatcher's own table):
/// title, then each section's header + its bindings (key leading, action right),
/// its display-only meta rows, then a footer hint. `row_events` runs parallel to
/// `popup.rows` so a selected row's chord is one lookup away.
fn build_keys_modal() -> KeysModal {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut events: Vec<Option<Event>> = Vec::new();
    let mut add = |row: PopupRow, ev: Option<Event>| {
        rows.push(row);
        events.push(ev);
    };
    add(PopupRow::Header("keybinds  ·  esc close".into()), None);
    let bindings = key_bindings();
    for section in [
        KeySection::Global,
        KeySection::Navigation,
        KeySection::WorkspacesTabs,
        KeySection::Panes,
    ] {
        add(PopupRow::Header(section.title().into()), None);
        for kb in bindings.iter().filter(|kb| kb.section == section) {
            add(
                PopupRow::Entry {
                    glyph: kb.disp.to_string(),
                    label: kb.label.to_string(),
                    hint: String::new(),
                },
                Some(kb.event.clone()),
            );
        }
        // Display-only rows (1-9 select tab, C-b C-b literal): selectable so the
        // reference shows them, but not single-event chords, so Enter BELs.
        for (disp, label, _) in meta_rows().iter().filter(|(_, _, s)| *s == section) {
            add(
                PopupRow::Entry {
                    glyph: (*disp).to_string(),
                    label: (*label).to_string(),
                    hint: String::new(),
                },
                None,
            );
        }
    }
    add(PopupRow::Rule, None);
    add(
        PopupRow::Header("scroll wheel · pgup/pgdn · ⏎/click/tap runs".into()),
        None,
    );
    KeysModal {
        popup: Popup::new(rows, Anchor::Center),
        row_events: events,
    }
}

/// (x-8ccf US2) The right-click / `m` row context menu over a sideline agent
/// row. The target is pinned by NAME (not index) so a layout reshuffle between
/// open and click can only turn an action into a stale-name refusal, never
/// redirect it to a different agent (Concurrency). `actions` runs parallel to
/// the popup's flat targets (`popup.sel` indexes it directly).
struct RowMenu {
    popup: Popup,
    /// The target agent's identity, pinned at open. Names ALONE are ambiguous
    /// (the daemon roster can surface two rows with the same name), so the
    /// pane_id / attach_id disambiguate; execution fails closed if this identity
    /// does not resolve to exactly one live row (never acts on the wrong agent).
    target: AgentIdent,
    actions: Vec<MenuAction>,
}

/// The disambiguating identity of an agent row, captured when a row menu opens.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentIdent {
    name: String,
    pane_id: Option<u64>,
    attach_id: Option<String>,
}

impl AgentIdent {
    fn of(a: &AgentRow) -> Self {
        AgentIdent {
            name: a.name.clone(),
            pane_id: a.pane_id,
            attach_id: a.attach_id.clone(),
        }
    }
    fn matches(&self, a: &AgentRow) -> bool {
        a.name == self.name && a.pane_id == self.pane_id && a.attach_id == self.attach_id
    }
}

/// What a context-menu entry does, resolved against the LIVE agent row (found by
/// name) at execution time - a stale target becomes a Notice, not a wrong action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    /// Attach a bg (paneless) agent by repointing the focused pane (x-9f75).
    OpenHere,
    /// Attach a bg (paneless) agent as a new tab.
    NewTab,
    /// Attach a bg agent as a directional split of the current tab.
    Split(Dir),
    /// Focus an existing pane-hosted row.
    Focus,
    /// Open the read-only peek overlay.
    Peek,
    /// Stop a live row (StopAgent, or StopExternal for a daemon-roster row).
    Stop,
    /// Remove an exited row (RemoveAgent, or RemoveExternal for a roster row).
    Remove,
}

/// Build the per-state row menu for the agent at `display_rows()` index `i`,
/// anchored at `anchor`. `None` for a non-agent row (the menu is agent-only).
/// Entry sets mirror the row's state so no dead item ever renders: a paneless
/// bg row gets the new-tab + 2x2 split grid (its whole point); a pane row gets
/// focus; an exited row gets remove; peek/stop apply where they make sense.
fn build_row_menu(agent: &AgentRow, anchor: Anchor) -> RowMenu {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<MenuAction> = Vec::new();
    let mut add = |row: PopupRow, acts: &[MenuAction]| {
        rows.push(row);
        actions.extend_from_slice(acts);
    };
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
    };
    let cell = |glyph: &str, label: &str| GridCell {
        glyph: glyph.into(),
        label: label.into(),
    };
    add(PopupRow::Header(agent.name.clone()), &[]);
    add(PopupRow::Rule, &[]);
    if agent.exited {
        add(entry("✕", "Remove"), &[MenuAction::Remove]);
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
    } else if agent.pane_id.is_some() {
        // Live pane row: already placed, so focus/peek/stop (no splits).
        add(entry("→", "Focus"), &[MenuAction::Focus]);
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        add(PopupRow::Rule, &[]);
        add(entry("■", "Stop"), &[MenuAction::Stop]);
    } else if agent.attach_id.is_some() {
        // Paneless bg row: the motivating case - open as a tab or a split pane.
        // Open-here leads (repoint the focused viewer); the server's
        // fail-closed notice is the feedback path when the focus isn't a
        // detachable viewer (the client can't know viewer-ness).
        add(
            PopupRow::FullWidth("⊙ Open Here".into()),
            &[MenuAction::OpenHere],
        );
        add(
            PopupRow::FullWidth("▭ New Tab".into()),
            &[MenuAction::NewTab],
        );
        add(PopupRow::Rule, &[]);
        // 2x2 spatial grid: Left/Right on top, Up/Down below (the cell you pick
        // IS the direction). Glyphs are half-block squares; a non-nerd-font
        // terminal still shows the label beside them.
        add(
            PopupRow::Grid(vec![cell("◧", "Split Left"), cell("◨", "Split Right")]),
            &[MenuAction::Split(Dir::Left), MenuAction::Split(Dir::Right)],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Split Up"), cell("⬓", "Split Down")]),
            &[MenuAction::Split(Dir::Up), MenuAction::Split(Dir::Down)],
        );
        add(PopupRow::Rule, &[]);
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        add(entry("■", "Stop"), &[MenuAction::Stop]);
    } else {
        // A live row that is neither pane-hosted nor attachable here.
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        add(entry("■", "Stop"), &[MenuAction::Stop]);
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: AgentIdent::of(agent),
        actions,
    }
}

/// (x-8ccf US4/US5) The sideline MENU popup and the minimal settings modal share
/// this one aux-popup type: a [`Popup`] plus the [`AuxAction`] each selectable
/// row runs. The two chain (MENU -> settings) by swapping the `aux` slot.
struct AuxPopup {
    popup: Popup,
    actions: Vec<AuxAction>,
}

/// What a MENU / settings-modal row does. Menu entries open a surface or detach;
/// settings entries flip a session-local view toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuxAction {
    OpenKeybinds,
    OpenSettings,
    OpenConnections,
    Detach,
    ToggleHoverFocus,
    ToggleStatus,
}

/// Build the sideline MENU popup (US4), anchored at the footer's menu cell:
/// keybinds / settings / detach. `reload config` is intentionally absent - there
/// is no config-reload machinery to route it to (a net-new capability, not a
/// re-route), so the menu advertises only what actually works.
fn build_sideline_menu(anchor: Anchor) -> AuxPopup {
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
    };
    AuxPopup {
        popup: Popup::new(
            vec![
                PopupRow::Header("menu".into()),
                PopupRow::Rule,
                entry("⌨", "keybinds"),
                entry("⚙", "settings"),
                entry("⇄", "connections"),
                entry("⏏", "detach"),
            ],
            anchor,
        ),
        actions: vec![
            AuxAction::OpenKeybinds,
            AuxAction::OpenSettings,
            AuxAction::OpenConnections,
            AuxAction::Detach,
        ],
    }
}

impl View {
    fn new(term: (u16, u16), session: String, layout: LayoutView) -> Self {
        // Seed with the active squad so the first frame already shows its tabs
        // (and the focused tab's `*` marker) without any keypress (x-2f99).
        let expanded = HashSet::from([layout.active_squad]);
        View {
            term,
            session,
            layout,
            frames: HashMap::new(),
            panel_on: true,
            status_on: true,
            hint: false,
            keys_modal: None,
            keys_modal_esc: Vec::new(),
            row_menu: None,
            row_menu_esc: Vec::new(),
            aux: None,
            aux_esc: Vec::new(),
            expanded,
            selector: None,
            sel_esc: Vec::new(),
            sideline_offset: 0,
            answers: None,
            ans_esc: Vec::new(),
            needs_fold: None,
            needs_fold_at: None,
            needs_degraded: false,
            needs_want: false,
            needs_inflight: false,
            needs_gen: 0,
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
            rename: None,
            rename_esc: Vec::new(),
            marks: std::collections::HashSet::new(),
            recruit: None,
            recruit_esc: Vec::new(),
            move_pick: None,
            attach_place: None,
            sel_follow: None,
            nav: None,
            nav_esc: Vec::new(),
            peek: None,
            peek_esc: Vec::new(),
            peek_seq: 0,
            connections: None,
            conn_esc: Vec::new(),
            conn_want: false,
            conn_inflight: false,
            conn_gen: 0,
            conn_action: None,
        }
    }

    /// (x-84d7) Open the Connections modal in its loading state and arm the first
    /// read. Bumps the gen so any in-flight read from a prior open is discarded.
    fn open_connections(&mut self) {
        self.conn_gen = self.conn_gen.wrapping_add(1);
        let mut cv = crate::connections_view::ConnectionsView::new();
        cv.gen = self.conn_gen;
        self.connections = Some(cv);
        self.conn_esc.clear();
        self.conn_want = true; // the run loop spawns the fold at loop top
    }

    /// (x-84d7) Close the modal and bump the gen so a late read is dropped.
    fn close_connections(&mut self) {
        self.connections = None;
        self.conn_esc.clear();
        self.conn_gen = self.conn_gen.wrapping_add(1);
    }

    /// (x-84d7) Arm a fresh read (R refresh) under a new gen.
    fn refresh_connections(&mut self) {
        self.conn_gen = self.conn_gen.wrapping_add(1);
        if let Some(cv) = self.connections.as_mut() {
            cv.gen = self.conn_gen;
            cv.state = crate::connections_view::ModalState::Loading;
            cv.notice = None;
            // NB: do NOT clear `acting` here. A manual R during an in-flight
            // mutation must keep the single-flight guard up until the subprocess
            // actually exits, or a second use/register/update could overlap the
            // first and race two config/credential writes. The action-result arm
            // clears `acting` unconditionally on completion, so R can't wedge it.
        }
        self.conn_want = true;
    }

    /// (x-84d7) Re-read after a mutation: keep the current lists + the result
    /// notice visible (no Loading blank) while the fresh data folds in. This is
    /// the read-after-write that keeps the modal from trusting optimistic state.
    fn rearm_connections_read(&mut self) {
        self.conn_gen = self.conn_gen.wrapping_add(1);
        if let Some(cv) = self.connections.as_mut() {
            cv.gen = self.conn_gen;
        }
        self.conn_want = true;
    }

    /// The unified needs-me queue (x-feec), worst-first: the live badge leg
    /// (this session's blocked / done-unseen rows, instant from the layout)
    /// merged with the event-fold leg (`review_wedged` / `budget_stop`), each
    /// fold item joined to a roster row when one exists. Owned rows so a per-key
    /// mutation of `answers` never aliases the borrow (the reason the old
    /// blocked_queue cloned too). Sorted `(kind, ts, name)`.
    fn needs_queue(&self) -> Vec<NeedRow> {
        let mut rows: Vec<NeedRow> = Vec::new();

        // Leg 1: live badge rows from the current layout (no shell-out).
        for a in &self.layout.agents {
            if is_blocked_row(a) {
                let kind = if a.answerable.is_some() {
                    NeedKind::BlockedAnswerable
                } else {
                    NeedKind::BlockedFocusOnly
                };
                let reason = a.reason.clone().unwrap_or_else(|| {
                    if a.answerable.is_some() {
                        "needs an answer".into()
                    } else {
                        "needs focus".into()
                    }
                });
                rows.push(NeedRow {
                    kind,
                    name: a.name.clone(),
                    reason,
                    ts: String::new(),
                    id_key: a.name.clone(),
                    answerable: a.answerable.clone(),
                    pane_id: a.pane_id,
                    attach_id: a.attach_id.clone(),
                    squad: a.squad,
                    tab: a.tab,
                });
            } else if !a.exited && pane_state(a.badge, a.seen) == PaneState::DoneUnseen {
                rows.push(NeedRow {
                    kind: NeedKind::DoneUnseen,
                    name: a.name.clone(),
                    reason: a.reason.clone().unwrap_or_else(|| "done, unseen".into()),
                    ts: String::new(),
                    id_key: a.name.clone(),
                    answerable: None,
                    pane_id: a.pane_id,
                    attach_id: a.attach_id.clone(),
                    squad: a.squad,
                    tab: a.tab,
                });
            }
        }

        // Leg 2: event-fold items, joined to a roster row, else rendered
        // squadless when live, else dropped (a dead session's stale stop must
        // not nag forever - Locked 5).
        if let Some(items) = &self.needs_fold {
            for item in items {
                let kind = match item.kind.as_str() {
                    "review_wedged" => NeedKind::ReviewWedged,
                    "budget_stop" => NeedKind::BudgetStop,
                    _ => continue,
                };
                match self.join_fold_row(item) {
                    Some(a) => rows.push(NeedRow {
                        kind,
                        name: a.name.clone(),
                        reason: item.evidence.clone(),
                        ts: item.ts.clone(),
                        id_key: item.session_id.clone(),
                        answerable: None,
                        pane_id: a.pane_id,
                        attach_id: a.attach_id.clone(),
                        squad: a.squad,
                        tab: a.tab,
                    }),
                    None if item.live => rows.push(NeedRow {
                        kind,
                        name: item
                            .name
                            .clone()
                            .or_else(|| item.node.clone())
                            .unwrap_or_else(|| item.session_id.clone()),
                        reason: item.evidence.clone(),
                        ts: item.ts.clone(),
                        id_key: item.session_id.clone(),
                        answerable: None,
                        pane_id: None,
                        attach_id: None,
                        squad: None,
                        tab: None,
                    }),
                    None => {} // unjoined + not live: drop (stale-nag guard)
                }
            }
        }

        rows.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.ts.cmp(&b.ts))
                .then_with(|| a.name.cmp(&b.name))
        });
        rows
    }

    /// The roster row a fold item joins to: a name / node / session-id match
    /// against a layout row's name or cwd basename (the identity a sideline
    /// orphan row carries).
    fn join_fold_row(&self, item: &crate::needs_overlay::FoldItem) -> Option<&AgentRow> {
        let keys: Vec<&str> = [
            item.name.as_deref(),
            item.node.as_deref(),
            Some(item.session_id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.layout.agents.iter().find(|a| {
            keys.iter().any(|k| a.name == *k)
                || a.cwd_base.as_deref().is_some_and(|c| keys.contains(&c))
        })
    }

    /// The capped, sorted queue actually rendered and indexed, plus the count of
    /// rows the worst-first cap dropped (for the footer). Both the overlay draw
    /// and the key handler read this, so cursor index and rendered rows never
    /// diverge.
    fn needs_view(&self) -> (Vec<NeedRow>, usize) {
        let mut rows = self.needs_queue();
        let dropped = rows.len().saturating_sub(NEEDS_CAP);
        rows.truncate(NEEDS_CAP);
        (rows, dropped)
    }

    /// The overlay footer state: a failed fold degrades loudly (AC2-ERR), an
    /// unfetched fold reads as still folding, else it has landed.
    fn needs_footer(&self) -> NeedsFooter {
        if self.needs_degraded {
            NeedsFooter::Degraded
        } else if self.needs_fold.is_none() {
            NeedsFooter::Folding
        } else {
            NeedsFooter::AsOf
        }
    }

    /// The identity of the currently-selected needs row, for re-anchoring the
    /// cursor across a layout push or fold merge (AC3-UI).
    fn answers_selected_id(&self) -> Option<(NeedKind, String)> {
        let cur = self.answers?;
        let (rows, _) = self.needs_view();
        rows.get(cur).map(NeedRow::id)
    }

    /// Re-anchor the answer cursor to `prev` (its item identity) after the queue
    /// recomputed: keep it on the same item if still present, else clamp. The
    /// overlay stays open on an empty queue (the "nothing needs you" state,
    /// AC4-EDGE) with the cursor clamped to 0 so a later merge lands cleanly.
    fn reanchor_answers(&mut self, prev: Option<(NeedKind, String)>) {
        if self.answers.is_none() {
            return;
        }
        let (rows, _) = self.needs_view();
        if rows.is_empty() {
            self.answers = Some(0);
            return;
        }
        let idx = prev
            .and_then(|(k, n)| rows.iter().position(|r| r.kind == k && r.name == n))
            .or(self.answers)
            .unwrap_or(0)
            .min(rows.len() - 1);
        self.answers = Some(idx);
    }

    /// Open the new-workspace name overlay modally (x-9e5e): clear any
    /// keyboard-opened overlay first. `create_keys` is routed AFTER
    /// selector/answers in `handle_stdin`, so a lingering selector would
    /// otherwise swallow the typed name (codex peer review).
    fn open_create(&mut self) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.rename = None;
        self.move_pick = None;
        self.attach_place = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.create = Some(String::new());
        self.create_esc.clear();
    }

    /// Open the recruit workspace-name overlay modally (x-8f11), clearing other
    /// keyboard-opened overlays first (x-260a). The marks are NOT cleared - they
    /// are the payload; Esc keeps them, a submit clears them.
    fn open_recruit(&mut self) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.rename = None;
        self.create = None;
        self.move_pick = None;
        self.attach_place = None;
        self.nav = None;
        self.confirm = None;
        self.clear_peek();
        self.recruit = Some(String::new());
        self.recruit_esc.clear();
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
        // Same for a half-typed rename (x-c150): the confirm must not hide a
        // live text-input overlay whose next Enter it would steal.
        self.rename = None;
        self.rename_esc.clear();
        self.move_pick = None;
        self.attach_place = None;
        // A live navigator overlay (x-653d) must not linger behind the confirm:
        // it wins stdin routing after the confirm resolves and would swallow the
        // next keys, same reasoning as the selector above.
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.confirm = Some(action);
    }

    /// Open the rename overlay modally for `target` (x-c150 tab, widened x-96e8
    /// to a squad), clearing any other keyboard-opened overlay first - the same
    /// discipline as [`View::open_create`] (a lingering selector would swallow
    /// the name).
    fn open_rename(&mut self, target: RenameTarget) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.move_pick = None;
        self.attach_place = None;
        self.create = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.rename = Some((target, String::new()));
        self.rename_esc.clear();
    }

    /// Open the move-tab-to-squad picker modally for `tab` (x-96e8), listing the
    /// candidate destination squads (source excluded, capped at 9) in the order
    /// a digit selects them. Same overlay-clearing discipline as the others.
    fn open_move_pick(&mut self, tab: TabId, squads: Vec<u64>) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.create = None;
        self.rename = None;
        self.confirm = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.attach_place = None;
        self.clear_peek();
        self.move_pick = Some((tab, squads));
    }

    fn open_attach_place(&mut self, id: String, target: u64, squads: Vec<u64>) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.create = None;
        self.rename = None;
        self.confirm = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.move_pick = None;
        self.clear_peek();
        self.attach_place = Some(AttachPlace {
            id,
            target,
            squads,
            esc: Vec::new(),
        });
    }

    /// Clear the read-only peek overlay (x-c376) and its escape carry. Called by
    /// every modal `open_*` helper so a mouse-driven overlay open (the mouse
    /// pre-pass runs before overlay routing) never leaves peek rendering on top.
    fn clear_peek(&mut self) {
        self.peek = None;
        self.peek_esc.clear();
    }

    /// Open the which-key keybinds modal (leader+?, x-8ccf US3). Clears peek like
    /// every other overlay open so a mouse-driven open never leaves peek on top.
    fn open_keys_modal(&mut self) {
        self.clear_peek();
        self.keys_modal = Some(build_keys_modal());
        self.keys_modal_esc.clear();
    }

    /// The flat popup target under a screen cell while the modal is open, for
    /// mouse hover/click. Renders the modal (windowed by the live scroll) and
    /// walks the visible line's hit spans; `None` off the popup.
    fn keys_modal_hit(&self, row: u16, col: u16) -> Option<usize> {
        let m = self.keys_modal.as_ref()?;
        let r = m.popup.render(self.term);
        let (r0, c0) = r.origin;
        let li = (row as usize).checked_sub(r0)?;
        let line = r.lines.get(li)?;
        let cc = (col as usize).checked_sub(c0)?;
        line.hits
            .iter()
            .find(|(_, off, len)| cc >= *off && cc < *off + *len)
            .map(|(t, _, _)| *t)
    }

    /// Keep the selected modal row inside the scrolled viewport after an arrow
    /// move (the block is one line per row, so the row index IS the line index).
    fn follow_modal_selection(&mut self) {
        let trows = self.term.0.max(1) as usize;
        if let Some(m) = self.keys_modal.as_mut() {
            let vis_h = m.popup.rows.len().min(trows);
            if let Some((ri, _)) = m.popup.selected() {
                if ri < m.popup.scroll {
                    m.popup.scroll = ri;
                } else if ri >= m.popup.scroll + vis_h {
                    m.popup.scroll = ri + 1 - vis_h;
                }
            }
        }
    }

    /// Open the row context menu on `display_rows()` index `i`, anchored at
    /// `anchor` (x-8ccf US2). Returns whether it opened - `false` for a non-agent
    /// row (the caller then leaves the press alone / forwards it).
    fn open_row_menu(&mut self, i: usize, anchor: Anchor) -> bool {
        let agent = match self.display_rows().get(i) {
            Some(DisplayRow::Agent(a)) => (*a).clone(),
            _ => return false,
        };
        self.clear_peek();
        self.row_menu = Some(build_row_menu(&agent, anchor));
        self.row_menu_esc.clear();
        true
    }

    /// The flat popup target under a screen cell while the row menu is open, for
    /// mouse hover/click; `None` off the popup.
    fn row_menu_hit(&self, row: u16, col: u16) -> Option<usize> {
        let m = self.row_menu.as_ref()?;
        let r = m.popup.render(self.term);
        let (r0, c0) = r.origin;
        let li = (row as usize).checked_sub(r0)?;
        let line = r.lines.get(li)?;
        let cc = (col as usize).checked_sub(c0)?;
        line.hits
            .iter()
            .find(|(_, off, len)| cc >= *off && cc < *off + *len)
            .map(|(t, _, _)| *t)
    }

    /// Open the sideline MENU popup anchored at `anchor` (x-8ccf US4).
    fn open_sideline_menu(&mut self, anchor: Anchor) {
        self.clear_peek();
        self.aux = Some(build_sideline_menu(anchor));
        self.aux_esc.clear();
    }

    /// Build the minimal settings modal (x-8ccf US5): 2 session-only toggles that
    /// live-apply to this session. Persistence to config.toml is out of scope for
    /// v1, so each row is honestly labeled "session only" rather than pretending
    /// it persisted (the modal must never claim persistence it did not achieve).
    fn build_settings_modal(&self) -> AuxPopup {
        let toggle = |on: bool, label: &str| PopupRow::Entry {
            glyph: if on { "☑".into() } else { "☐".into() },
            label: label.into(),
            hint: "session only".into(),
        };
        AuxPopup {
            popup: Popup::new(
                vec![
                    PopupRow::Header("settings".into()),
                    PopupRow::Rule,
                    toggle(self.hover_focus, "focus follows mouse"),
                    toggle(self.status_on, "status row"),
                ],
                Anchor::Center,
            ),
            actions: vec![AuxAction::ToggleHoverFocus, AuxAction::ToggleStatus],
        }
    }

    /// Rebuild the settings modal after a toggle so its glyph reflects the new
    /// state, preserving the current selection (a keyboard toggle must re-toggle
    /// the SAME row on the next Enter, not reset to row 0).
    fn reopen_settings_keeping_sel(&mut self) {
        let sel = self.aux.as_ref().map(|m| m.popup.sel).unwrap_or(0);
        let mut modal = self.build_settings_modal();
        let n = modal.popup.targets().len();
        modal.popup.sel = if n > 0 { sel.min(n - 1) } else { 0 };
        self.aux = Some(modal);
    }

    /// The flat popup target under a screen cell while an aux popup is open.
    fn aux_hit(&self, row: u16, col: u16) -> Option<usize> {
        let m = self.aux.as_ref()?;
        let r = m.popup.render(self.term);
        let (r0, c0) = r.origin;
        let li = (row as usize).checked_sub(r0)?;
        let line = r.lines.get(li)?;
        let cc = (col as usize).checked_sub(c0)?;
        line.hits
            .iter()
            .find(|(_, off, len)| cc >= *off && cc < *off + *len)
            .map(|(t, _, _)| *t)
    }

    /// Apply a `PeekBody` under the seq guard (x-c376, AC1-FR): store `lines`
    /// only when peek is open AND `seq` is the current request. Returns whether
    /// it applied (the caller redraws on true). A stale body (any other seq) is
    /// dropped, so a peek moved on to another row never shows the prior row's
    /// transcript.
    fn apply_peek_body(&mut self, seq: u64, lines: Vec<String>) -> bool {
        match self.peek.as_mut().filter(|p| p.seq == seq) {
            Some(peek) => {
                peek.body = Some(lines);
                true
            }
            None => false,
        }
    }

    /// Open the read-only peek overlay on `cursor` (x-c376), a `display_rows()`
    /// index the caller verified is a `DisplayRow::Agent`. Bumps the request seq
    /// and starts in the loading state; the caller sends the matching
    /// `PeekAgent` with the returned seq. Deliberately unlike the modal `open_*`
    /// helpers: the selector stays open UNDERNEATH so Esc drops back into it.
    fn open_peek(&mut self, cursor: usize, name: String) -> u64 {
        self.peek_seq = self.peek_seq.wrapping_add(1);
        self.peek = Some(PeekView {
            cursor,
            seq: self.peek_seq,
            body: None,
            name,
        });
        self.peek_esc.clear();
        self.peek_seq
    }

    /// The `display_rows()` index of squad `id`'s own row (a `Sel` with no tab),
    /// or `None` if it is not currently a visible row. Used to re-point the
    /// selector cursor onto a squad after a `J`/`K` reorder (x-96e8).
    fn squad_row(&self, id: u64) -> Option<usize> {
        self.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Sel(s) if s.tab.is_none() && s.squad == id))
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

    /// Content viewport as `(origin, dims)` in `usize`, for centering a
    /// [`draw_lines_overlay`] popover against the content rect (right of the
    /// sideline, above any splits) instead of the outer terminal. One call
    /// site for every corner-anchored popover (x-e9c3).
    fn overlay_viewport(&self) -> ((usize, usize), (usize, usize)) {
        let (rows, cols) = self.content_dims();
        (
            (TAB_BAR_ROWS as usize, self.panel_w() as usize),
            (rows as usize, cols as usize),
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

    /// The column range of the footer's `☰ menu` button (x-8ccf US4), shared by
    /// the renderer and the hit-test so a click lands where it draws. `None` when
    /// the panel is too narrow to add the button beside the `+ new workspace`
    /// affordance, or a recruit-mark tally is competing for the row.
    fn footer_menu_range(&self, panel_w: usize) -> Option<std::ops::Range<usize>> {
        let tw = panel_w.saturating_sub(1); // last column is the divider
        let mw = FOOTER_MENU.chars().count();
        (self.marks.is_empty() && tw >= FOOTER_NEW_LABEL.len() + 2 + mw).then(|| (tw - mw)..tw)
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
            if let Some((start, text)) = self.notice_overlay(self.term.1 as usize) {
                if col >= start && col < start + text.chars().count() {
                    return None;
                }
            }
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
        // Display row i is painted at TAB_BAR_ROWS + (i - sideline_offset)
        // (draw_sideline), so invert with the offset - else a click on a scrolled
        // row activates the wrong unscrolled row (codex P2). Mirrors sideline_row_at.
        let i = (row - TAB_BAR_ROWS) as usize + self.sideline_offset;
        // x-8ccf US4: a click on the footer's `☰ menu` region opens the sideline
        // MENU popup; the rest of the footer row keeps its `+ new` create action.
        if matches!(self.display_rows().get(i), Some(DisplayRow::NewSquad)) {
            if let Some(range) = self.footer_menu_range(panel_w as usize) {
                if range.contains(&(col as usize)) {
                    return Some(ChromeHit::OpenSidelineMenu { row, col });
                }
            }
        }
        self.row_action(i)
    }

    /// What acting on sideline display row `i` does - the single resolver both
    /// a mouse click ([`View::chrome_hit`]) and the leader+w selector's Enter
    /// route through (x-260a), so the two inputs can never diverge. `None` only
    /// for an out-of-range index or an inert [`DisplayRow::Header`].
    fn row_action(&self, i: usize) -> Option<ChromeHit> {
        match self.display_rows().get(i)? {
            DisplayRow::Sel(row) => match row.tab {
                // Acting on the already-active squad row was a silent no-op
                // (SelectSquad to the squad you're on); it now toggles the
                // caret locally instead (x-2f99). Inactive rows keep
                // SelectSquad - auto-expand in set_layout completes the
                // gesture when the resulting layout push lands.
                None if row.squad == self.layout.active_squad => {
                    Some(ChromeHit::ToggleExpand(row.squad))
                }
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
            // A pane-hosted agent focuses its pane; a paneless claude bg row
            // attaches; a non-attachable row says so. Resolved by [`agent_hit`],
            // shared with the navigator's goto so a click and a keyboard jump
            // never diverge on what an agent's action is (x-653d).
            DisplayRow::Agent(a) => Some(agent_hit(a, self.layout.active_squad)),
            // A work-queue card dispatches/focuses via [`View::card_hit`], the
            // same resolver the navigator uses (x-653d).
            DisplayRow::Card(c) => Some(self.card_hit(c)),
            DisplayRow::Header(_) => None,
            // The `+` footer opens the name-input overlay (x-9e5e).
            DisplayRow::NewSquad if self.term.0 < MIN_ROWS_FOR_STATUS => Some(ChromeHit::Notice(
                "terminal too short for the name prompt".into(),
            )),
            DisplayRow::NewSquad => Some(ChromeHit::OpenCreate),
        }
    }

    /// The [`ChromeHit`] for one work-queue card - the resolver shared by a
    /// sideline click ([`View::row_action`]) and the navigator's goto
    /// ([`View::nav_rows`], x-653d). A method (not a free fn like [`agent_hit`])
    /// because the Ready confirm needs the term-height guard.
    ///
    /// Only a READY card starts a session (x-a496) - the same nodes leader+g
    /// picks - and only behind a one-keypress confirm (too costly for a stray
    /// tap). A blocked/in-flight card is work leader+g never selects, so it says
    /// why or routes to the running session (x-54fa, priority pane > attach >
    /// notice) rather than opening the confirm.
    fn card_hit(&self, c: &BacklogCard) -> ChromeHit {
        match c.state {
            // A terminal too short to render the bottom-row prompt refuses
            // instead of arming an INVISIBLE confirm that would capture keys and
            // could dispatch blind (sigma review x-260a).
            CardState::Ready if self.term.0 < MIN_ROWS_FOR_STATUS => {
                ChromeHit::Notice("terminal too short for the dispatch prompt".into())
            }
            CardState::Ready => ChromeHit::Confirm(ConfirmAction {
                action: ConfirmKind::Dispatch { node: c.id.clone() },
                label: if c.slug.is_empty() {
                    c.id.clone()
                } else {
                    c.slug.clone()
                },
            }),
            CardState::Blocked => ChromeHit::Notice("card blocked - unmet deps".into()),
            CardState::InFlight => match (c.pane_id, &c.attach_id) {
                (Some(pid), _) => ChromeHit::Cmds(vec![Command::FocusPane(pid)]),
                (None, Some(id)) => ChromeHit::Cmds(vec![Command::attach_agent(id)]),
                (None, None) => ChromeHit::Notice(
                    c.where_hint
                        .clone()
                        .unwrap_or_else(|| "card in flight - no session visible here".into()),
                ),
            },
        }
    }

    /// The navigator's flat GLOBAL catalog (x-653d): one [`NavRow`] per squad,
    /// per tab (ignoring expand state - a collapsed squad's tabs still appear,
    /// the key difference from [`display_rows`]), per plain pane (v22: those NOT
    /// already shown as an agent row), per agent, and per work-queue card across
    /// the WHOLE session. Shares the agent/card -> [`ChromeHit`]
    /// mapping with [`row_action`] (via [`agent_hit`]/[`card_hit`]) so a keyboard
    /// goto and a mouse click never diverge. Squad/tab rows carry their own
    /// SelectSquad/SelectTab in `hit`; an agent row carries a `goto_squad`
    /// prefix (its pane lives in another squad). The `+ new workspace` footer is
    /// omitted - the navigator is a goto-existing picker (Discretion 4). Fully
    /// owned (no layout borrow) so goto can mutate the view after building it.
    fn nav_rows(&self) -> Vec<NavRow> {
        let mut out = Vec::new();
        let cross = |sq: u64| (sq != self.layout.active_squad).then_some(sq);
        for s in &self.layout.squads {
            // Always SelectSquad (unlike the sideline's active-squad
            // ToggleExpand): the navigator is a jump, never an expand toggle.
            out.push(NavRow {
                label: s.name.clone(),
                state: PaneState::Idle,
                goto_squad: None,
                goto_tab: None,
                hit: ChromeHit::Cmds(vec![Command::SelectSquad(s.id)]),
            });
            for (t, tab) in s.tabs.iter().enumerate() {
                let tab_text = tab_label_text(&tab.name, t);
                out.push(NavRow {
                    label: format!("{} › {}", s.name, tab_text),
                    state: PaneState::Idle,
                    // SelectTab resolves the squad server-side, so one command
                    // switches squad+tab (row_action's tab arm, gemini review).
                    goto_squad: None,
                    goto_tab: None,
                    hit: ChromeHit::Cmds(vec![Command::SelectTab(tab.id)]),
                });
                // Plain panes of the tab (v22): a pane already shown as an agent
                // row is skipped (the agent row is the richer view of the same
                // pane); the rest become goto-able so a bare shell pane in any
                // tab/squad is reachable, not just the active view (codex review).
                for p in &tab.panes {
                    if self.layout.agents.iter().any(|a| a.pane_id == Some(p.id)) {
                        continue;
                    }
                    out.push(NavRow {
                        label: format!("{} › {} › {}", s.name, tab_text, p.label),
                        state: PaneState::Idle,
                        goto_squad: cross(s.id),
                        goto_tab: Some(tab.id),
                        hit: ChromeHit::Cmds(vec![Command::FocusPane(p.id)]),
                    });
                }
            }
            for a in self.layout.agents.iter().filter(|a| a.squad == Some(s.id)) {
                // (x-0090) A pane-hosted agent names its tab with a `·N` ordinal,
                // coherent with the sideline; a watch-only row has no tab.
                let label = match a.tab.and_then(|tid| self.tab_ordinal(a.squad, tid)) {
                    Some(n) => format!("{} › {} ·{n}", s.name, a.name),
                    None => format!("{} › {}", s.name, a.name),
                };
                out.push(NavRow {
                    label,
                    state: nav_agent_state(a),
                    // Switch to the agent's squad first when it is not active, so
                    // the following FocusPane lands there (the server resolves the
                    // pane's tab on focus; the ordinal is display-only).
                    goto_squad: cross(s.id),
                    goto_tab: None,
                    hit: agent_hit(a, self.layout.active_squad),
                });
            }
        }
        // Orphan agents (no live squad), mirroring display_rows' orphan section.
        for a in self.layout.agents.iter().filter(
            |a| !matches!(a.squad, Some(id) if self.layout.squads.iter().any(|s| s.id == id)),
        ) {
            out.push(NavRow {
                label: a.name.clone(),
                state: nav_agent_state(a),
                goto_squad: None,
                goto_tab: None,
                hit: agent_hit(a, self.layout.active_squad),
            });
        }
        // Work-queue cards: goto opens the dispatch confirm / focuses the worker
        // (card_hit), no squad switch. A blocked/in-flight card reads as
        // Blocked/Working so the state filter surfaces stuck work uniformly.
        for c in &self.layout.backlog {
            let label = if c.slug.is_empty() { &c.id } else { &c.slug };
            out.push(NavRow {
                label: format!("{label} {}", c.priority),
                state: card_state(c),
                goto_squad: None,
                goto_tab: None,
                hit: self.card_hit(c),
            });
        }
        out
    }

    /// The navigator rows matching the current text + state filter (x-653d),
    /// recomputed per keypress (no cache): case-insensitive substring on the
    /// label AND the state chip when one is set. Text and state compose (both
    /// must match); letters only ever edit the query (Locked 5).
    fn nav_filtered(&self, nav: &NavView) -> Vec<NavRow> {
        let q = nav.query.to_lowercase();
        self.nav_rows()
            .into_iter()
            .filter(|r| nav.state_filter.is_none_or(|s| r.state == s))
            .filter(|r| q.is_empty() || r.label.to_lowercase().contains(&q))
            .collect()
    }

    /// Move the navigator cursor by `delta`, clamped to the filtered row count
    /// (no wrap). Rows are recomputed to know the current ceiling.
    fn nav_move_cursor(&mut self, delta: isize) {
        let len = match self.nav.as_ref() {
            Some(n) => self.nav_filtered(n).len(),
            None => return,
        };
        if len == 0 {
            return;
        }
        if let Some(n) = self.nav.as_mut() {
            let cur = n.cursor.min(len - 1) as isize;
            n.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
        }
    }

    /// Advance the state chip on `Tab`: all -> Blocked -> Working -> DoneUnseen
    /// -> Idle -> all. Resets the cursor to the top of the re-filtered set.
    fn nav_cycle_state(&mut self) {
        if let Some(n) = self.nav.as_mut() {
            n.state_filter = match n.state_filter {
                None => Some(PaneState::Blocked),
                Some(PaneState::Blocked) => Some(PaneState::Working),
                Some(PaneState::Working) => Some(PaneState::DoneUnseen),
                Some(PaneState::DoneUnseen) => Some(PaneState::Idle),
                Some(PaneState::Idle) => None,
            };
            n.cursor = 0;
        }
    }

    /// Reverse the state chip on `Shift-Tab`: all -> Idle -> DoneUnseen ->
    /// Working -> Blocked -> all (the exact reverse of [`nav_cycle_state`]).
    /// Resets the cursor to the top of the re-filtered set.
    fn nav_cycle_state_rev(&mut self) {
        if let Some(n) = self.nav.as_mut() {
            n.state_filter = match n.state_filter {
                None => Some(PaneState::Idle),
                Some(PaneState::Idle) => Some(PaneState::DoneUnseen),
                Some(PaneState::DoneUnseen) => Some(PaneState::Working),
                Some(PaneState::Working) => Some(PaneState::Blocked),
                Some(PaneState::Blocked) => None,
            };
            n.cursor = 0;
        }
    }

    /// BEL when the current filter excludes every row (AC2-ERR/AC3-ERR): a query
    /// or state that matches nothing is audible, never a silent empty overlay.
    fn nav_ring_if_empty(&self) {
        if let Some(n) = self.nav.as_ref() {
            if self.nav_filtered(n).is_empty() {
                let _ = raw_out(b"\x07");
            }
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
        let i = (row - TAB_BAR_ROWS) as usize + self.sideline_offset;
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
        // Auto-expand the newly active squad so focus is always visible - but
        // only on an `active_squad` CHANGE: layout pushes arrive on every
        // scrape tick, so an unconditional insert would fight manual collapse
        // (x-2f99, AC1-EDGE). Insert-only: activation never collapses others.
        if layout.active_squad != self.layout.active_squad {
            self.expanded.insert(layout.active_squad);
        }
        // Prune ids whose squad vanished server-side, so the set only ever
        // holds live squads (bounded leak otherwise).
        self.expanded
            .retain(|id| layout.squads.iter().any(|s| s.id == *id));
        // Capture the selected needs-row identity against the OLD layout, before
        // the swap, so the cursor can re-anchor to the same item afterward.
        let needs_prev = self.answers_selected_id();
        self.layout = layout;
        // Selector re-anchors to a live, actionable row on catalog change
        // (AC6-FR): clamp into the unified rows, then step off an inert Header
        // so the cursor never rests on a label (x-260a). A pending J/K reorder
        // (x-96e8) instead re-points the cursor at the moved squad's new row so
        // it visually follows the workspace; the follow persists across repeated
        // presses until a non-reorder key clears sel_follow.
        if self.selector.is_some() {
            let anchored = match self.sel_follow.and_then(|sq| self.squad_row(sq)) {
                Some(row) => Some(row),
                None => self.selector.and_then(|cur| self.selector_anchor(cur)),
            };
            self.selector = anchored;
        }
        // Needs-me overlay re-anchors to the SAME item across a scrape tick
        // (x-feec AC3-UI): a resolved row drops out, the queue re-sorts, and the
        // cursor stays on the item it was on (by identity, not index), clamped
        // in range. An emptied queue keeps the overlay open in its "nothing
        // needs you" state (AC4-EDGE) rather than closing under the user.
        self.reanchor_answers(needs_prev);
        // Navigator re-clamps its cursor when a scrape tick reorders/removes rows
        // under it (x-653d, AC1-FR/AC2-EDGE): the rows recompute from self.layout
        // on every access, so after a push a past-the-end cursor would draw no
        // marker and mis-target Enter. Clamp, don't reset - the query and state
        // filter are unchanged, only the underlying catalog moved; resetting to 0
        // on every tick would fight a live badge update (matching the selector's
        // clamp-don't-jump discipline above).
        let nav_count = self.nav.as_ref().map(|n| self.nav_filtered(n).len());
        if let (Some(count), Some(nav)) = (nav_count, self.nav.as_mut()) {
            nav.cursor = count.saturating_sub(1).min(nav.cursor);
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
        // Re-clamp the sideline scroll offset against the new catalog so a
        // shrunk row set never leaves the offset past the last row (x-a621).
        self.clamp_sideline_offset();
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

    /// Flip a squad's caret expansion (x-2f99): pure client state, always
    /// visible next frame (the caret glyph and tab rows change together).
    fn toggle_expand(&mut self, id: u64) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        // Collapsing shrinks the row set; re-clamp so a scrolled sideline never
        // skips past the new last row (x-a621).
        self.clamp_sideline_offset();
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

    /// The nearest `DisplayRow::Agent` index past `from` in `dir` (+1 down, -1
    /// up), skipping every non-agent row (x-c376 j/k peek). `None` when there is
    /// no agent row that way (the caller BELs and stays put). Re-reads the live
    /// catalog per call, so a scrape tick between keys never chases a stale row.
    fn peek_next_agent(&self, from: usize, dir: isize) -> Option<usize> {
        let rows = self.display_rows();
        let mut i = from as isize + dir;
        while i >= 0 && (i as usize) < rows.len() {
            if matches!(rows[i as usize], DisplayRow::Agent(_)) {
                return Some(i as usize);
            }
            i += dir;
        }
        None
    }

    /// Re-anchor or close the peek overlay after a catalog change (x-c376): if
    /// the peeked index no longer lands on an agent row, snap to the nearest
    /// agent row (down first, then up); close peek when none remain. Returns the
    /// name to re-fetch when it re-anchored, `None` when it held or closed.
    fn peek_reanchor(&mut self) -> Option<(usize, String)> {
        let Some((cursor, peeked)) = self.peek.as_ref().map(|p| (p.cursor, p.name.clone())) else {
            return None;
        };
        // One `display_rows()` snapshot for the whole check: the identity test,
        // both direction scans, and the re-anchored name all read it (gemini
        // review).
        let rows = self.display_rows();
        if let Some(DisplayRow::Agent(a)) = rows.get(cursor) {
            // The SAME agent still sits here: hold. A DIFFERENT agent (a layout
            // shift reindexed the rows) refetches so the header and transcript
            // never disagree (codex review) - the seq guard alone can't catch
            // this, since the stale body already applied under the old identity.
            return (a.name != peeked).then(|| (cursor, a.name.clone()));
        }
        let scan = |dir: isize| {
            let mut i = cursor as isize + dir;
            while i >= 0 && (i as usize) < rows.len() {
                if matches!(rows[i as usize], DisplayRow::Agent(_)) {
                    return Some(i as usize);
                }
                i += dir;
            }
            None
        };
        let anchored = scan(1)
            .or_else(|| scan(-1))
            .and_then(|i| match rows.get(i) {
                Some(DisplayRow::Agent(a)) => Some((i, a.name.clone())),
                _ => None,
            });
        if anchored.is_none() {
            drop(rows);
            self.clear_peek();
        }
        anchored
    }

    /// Sideline rows the cursor can occupy: below the tab bar and above the
    /// bottom chrome row. `draw_bottom_row` repaints the last row over the
    /// sideline when it is chrome, and [`sideline_row_at`] excludes it from
    /// hit-testing, so it must not count as a scroll slot - else follow-cursor
    /// scroll would park the last row under the status bar (invisible, unclickable).
    fn sideline_visible_rows(&self) -> usize {
        (self.term.0 as usize)
            .saturating_sub(TAB_BAR_ROWS as usize)
            .saturating_sub(self.bottom_row_is_chrome() as usize)
    }

    /// Follow-the-cursor sideline scroll (x-a621): move [`View::sideline_offset`]
    /// the least it takes to keep the selector (or hover) row on screen, then
    /// clamp into `[0, rows - visible]` so a shrunk catalog never scrolls past the
    /// last row. Everything-fits (or an empty window) resets the offset to 0, so
    /// the common case renders byte-identically to a non-scrolling sideline.
    fn clamp_sideline_offset(&mut self) {
        let total = self.display_rows().len();
        let visible = self.sideline_visible_rows();
        if total <= visible || visible == 0 {
            self.sideline_offset = 0;
            return;
        }
        if let Some(cur) = self.selector.or(self.hover_row) {
            if cur < self.sideline_offset {
                self.sideline_offset = cur;
            } else if cur >= self.sideline_offset + visible {
                self.sideline_offset = cur + 1 - visible;
            }
        }
        self.sideline_offset = self.sideline_offset.min(total - visible);
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
        let (overlay_origin, overlay_dims) = self.overlay_viewport();
        if let Some(lines) = &self.digest {
            // x-4e2d catch-up overlay: reuse the inverse-video chrome; any key
            // dismisses (handled in handle_stdin, like the key-table overlay).
            draw_lines_overlay(&mut cells, rows, cols, overlay_origin, overlay_dims, lines);
        } else if let Some(m) = &self.keys_modal {
            // x-8ccf US3: the centered which-key modal replaces the old top-left
            // key-table poster (opaque, sectioned, scrollable).
            popup::draw(&mut cells, rows, cols, &m.popup.render(self.term));
        } else if let Some(m) = &self.row_menu {
            // x-8ccf US2: the anchored row context menu, drawn at the pointer.
            popup::draw(&mut cells, rows, cols, &m.popup.render(self.term));
        } else if let Some(m) = &self.aux {
            // x-8ccf US4/US5: the sideline MENU popup or settings modal.
            popup::draw(&mut cells, rows, cols, &m.popup.render(self.term));
        } else if let Some(sel) = self.answers {
            // x-feec needs-me queue (grown from the x-c929 answer overlay): the
            // severity-ranked union + the selected row's answer options, on the
            // shared inverse-video chrome. Always drawn while open - an empty
            // union renders "nothing needs you", a pending/failed fold renders
            // its footer notice (never a blank overlay).
            let (queue, dropped) = self.needs_view();
            let sel = sel.min(queue.len().saturating_sub(1));
            let lines = needs_overlay_lines(&queue, sel, dropped, self.needs_footer());
            draw_lines_overlay(&mut cells, rows, cols, overlay_origin, overlay_dims, &lines);
        } else if let Some((_, squads)) = &self.move_pick {
            // x-96e8 move-tab picker: `move tab to:` + one numbered line per
            // candidate squad, on the same inverse-video overlay chrome.
            let lines = self.move_pick_lines(squads);
            draw_lines_overlay(&mut cells, rows, cols, overlay_origin, overlay_dims, &lines);
        } else if let Some(picker) = &self.attach_place {
            let lines = self.attach_place_lines(picker);
            draw_lines_overlay(&mut cells, rows, cols, overlay_origin, overlay_dims, &lines);
        } else if let Some(conn) = &self.connections {
            // x-84d7 Connections modal: accounts + combos lists on the shared
            // inverse-video chrome. Drawn from the modal's own render (pure).
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &conn.render(),
            );
        } else if let Some(peek) = &self.peek {
            // x-c376 peek overlay: the peeked agent row (re-read LIVE from the
            // layout, navigator-style) header + transcript, on the shared
            // inverse-video chrome. Drawn above nav (mutually exclusive modes).
            let drows = self.display_rows();
            let agent = drows.get(peek.cursor).and_then(|r| match r {
                DisplayRow::Agent(a) => Some(*a),
                _ => None,
            });
            let lines = peek_overlay_lines(agent, peek);
            draw_lines_overlay(&mut cells, rows, cols, overlay_origin, overlay_dims, &lines);
        } else if let Some(nav) = &self.nav {
            // x-653d navigator: the filtered flat catalog + query/chip line, on
            // the same inverse-video overlay chrome. Rows recompute per frame
            // from the live layout (no cache), so a push repopulates it.
            let filtered = self.nav_filtered(nav);
            let lines = nav_overlay_lines(&filtered, nav);
            draw_lines_overlay(&mut cells, rows, cols, overlay_origin, overlay_dims, &lines);
        }

        // Terminal cursor: the FOCUSED pane's, offset into its rect - the
        // one place the cursor may sit (AC1-UI/AC5-UI).
        let (mut cur_r, mut cur_c, mut cur_vis) = (0u16, 0u16, false);
        if self.selector.is_none()
            && self.answers.is_none()
            && self.digest.is_none()
            && self.move_pick.is_none()
            && self.attach_place.is_none()
            && self.nav.is_none()
            && self.peek.is_none()
            && self.connections.is_none()
            && self.keys_modal.is_none()
            && self.row_menu.is_none()
            && self.aux.is_none()
        {
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
                || self.rename.is_some()
                || self.recruit.is_some()
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
            self.draw_confirm_line(cells, rows, cols, c);
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
        // The rename name input (x-c150 tab; widened x-96e8 to squads), same
        // overlay discipline as the create input above; the hint spells out the
        // blank-clears semantics. The noun tracks the target so the operator
        // sees what they are renaming.
        if let Some((target, name)) = &self.rename {
            let r = rows - 1;
            for c in 0..cols {
                cells[r * cols + c] = Cell::default();
            }
            let noun = match target {
                RenameTarget::Tab(_) => "tab",
                RenameTarget::Squad(_) => "workspace",
            };
            let text = format!(" rename {noun}: {name}_ (empty resets to auto)");
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
        // The recruit workspace-name input (x-8f11): same overlay discipline; the
        // hint names how many marked agents will join (create-if-absent).
        if let Some(name) = &self.recruit {
            let r = rows - 1;
            for c in 0..cols {
                cells[r * cols + c] = Cell::default();
            }
            let n = self.marks.len();
            let text = format!(" recruit {n} into: {name}_ (create-if-absent)");
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
                        · g grab · f find · / search · s status · d detach · ? all keys";
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
        // Active squad's name, only when there is more than one squad to be
        // ambiguous about (x-2f99) - the always-visible answer to "which
        // squad?" when the sideline is toggled off or auto-hidden. BOLD: it
        // is identity, like the session cell, not context like the cwd.
        if self.layout.squads.len() > 1 {
            if let Some(s) = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == self.layout.active_squad)
            {
                for ch in format!("│ {} ", s.name).chars() {
                    put(cells, c, ch, cell_flags::BOLD);
                    c += 1;
                }
            }
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

    /// Paint the confirm prompt over the bottom row (x-a496 dispatch; x-96e8
    /// squad removal). Blank first (the x-5041 divider-bleed gotcha), then the
    /// BOLD prompt whose wording tracks the action being confirmed.
    fn draw_confirm_line(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        action: &ConfirmAction,
    ) {
        let r = rows - 1;
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
        let label = &action.label;
        let text = match &action.action {
            ConfirmKind::Dispatch { .. } => format!(" start session on {label}? ⏎/esc"),
            ConfirmKind::RemoveSquad {
                panes, last: true, ..
            } => format!(
                " close workspace {label} ({panes} panes) - last workspace, ends the session? ⏎/esc"
            ),
            ConfirmKind::RemoveSquad {
                panes, last: false, ..
            } => {
                format!(" close workspace {label} ({panes} panes)? ⏎/esc")
            }
            ConfirmKind::StopAgent { .. } => format!(" stop {label}? ⏎/esc"),
            ConfirmKind::RemoveAgent { .. } => format!(" remove {label}? ⏎/esc"),
            ConfirmKind::ReapAgents => " reap all exited fno agents? ⏎/esc".to_string(),
            ConfirmKind::StopExternal { .. } => format!(" stop {label}? ⏎/esc"),
            ConfirmKind::RemoveExternal { .. } => {
                format!(" remove {label} and worktree? ⏎/esc")
            }
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

    /// Build the move-tab picker overlay lines (x-96e8): a header plus one
    /// numbered line per candidate squad, the number being the digit that
    /// selects it. A candidate that vanished from the catalog since open still
    /// renders (labelled) - the digit is re-validated on press, and the server
    /// refuses a stale id regardless.
    fn move_pick_lines(&self, squads: &[u64]) -> Vec<String> {
        const W: usize = 40;
        let mut lines = vec![pad_to(" move tab to: · digit selects · esc cancel", W)];
        for (i, &sid) in squads.iter().enumerate() {
            let name = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == sid)
                .map(|s| s.name.as_str())
                .unwrap_or("(gone)");
            lines.push(pad_to(&format!(" {} {name}", i + 1), W));
        }
        lines
    }

    fn attach_place_lines(&self, picker: &AttachPlace) -> Vec<String> {
        const W: usize = 54;
        let mut lines = vec![pad_to(" attach placement · digit selects workspace", W)];
        for (i, &sid) in picker.squads.iter().enumerate() {
            let name = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == sid)
                .map(|s| s.name.as_str())
                .unwrap_or("(gone)");
            let marker = if sid == picker.target { '›' } else { ' ' };
            lines.push(pad_to(&format!(" {marker} {} {name}", i + 1), W));
        }
        lines.push(pad_to(" h left · j down · k up · l right", W));
        lines.push(pad_to(" t/enter new tab · esc/q cancel", W));
        lines
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
            let label = tab_label_text(&t.name, i);
            let (text, flags) = if i == s.active_tab {
                (format!("[{label}]"), cell_flags::INVERSE)
            } else {
                (format!(" {label} "), 0)
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
        if let Some((start, text)) = self.notice_overlay(cols) {
            for (i, ch) in text.chars().enumerate() {
                let idx = start + i;
                if idx < cols {
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

    fn notice_overlay(&self, cols: usize) -> Option<(usize, String)> {
        let (text, _) = self.notice.as_ref()?;
        let text: String = text.chars().take(cols.saturating_sub(1)).collect();
        let start = cols.saturating_sub(text.chars().count() + 1);
        Some((start, text))
    }

    /// The sideline's display order (4a-G2): each squad's squad/tab rows, that
    /// squad's agent rows, then the `+ new workspace` footer, a catch-all
    /// section for agents matched to no squad, and the work-queue lane. The
    /// ONE row enumeration (x-260a): painting, hover, mouse hit-testing, and
    /// the leader+w selector all index into it.
    /// (x-0090) The 1-based display ordinal of `tab_id` within its squad,
    /// derived from the `Layout` the client already holds (Discretion 2): a
    /// closed tab shifts the survivors, so the next push recomputes the right
    /// ordinal (AC2-EDGE) with no server round-trip. `None` if the squad/tab
    /// raced out of the layout - the row then renders without a suffix.
    fn tab_ordinal(&self, squad: Option<u64>, tab_id: TabId) -> Option<usize> {
        let sid = squad?;
        let s = self.layout.squads.iter().find(|s| s.id == sid)?;
        s.tabs.iter().position(|t| t.id == tab_id).map(|i| i + 1)
    }

    fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        let mut out = Vec::new();
        for s in &self.layout.squads {
            out.push(DisplayRow::Sel(SelRow {
                squad: s.id,
                tab: None,
            }));
            // Agents-first (x-0090, Locked 4): the caret gates the squad's agent
            // rows; tab rows are gone (tabs live in the top tab bar). A collapsed
            // squad shows only its name row + the x-d140 rollup glyph, so the
            // rollup is the sole signal there - no more agent rows rendering
            // unconditionally under a folded squad.
            if self.expanded.contains(&s.id) {
                out.extend(
                    self.layout
                        .agents
                        .iter()
                        .filter(|a| a.squad == Some(s.id))
                        .map(DisplayRow::Agent),
                );
            }
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
            // Orphans (cwd matched no squad) keep one flat section in the same
            // row grammar; the header reads `~ elsewhere` (Locked 6).
            out.push(DisplayRow::Header("~ elsewhere"));
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
        let off = self.sideline_offset;
        // `i` stays the TRUE display index (so the selector/hover highlight and
        // hit-test still match); the painted row subtracts the scroll offset.
        for (i, drow) in self.display_rows().into_iter().enumerate().skip(off) {
            let r = TAB_BAR_ROWS as usize + (i - off);
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
                            let expanded = self.expanded.contains(&squad.id);
                            let caret = if expanded { '▾' } else { '▸' };
                            // `*` after the caret marks the active squad so
                            // activity survives weak-BOLD themes and manual
                            // collapse (x-2f99); replaces the space, so row
                            // width is unchanged. Same vocabulary as the
                            // active-tab marker below.
                            let mark = if is_active_squad { '*' } else { ' ' };
                            let flags = if is_active_squad { cell_flags::BOLD } else { 0 };
                            let base = format!("{caret}{mark}{}", squad.name);
                            // Roll the squad's worst agent state onto its
                            // COLLAPSED name row so a blocked pane is visible
                            // without expanding (x-d140). An expanded squad
                            // already shows each agent's glyph on its own row, so
                            // it folds to Idle here (no duplicate glyph, name row
                            // byte-identical to today). Reuses x-653d's
                            // `nav_agent_state` fold (exited -> Idle; the x-4328
                            // seen bit not yet wired); worst = Ord-min per
                            // PaneState's ordering.
                            let state = if expanded {
                                PaneState::Idle
                            } else {
                                self.layout
                                    .agents
                                    .iter()
                                    .filter(|a| a.squad == Some(squad.id))
                                    .map(nav_agent_state)
                                    .min()
                                    .unwrap_or(PaneState::Idle)
                            };
                            let text = match rollup_glyph(state) {
                                // Reserve the last 2 cells for ` {glyph}` at the
                                // right edge, truncating the name into
                                // `text_w - 2` FIRST so the later `.take(text_w)`
                                // can never clip the signal. Guarded on width so
                                // a narrow panel skips the reservation instead of
                                // underflowing the subtraction.
                                Some(glyph) if text_w >= 3 => {
                                    let body: String = base.chars().take(text_w - 2).collect();
                                    format!("{body:<width$} {glyph}", width = text_w - 2)
                                }
                                _ => base,
                            };
                            (text, flags)
                        }
                        Some(t) => {
                            let marker = if is_active_squad && t == squad.active_tab {
                                '*'
                            } else {
                                ' '
                            };
                            // The same digit-collapse as the tab bar: a
                            // no-signal tab renders its bare ordinal (x-c150).
                            let label = match squad.tabs.get(t) {
                                Some(tm) => tab_label_text(&tm.name, t),
                                None => (t + 1).to_string(),
                            };
                            (format!("  {marker}{label}"), 0)
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
                    // A recruit mark (x-8f11) replaces the leading space with a
                    // `*`, keeping the row width unchanged (same vocabulary as
                    // the active-squad/tab marker).
                    let mark = if a
                        .attach_id
                        .as_deref()
                        .is_some_and(|id| self.marks.contains(id))
                    {
                        '*'
                    } else {
                        ' '
                    };
                    let mut text = format!(" {mark}{glyph} {}", a.name);
                    // (x-0090) A pane row names its tab with a `·N` ordinal; an
                    // orphan row names its repo with a ` (basename)` suffix. The
                    // two are mutually exclusive (a pane row has a tab, an orphan
                    // is paneless), so at most one suffix ever lands.
                    if let Some(ord) = a.tab.and_then(|tid| self.tab_ordinal(a.squad, tid)) {
                        text.push_str(&format!(" ·{ord}"));
                    } else if let Some(base) = a.cwd_base.as_deref() {
                        text.push_str(&format!(" ({base})"));
                    }
                    if let Some(reason) = a.reason.as_deref().filter(|x| !x.is_empty()) {
                        text.push_str(": ");
                        text.push_str(reason);
                    }
                    // Fact-badge lattice + roster provenance (x-0a2e, AC1-UI):
                    // `✗`+DIM = exited, `·`+DIM = external (roster-surfaced live),
                    // `·` bright = fno-owned live. Exit keeps its glyph; external
                    // only dims a live `·` row, staying pairwise-distinct.
                    let flags = if a.exited || a.external {
                        cell_flags::DIM
                    } else {
                        0
                    };
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
                DisplayRow::NewSquad => {
                    // The recruit-mark footer count rides the create affordance
                    // (x-8f11): `space` marks, `R` recruits the marked set.
                    let base = if self.marks.is_empty() {
                        FOOTER_NEW_LABEL.to_string()
                    } else {
                        format!("{FOOTER_NEW_LABEL}   {} marked ·R", self.marks.len())
                    };
                    // x-8ccf US4: the `☰ menu` button rides the footer's right edge
                    // when the panel is wide enough (footer_menu_range gates it);
                    // the same range routes a click there to the MENU popup.
                    let label = match self.footer_menu_range(panel_w) {
                        Some(range) => format!("{}{FOOTER_MENU}", pad_to(&base, range.start)),
                        None => base,
                    };
                    (label, cell_flags::DIM)
                }
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
    /// A work-queue backlog card (x-6f77); a Ready card dispatches via the
    /// confirm (x-a496), by click or selector Enter (x-260a).
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
    /// Owned, not `&'static`: an in-flight card's notice carries the
    /// server-computed `where_hint` (v18), which is per-card data.
    Notice(String),
    Confirm(ConfirmAction),
    /// Open the new-workspace name-input overlay (x-9e5e); the `+` footer.
    OpenCreate,
    /// Flip the active squad row's caret locally (x-2f99); no socket write.
    ToggleExpand(u64),
    /// Open the sideline MENU popup anchored at the footer's menu cell (x-8ccf
    /// US4). Carries the click cell so the popup anchors under the pointer.
    OpenSidelineMenu {
        row: u16,
        col: u16,
    },
}

/// The [`ChromeHit`] for an agent row: focus its pane, else attach a paneless
/// claude bg row, else say it has no pane here. Shared by a sideline click
/// ([`View::row_action`]) and the navigator's goto ([`View::nav_rows`]) so the
/// two inputs resolve the same entity identically (x-653d). Pure - the agent's
/// own fields decide, so no `&self` needed.
fn agent_hit(a: &AgentRow, active_squad: u64) -> ChromeHit {
    match a.pane_id {
        Some(pid) => ChromeHit::Cmds(vec![Command::FocusPane(pid)]),
        None => match &a.attach_id {
            // Watch-only attachable row (x-9f75 row-kind default): a
            // same-workspace sibling (its squad is the active one) splits the
            // current tab so siblings sit side by side; a cross-workspace /
            // orphan row keeps the new-tab-in-owner default (owner routing +
            // view switch, today's behavior). Peek stays an explicit gesture,
            // never a click default.
            Some(id) if a.squad == Some(active_squad) => {
                ChromeHit::Cmds(vec![Command::AttachAgent {
                    id: id.clone(),
                    placement: PanePlacement {
                        target: PaneTarget::CurrentRoute,
                        split: Some(Dir::Right),
                        here: false,
                    },
                }])
            }
            Some(id) => ChromeHit::Cmds(vec![Command::attach_agent(id)]),
            None => ChromeHit::Notice("agent has no pane here".into()),
        },
    }
}

/// The rollup state of a pane/agent, worst-first. The navigator's state filter
/// (x-653d), the squad-row rollup (x-d140), and seen/unseen surfacing (x-4328)
/// all consume it. Derived, never wire-serialized - computed from [`AgentBadge`]
/// + the seen bit at render time. The derive orders it `Blocked < Working <
/// DoneUnseen < Idle`, so a squad rollup is `agents.map(pane_state).min()` (the
/// worst state wins - x-d140's `min` and this filter agree on the ordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PaneState {
    Blocked,
    Working,
    DoneUnseen,
    Idle,
}

/// Derive a [`PaneState`] from an agent's badge and whether its output has
/// been seen (x-4328's `AgentRow.seen`, server-owned): a `Done` badge folds
/// to `Idle` once seen, else `DoneUnseen` (a finished-but-unviewed agent
/// stays surfaced).
fn pane_state(badge: Option<AgentBadge>, seen: bool) -> PaneState {
    match badge {
        Some(AgentBadge::Blocked) => PaneState::Blocked,
        Some(AgentBadge::Working) => PaneState::Working,
        Some(AgentBadge::Done) if seen => PaneState::Idle,
        Some(AgentBadge::Done) => PaneState::DoneUnseen,
        None => PaneState::Idle,
    }
}

/// Why a session needs a human, worst-first (x-feec). Declaration order IS the
/// severity contract: the needs-me queue is `(kind, ts, name)`-sorted, so the
/// worst band leads and the longest-waiting fold item tops its band (a live
/// badge row carries no ts, so it degenerates to name order within its band -
/// leg-1 and leg-2 never share a band, so the two orderings never mix). Same
/// declaration-order `Ord` trick as [`PaneState`]. `Decision` is reserved for
/// the typed help / decision-gate source (x-dbaf) and is unpopulated in v1 -
/// kept so the ordering contract and the future fold arm have a home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NeedKind {
    // Reserved (x-dbaf); never constructed in v1. Leads the order so a decision
    // gate, when it lands, outranks everything downstream of it. Kept so the
    // severity contract and the future fold arm have a home.
    #[allow(dead_code)]
    Decision,
    BlockedAnswerable,
    BlockedFocusOnly,
    ReviewWedged,
    BudgetStop,
    DoneUnseen,
}

/// The leading glyph per need kind, matching the sideline's [`nav_glyph`]
/// vocabulary where they overlap (blocked `▲`, done `✓`).
fn need_glyph(k: NeedKind) -> char {
    match k {
        NeedKind::Decision => '⁉',
        NeedKind::BlockedAnswerable | NeedKind::BlockedFocusOnly => '▲',
        NeedKind::ReviewWedged => '⏳',
        NeedKind::BudgetStop => '⏹',
        NeedKind::DoneUnseen => '✓',
    }
}

/// One unified needs-me-queue row: a live badge row (leg 1) or an event-fold
/// item joined to the roster (leg 2), reduced to what the overlay renders and
/// routes on. Identity for cursor re-anchor is `(kind, name)`.
#[derive(Clone)]
struct NeedRow {
    kind: NeedKind,
    name: String,
    reason: String,
    /// The deciding event ts for a fold row (oldest-first tie-break); `""` for a
    /// live badge row (name-ordered within its band).
    ts: String,
    /// A STABLE re-anchor key: a fold row's session id (survives a joined <->
    /// squadless flip, where `name` changes), a badge row's name. Not shown.
    id_key: String,
    /// Present only on a `BlockedAnswerable` row - the digit-answer payload.
    answerable: Option<AnswerablePrompt>,
    pane_id: Option<u64>,
    attach_id: Option<String>,
    squad: Option<u64>,
    tab: Option<u64>,
}

impl NeedRow {
    /// The re-anchor identity: a scrape tick / fold merge keeps the cursor on
    /// the same item, not the same index (AC3-UI). Keyed on `(kind, id_key)` -
    /// a stable session id for fold rows so a joined<->squadless transition (its
    /// `name` flips) does not drop the cursor (codex P2).
    fn id(&self) -> (NeedKind, String) {
        (self.kind, self.id_key.clone())
    }
}

/// Cap on rendered rows (worst-first, so the cap drops only the least severe);
/// the footer states the drop count. Matches the sideline card cap.
const NEEDS_CAP: usize = 40;
/// Re-open cache: a fold younger than this is reused instantly (mashing
/// `leader+a` never re-shells - Perspective B).
const NEEDS_CACHE_TTL: Duration = Duration::from_secs(5);
/// Default fold window: the last 24h (the fold also windows server-side).
const NEEDS_WINDOW_SECS: u64 = 24 * 60 * 60;

/// One row of the navigator's flat catalog (x-653d). Fully owned (no layout
/// borrow) so goto can mutate the view after the catalog is built; recomputed
/// per keypress, never cached.
struct NavRow {
    /// The searchable, displayed label (e.g. `nairobi › build` or
    /// `nairobi › claude#3`). The text filter matches here, case-insensitively.
    label: String,
    /// Derived rollup state, for the state filter + the leading glyph.
    state: PaneState,
    /// Switch to this squad before applying `hit`, when it is not already the
    /// active one (an agent's or pane's row lives in another squad). `None` for a
    /// squad/tab row (the switch is in `hit`) or a card (never switches).
    goto_squad: Option<u64>,
    /// Switch to this tab (after `goto_squad`) before applying `hit`, when it is
    /// not the active view's tab. `Some` only for a pane row (a pane lives in a
    /// specific tab, which `FocusPane` alone does not select); `None` for every
    /// other row. Together the two prefixes give a pane goto the full
    /// SelectSquad -> SelectTab -> FocusPane sequence.
    goto_tab: Option<u64>,
    /// The terminal action: SelectSquad/SelectTab for a container row,
    /// FocusPane/AttachAgent for an agent, the dispatch confirm / focus for a
    /// card, or a [`ChromeHit::Notice`] that keeps the navigator open.
    hit: ChromeHit,
}

/// The navigator state of an agent row: an exited pane reads `Idle` (finished,
/// nothing to act on); otherwise derive from the badge + the server-owned
/// seen bit (x-4328): a looked-at `Done` reads `Idle`, an unseen one
/// `DoneUnseen`.
fn nav_agent_state(a: &AgentRow) -> PaneState {
    if a.exited {
        PaneState::Idle
    } else {
        pane_state(a.badge, a.seen)
    }
}

/// The navigator state of a work-queue card: blocked/in-flight map onto
/// `Blocked`/`Working` so the state filter surfaces stuck and running work
/// uniformly with agents; a ready card is neutral (`Idle`).
fn card_state(c: &BacklogCard) -> PaneState {
    match c.state {
        CardState::Blocked => PaneState::Blocked,
        CardState::InFlight => PaneState::Working,
        CardState::Ready => PaneState::Idle,
    }
}

/// A named tab's visible label width in the tab bar / sideline (x-c150);
/// keeps ~4 labeled tabs visible at 100 cols.
const TAB_LABEL_W: usize = 14;

/// A tab span's label body (x-c150): the bare 1-based ordinal when the
/// server-derived name carries no signal (the name IS the ordinal -
/// byte-for-byte today's render for a plain shell tab, AC1-EDGE), else
/// `{ordinal}:{name}` with the name truncated to [`TAB_LABEL_W`] chars. The
/// ordinal stays visible in every span because the `1-9 select tab` keys
/// key off it (Locked 5).
fn tab_label_text(name: &str, i: usize) -> String {
    let ordinal = (i + 1).to_string();
    if name == ordinal {
        return ordinal;
    }
    let short: String = name.chars().take(TAB_LABEL_W).collect();
    format!("{ordinal}:{short}")
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

/// Draw inverse-video overlay lines centered in the content viewport (right of
/// the sideline, above any splits), one line per row, cell-bounds-checked (a
/// tiny terminal clips rather than panics). `content_origin` is `(TAB_BAR_ROWS,
/// panel_w)`; `content_dims` is the content viewport's `(rows, cols)` (status
/// row excluded). Shared by every corner-anchored popover (key-table, needs
/// queue, nav, peek, move-pick, attach-place, connections) so centering all of
/// them is this one change (x-e9c3; placement policy per x-9f75).
fn draw_lines_overlay<S: AsRef<str>>(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    content_origin: (usize, usize),
    content_dims: (usize, usize),
    lines: &[S],
) {
    let (base_r, base_c) = content_origin;
    let (content_rows, content_cols) = content_dims;
    let box_h = lines.len();
    let box_w = lines
        .iter()
        .map(|l| l.as_ref().chars().count())
        .max()
        .unwrap_or(0);
    let origin_r = base_r + content_rows.saturating_sub(box_h) / 2;
    let origin_c = base_c + content_cols.saturating_sub(box_w) / 2;
    for (i, line) in lines.iter().enumerate() {
        let r = origin_r + i;
        if r >= rows {
            break;
        }
        for (j, ch) in line.as_ref().chars().enumerate() {
            let c = origin_c + j;
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

/// The answer-overlay content width; lines truncate to it (AC3-UI: a long
/// option label truncates for display while the daemon fingerprints the full
/// region text) and pad to it so the inverse block is a clean rectangle.
const ANSWER_OVERLAY_W: usize = 54;

/// The footer state of the needs-me overlay: whether the event-fold leg is
/// still in flight, failed (loud degrade, AC2-ERR), or landed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NeedsFooter {
    Folding,
    Degraded,
    AsOf,
}

/// Build the needs-me overlay lines (x-feec): the severity-ranked union + the
/// selected row's answer options + a state footer, on the shared inverse-video
/// chrome. `sel` is pre-clamped by the caller. A `▸` marks the selected row; an
/// answerable row lists its numbered options, a focus-only row is tagged.
/// Always renders something - an empty union shows "nothing needs you", so the
/// overlay never opens blank.
fn needs_overlay_lines(
    queue: &[NeedRow],
    sel: usize,
    dropped: usize,
    footer: NeedsFooter,
) -> Vec<String> {
    let mut lines = vec![pad_to(
        " needs me · digit answers · n/N cycle · ⏎ goto · q close",
        ANSWER_OVERLAY_W,
    )];
    if queue.is_empty() {
        lines.push(pad_to("   nothing needs you", ANSWER_OVERLAY_W));
    } else {
        for (i, r) in queue.iter().enumerate() {
            let marker = if i == sel { '▸' } else { ' ' };
            let tag = match r.kind {
                NeedKind::BlockedFocusOnly => "  ⚠ focus",
                _ => "",
            };
            lines.push(pad_to(
                &format!(
                    " {marker} {} {}  {}{tag}",
                    need_glyph(r.kind),
                    r.name,
                    r.reason
                ),
                ANSWER_OVERLAY_W,
            ));
        }
        lines.push(pad_to("", ANSWER_OVERLAY_W)); // divider row
        if let Some(ans) = queue.get(sel).and_then(|r| r.answerable.as_ref()) {
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
    }
    let footer_line = match footer {
        NeedsFooter::Folding => "   folding events...".to_string(),
        NeedsFooter::Degraded => "   events fold unavailable - live badges only".to_string(),
        NeedsFooter::AsOf if dropped > 0 => {
            format!("   {dropped} more hidden (worst shown first)")
        }
        NeedsFooter::AsOf => "   as of now".to_string(),
    };
    lines.push(pad_to(&footer_line, ANSWER_OVERLAY_W));
    lines
}

/// The navigator overlay content width (x-653d): labels truncate to it and pad
/// to it so the inverse block is a clean rectangle, like the answer overlay.
const NAV_OVERLAY_W: usize = 54;

/// The leading state glyph for a navigator row (x-653d), reusing the sideline's
/// agent-badge lattice: blocked `▲`, working `●`, done `✓`, idle `·`.
fn nav_glyph(s: PaneState) -> char {
    match s {
        PaneState::Blocked => '▲',
        PaneState::Working => '●',
        PaneState::DoneUnseen => '✓',
        PaneState::Idle => '·',
    }
}

/// Build the navigator overlay lines (x-653d): a top `find › <query>  [chip]`
/// line, then one line per FILTERED row with a leading state glyph and the
/// cursor row marked `▸`. An empty result renders a single `no matches` line
/// (the key handler BELs). `rows` is pre-filtered; `cursor` is pre-clamped.
fn nav_overlay_lines(rows: &[NavRow], nav: &NavView) -> Vec<String> {
    let chip = match nav.state_filter {
        None => "all",
        Some(PaneState::Blocked) => "blocked",
        Some(PaneState::Working) => "working",
        Some(PaneState::DoneUnseen) => "done",
        Some(PaneState::Idle) => "idle",
    };
    let mut lines = vec![pad_to(
        &format!(" find › {}   [{chip}]", nav.query),
        NAV_OVERLAY_W,
    )];
    if rows.is_empty() {
        lines.push(pad_to("   no matches", NAV_OVERLAY_W));
        return lines;
    }
    for (i, r) in rows.iter().enumerate() {
        let marker = if i == nav.cursor { '▸' } else { ' ' };
        lines.push(pad_to(
            &format!(" {marker} {} {}", nav_glyph(r.state), r.label),
            NAV_OVERLAY_W,
        ));
    }
    lines
}

/// The peek overlay content width (x-c376): wider than the navigator/answer
/// overlays because it renders transcript lines, clamped to the terminal by
/// `draw_lines_overlay`.
const PEEK_OVERLAY_W: usize = 72;

/// Wrap `s` into lines no wider than `w` display chars, breaking on spaces. A
/// single word longer than `w` becomes its own line (pad_to ellipsizes it) - a
/// status sentence has no such words in practice, so the simple greedy pass is
/// enough. Always returns at least one (possibly empty) line.
fn wrap_words(s: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in s.split_whitespace() {
        match out.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= w => {
                line.push(' ');
                line.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Build the read-only peek overlay lines (x-c376): a header (badge glyph + name
/// + full wrapped status sentence), the x-c929 answerable block when the row is
/// blocked (prompt + numbered options, reused verbatim), a divider, then the
/// transcript body (" loading…" until it arrives, "no activity yet" for an empty
/// one, error/timeout text rendered verbatim as body lines) and a footer hint.
/// `agent` is the LIVE row re-read per frame; `None` means it vanished between
/// key and frame (a transient single frame - the key handler re-anchors/closes).
fn peek_overlay_lines(agent: Option<&AgentRow>, peek: &PeekView) -> Vec<String> {
    let Some(a) = agent else {
        return vec![pad_to(" peek · row gone", PEEK_OVERLAY_W)];
    };
    // Sanitize every external-sourced line (transcript body, scraped reason)
    // before it becomes overlay cells (codex review): `fno agents peek` reads
    // raw on-disk transcript text that can carry ANSI escapes / C0 controls, and
    // the peek path does NOT VT-parse (unlike pane output), so an unstripped
    // ESC/CR would reach the operator's terminal. Tabs become spaces; every
    // other control char is dropped (a residual bracket-code is harmless text).
    fn sanitize_peek_line(s: &str) -> String {
        s.chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect()
    }
    let glyph = if a.exited {
        '✗'
    } else {
        nav_glyph(pane_state(a.badge, a.seen))
    };
    let mut lines = vec![pad_to(&format!(" {glyph} {}", a.name), PEEK_OVERLAY_W)];
    if let Some(reason) = a.reason.as_deref().filter(|s| !s.is_empty()) {
        for wl in wrap_words(&sanitize_peek_line(reason), PEEK_OVERLAY_W - 3) {
            lines.push(pad_to(&format!("   {wl}"), PEEK_OVERLAY_W));
        }
    }
    // x-c929 answerable block: prompt + numbered options, mirroring the needs-me
    // overlay's body so a blocked peek reads identically. Digit answers (US3)
    // act on exactly these options.
    if let Some(ans) = &a.answerable {
        lines.push(pad_to("", PEEK_OVERLAY_W));
        if !ans.prompt.is_empty() {
            lines.push(pad_to(
                &format!("   {}", ans.prompt.replace('\n', " ")),
                PEEK_OVERLAY_W,
            ));
        }
        for o in &ans.options {
            lines.push(pad_to(
                &format!("     {}. {}", o.idx, o.label),
                PEEK_OVERLAY_W,
            ));
        }
    }
    lines.push(pad_to("", PEEK_OVERLAY_W)); // divider before the transcript
    match &peek.body {
        None => lines.push(pad_to("   loading…", PEEK_OVERLAY_W)),
        Some(body) if body.is_empty() => lines.push(pad_to("   no activity yet", PEEK_OVERLAY_W)),
        Some(body) => {
            for l in body {
                lines.push(pad_to(
                    &format!(" {}", sanitize_peek_line(l)),
                    PEEK_OVERLAY_W,
                ));
            }
        }
    }
    lines.push(pad_to(
        " j/k peek · digit answers · ⏎ attach · esc back",
        PEEK_OVERLAY_W,
    ));
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
    // config.toml read (fail-open to on), the digest_overlay idiom.
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
                | ServerMsg::SearchResult { .. }
                // PeekBody answers a post-attach PeekAgent (x-c376): impossible
                // in the preamble, ignore rather than desync.
                | ServerMsg::PeekBody { .. },
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

    // x-feec: the needs-me event-fold leg runs off the UI loop and reports back
    // here, tagged with the generation token it was kicked under, so a slow
    // `fno-agents needs` never blocks the overlay and a result landing after the
    // overlay closed/re-opened is discarded (AC6-FR). `None` = the fold failed.
    let (needs_tx, mut needs_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, Option<Vec<crate::needs_overlay::FoldItem>>)>(
        );

    // x-84d7: the Connections modal's read fold runs off the UI loop and reports
    // back here, tagged with the generation it was kicked under, so a slow `fno`
    // never blocks the modal and a result landing after a close/refresh is
    // discarded. Carries a full ReadOutcome (Ok lists, or a named degrade).
    let (conn_tx, mut conn_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, crate::connections_view::ReadOutcome)>();

    // x-84d7: a Connections single-flight mutation verb reports its result here,
    // gen-tagged like the read, so a result for a closed/superseded modal is
    // dropped and a live one surfaces the notice + triggers the read-after-write.
    let (conn_act_tx, mut conn_act_rx) = tokio::sync::mpsc::unbounded_channel::<(
        u64,
        crate::connections_view::ActionResult,
        bool, // is_login: keep the pending notice on success, no acting flip
    )>();

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
        // x-feec: kick a wanted event-fold off the UI loop, at most ONE in
        // flight (P2-5). Runs at loop top so a want re-armed from either the
        // stdin arm (OpenAnswers) or the needs_rx arm (superseded refold) fires
        // without needing another keypress. The sender lives in this scope, out
        // of the deep stdin handler; the result reports back on needs_rx tagged
        // with this generation.
        if view.needs_want && !view.needs_inflight {
            view.needs_want = false;
            view.needs_inflight = true;
            let tx = needs_tx.clone();
            let gen = view.needs_gen;
            let since = crate::digest_overlay::now_secs()
                .saturating_sub(NEEDS_WINDOW_SECS)
                .to_string();
            tokio::spawn(async move {
                let result = crate::needs_overlay::fold_now(&since).await;
                let _ = tx.send((gen, result));
            });
        }
        // x-84d7: kick a wanted Connections read off the UI loop, at most one in
        // flight, tagged with the current gen so a stale result is dropped.
        if view.conn_want && !view.conn_inflight {
            view.conn_want = false;
            view.conn_inflight = true;
            let tx = conn_tx.clone();
            let gen = view.conn_gen;
            tokio::spawn(async move {
                let outcome = crate::connections_view::load_all().await;
                let _ = tx.send((gen, outcome));
            });
        }
        // x-84d7: run a wanted single-flight mutation off the UI loop. The modal's
        // `acting` flag (set by the reducer) is the concurrency guard, so no extra
        // inflight bool is needed here; the result reports on conn_act_rx.
        if let Some((argv, env, is_login)) = view.conn_action.take() {
            let tx = conn_act_tx.clone();
            let gen = view.conn_gen;
            tokio::spawn(async move {
                let result = crate::connections_view::run_verb_env(argv, env).await;
                let _ = tx.send((gen, result, is_login));
            });
        }
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
                    // x-c376: a scrape tick may have removed the peeked row.
                    // Re-anchor to an adjacent agent row (fetch its transcript)
                    // or close - never a stale render / panic (AC1-EDGE).
                    if let Some((cursor, name)) = view.peek_reanchor() {
                        if let Err(e) = fetch_peek(&mut view, cursor, name, &mut sock_w).await {
                            break Err(e);
                        }
                    }
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
                Ok(ServerMsg::PeekBody { seq, lines, .. }) => {
                    // x-c376: the seq guard (AC1-FR) drops a superseded body so
                    // B's header never shows A's transcript. `name` is a wire
                    // checksum; the header reads the live row.
                    if view.apply_peek_body(seq, lines) {
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
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
            Some((gen, result)) = needs_rx.recv() => {
                // x-feec: an event-fold landed; the in-flight fold is done, so a
                // later open may spawn a fresh one (P2-5 bound).
                view.needs_inflight = false;
                // Merge only if the overlay is still open under the same
                // generation it was kicked for; a result for a closed/superseded
                // overlay is discarded (AC6-FR). If the overlay is still open but
                // moved on (re-opened, still live-only), re-arm a fresh fold.
                if gen == view.needs_gen && view.answers.is_some() {
                    let prev = view.answers_selected_id();
                    match result {
                        Some(items) => {
                            view.needs_fold = Some(items);
                            view.needs_degraded = false;
                            // Only a SUCCESS seeds the re-open cache; a failure is
                            // never cached, so the next open retries instead of
                            // silently serving the failed empty fold (P2-6).
                            view.needs_fold_at = Some(Instant::now());
                        }
                        // Fold failed/timed out: keep the live badge leg, flip the
                        // loud degraded notice (AC2-ERR), never a silent partial
                        // queue. An empty Some keeps leg-1 rendering; leave
                        // needs_fold_at untouched so the next open re-folds.
                        None => {
                            view.needs_fold = Some(Vec::new());
                            view.needs_degraded = true;
                        }
                    }
                    view.reanchor_answers(prev);
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                } else if view.answers.is_some() && view.needs_fold.is_none() {
                    // A superseded fold returned while the current overlay still
                    // needs one (re-opened past the cache): kick a fresh fold.
                    view.needs_want = true;
                }
            }
            Some((gen, outcome)) = conn_rx.recv() => {
                // x-84d7: apply a Connections read under the gen guard. A result
                // for a closed/superseded modal is discarded; a live match seeds
                // the lists (or the degraded banner) and repaints.
                view.conn_inflight = false;
                if gen == view.conn_gen {
                    if let Some(cv) = view.connections.as_mut() {
                        cv.apply_read(outcome);
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
                    }
                }
            }
            Some((gen, result, is_login)) = conn_act_rx.recv() => {
                // x-84d7: a mutation/login verb finished. Clear the single-flight
                // guard UNCONDITIONALLY (the subprocess has exited, whatever the
                // modal's read-gen), so a manual R during the mutation can never
                // wedge `acting` on nor let a second write overlap this one. The
                // notice + re-read are still gen-guarded: a stale/closed result
                // shows nothing (never optimistic state).
                if let Some(cv) = view.connections.as_mut() {
                    cv.acting = false;
                }
                if gen == view.conn_gen && view.connections.is_some() {
                    if let Some(cv) = view.connections.as_mut() {
                        if is_login {
                            // The login pane spawn: on failure name it; on success
                            // keep the reducer's "login pane opened - press r" notice.
                            if !result.ok {
                                cv.notice = Some(result.msg);
                            }
                        } else {
                            cv.notice = Some(result.msg);
                        }
                    }
                    view.rearm_connections_read();
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
            }
            _ = winch.recv() => {
                if let Ok((cols, rows)) = terminal::size() {
                    view.term = (rows, cols);
                    // A shorter terminal shrinks the scroll window; re-clamp so
                    // the offset never scrolls past the last row (x-a621).
                    view.clamp_sideline_offset();
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
        // x-8ccf US3: while the which-key modal is open, the mouse drives it
        // (hover selects, wheel scrolls, click executes or dismisses) and is
        // SWALLOWED - it never reaches a pane or the chrome underneath.
        if view.keys_modal.is_some() {
            if let StdinFlow::Detach = keys_modal_mouse(view, rep, sock_w).await? {
                return Ok(StdinFlow::Detach);
            }
            continue;
        }
        // x-8ccf US2: the row context menu owns the mouse while open (hover
        // selects, click runs, right-press re-anchors) and is swallowed.
        if view.row_menu.is_some() {
            row_menu_mouse(view, rep, sock_w).await?;
            continue;
        }
        // x-8ccf US4/US5: the MENU popup / settings modal owns the mouse.
        if view.aux.is_some() {
            if let StdinFlow::Detach = aux_mouse(view, rep, sock_w).await? {
                return Ok(StdinFlow::Detach);
            }
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
        // x-8ccf US2: right-click a sideline row opens its context menu (agent
        // rows) or is swallowed (non-agent chrome). A right-click on a PANE cell
        // (sideline_row_at -> None) falls through and forwards to the inner app,
        // so pane right-click behavior is untouched (AC3-EDGE).
        if matches!(rep.kind, MouseKind::Press(MouseButton::Right)) {
            if let Some(i) = view.sideline_row_at(rep.row, rep.col) {
                view.open_row_menu(
                    i,
                    Anchor::At {
                        row: rep.row,
                        col: rep.col,
                    },
                );
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
    if view.keys_modal.is_some() {
        // x-8ccf US3 which-key: a bound key executes through the shared dispatch,
        // arrows/pgup scroll+select, Enter runs the selected row, Esc/unbound
        // dismiss. Routed here (same precedence as the old poster) so its keys
        // never leak to a pane.
        return keys_modal_keys(view, &passthrough, sock_w).await;
    }
    if view.row_menu.is_some() {
        // x-8ccf US2: the row context menu consumes keys while open (arrows walk
        // the entries + grid, Enter runs, Esc/q close) - never leaks to a pane.
        return row_menu_keys(view, &passthrough, sock_w).await;
    }
    if view.aux.is_some() {
        // x-8ccf US4/US5: the MENU popup / settings modal consumes keys.
        return aux_keys(view, &passthrough, sock_w).await;
    }
    if view.connections.is_some() {
        // x-84d7: the Connections modal consumes all keys while open (Tab
        // switches tabs, j/k move, R refreshes, Esc closes) - never leaks to a
        // pane. Routed here (top-level modal, like the MENU it opened from).
        return connections_keys(view, &passthrough, sock_w).await;
    }
    if view.confirm.is_some() {
        return confirm_keys(view, &passthrough, sock_w).await;
    }
    if view.move_pick.is_some() {
        // Modal like confirm (x-96e8): a single digit/Esc resolves it. Ahead of
        // the selector (which it replaced on open) so its keys can't leak there.
        return move_pick_keys(view, &passthrough, sock_w).await;
    }
    if view.attach_place.is_some() {
        return attach_place_keys(view, &passthrough, sock_w).await;
    }
    if view.peek.is_some() {
        // x-c376: peek sits ON TOP of the selector; routed BEFORE it so its keys
        // (j/k, Esc, later digit/attach) never leak to the selector underneath.
        return peek_keys(view, &passthrough, sock_w).await;
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
    if view.rename.is_some() {
        // Same precedence slot as create_keys: AFTER selector/answers, so a
        // lingering overlay never swallows the typed name (x-9e5e finding).
        return rename_keys(view, &passthrough, sock_w).await;
    }
    if view.recruit.is_some() {
        return recruit_keys(view, &passthrough, sock_w).await;
    }
    if view.search.is_some() {
        return search_keys(view, &passthrough, sock_w).await;
    }
    if view.nav.is_some() {
        return nav_keys(view, &passthrough, sock_w).await;
    }
    for event in scanner.scan(&passthrough) {
        match dispatch_event(view, event, sock_w).await? {
            DispatchFlow::Continue => {}
            DispatchFlow::Break => break,
            DispatchFlow::Detach => return Ok(StdinFlow::Detach),
        }
    }
    Ok(StdinFlow::Continue)
}

/// One of three control-flow outcomes of dispatching a leader event: fall
/// through to the next event, stop consuming this chunk (a chord that opens a
/// typing mode must not leak the chunk's trailing bytes into a pane), or detach.
enum DispatchFlow {
    Continue,
    Break,
    Detach,
}

/// Dispatch one resolved leader [`Event`] to the wire / view state - the single
/// executor the key-scan loop and the which-key modal both call (x-8ccf Locked
/// 3), so a modal-executed chord runs the IDENTICAL path as a directly-typed one
/// (no parallel keymap to drift).
async fn dispatch_event(
    view: &mut View,
    event: Event,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
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
        Event::Detach => return Ok(DispatchFlow::Detach),
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
                // Open at the top: a stale offset from a prior session must
                // not hide row 0 (x-a621).
                view.sideline_offset = 0;
            }
        }
        Event::OpenAnswers => {
            // x-feec: open the needs-me queue. Always opens (even with an
            // empty live leg) so the async event-fold leg can populate it;
            // an ultimately-empty union renders "nothing needs you". The
            // fold merges in when it lands - the overlay never blocks on it.
            view.answers = Some(0);
            view.ans_esc.clear();
            view.needs_gen = view.needs_gen.wrapping_add(1);
            let fresh = view
                .needs_fold_at
                .is_some_and(|t| t.elapsed() < NEEDS_CACHE_TTL);
            if fresh {
                // Re-open within the cache TTL: reuse the last fold instantly
                // (Perspective B - mashing leader+a never re-shells).
                view.needs_degraded = false;
            } else {
                // Stale/first open: live-only until the refresh lands.
                view.needs_fold = None;
                view.needs_degraded = false;
                view.needs_want = true;
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
            view.open_keys_modal();
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
            return Ok(DispatchFlow::Break);
        }
        Event::OpenNav => {
            // Client-local overlay (x-653d): opening sends nothing and
            // reserves no row (it draws over the content top-left like the
            // answer overlay, not the bottom chrome). Break so same-chunk
            // bytes after the chord can't leak to the pane (like SearchOpen).
            // No width gate: draw_lines_overlay clips a tiny terminal, and a
            // zero-squad session shows an explicit `no matches` (AC1-EDGE).
            view.nav = Some(NavView {
                query: String::new(),
                state_filter: None,
                cursor: 0,
            });
            view.nav_esc.clear();
            return Ok(DispatchFlow::Break);
        }
        Event::OpenRename => {
            // Rename targets the ACTIVE tab, resolved to its stable id at
            // open time so a tab switch mid-edit cannot retarget the send
            // (the server refuses a stale id fail-closed - AC1-FR).
            let tab = view
                .layout
                .squads
                .iter()
                .find(|s| s.id == view.layout.active_squad)
                .and_then(|s| s.tabs.get(s.active_tab))
                .map(|t| t.id);
            match tab {
                Some(id) => {
                    view.open_rename(RenameTarget::Tab(id));
                    // Swallow same-chunk bytes after the chord, like
                    // SearchOpen: nothing may leak into the pane.
                    return Ok(DispatchFlow::Break);
                }
                None => {
                    let _ = raw_out(b"\x07");
                }
            }
        }
        Event::ReorderTab(delta) => {
            let target = view
                .layout
                .squads
                .iter()
                .find(|s| s.id == view.layout.active_squad)
                .and_then(|s| s.tabs.get(s.active_tab).map(|tab| (s.id, tab.id)));
            match target {
                Some((squad, tab)) => {
                    write_msg(
                        sock_w,
                        &ClientMsg::Command(Command::ReorderTab { squad, tab, delta }),
                    )
                    .await
                    .map_err(|e| format!("command send failed: {e}"))?;
                }
                None => {
                    let _ = raw_out(b"\x07");
                }
            }
            return Ok(DispatchFlow::Break);
        }
        Event::Bell => {
            let _ = raw_out(b"\x07");
        }
    }
    Ok(DispatchFlow::Continue)
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
        // Pure state flip, no I/O - usable even when the socket write path
        // is failing (x-2f99, AC1-FR).
        ChromeHit::ToggleExpand(id) => view.toggle_expand(id),
        // x-8ccf US4: open the sideline MENU popup anchored at the clicked cell.
        ChromeHit::OpenSidelineMenu { row, col } => {
            view.open_sideline_menu(Anchor::At { row, col })
        }
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
        let cmd = match action.action {
            ConfirmKind::Dispatch { node } => Command::DispatchNode(node),
            ConfirmKind::RemoveSquad { squad, .. } => Command::RemoveSquad(squad),
            ConfirmKind::StopAgent { name } => Command::StopAgent { name },
            ConfirmKind::RemoveAgent { name } => Command::RemoveAgent { name },
            ConfirmKind::ReapAgents => Command::ReapAgents,
            ConfirmKind::StopExternal { attach_id, name } => {
                Command::StopExternal { attach_id, name }
            }
            ConfirmKind::RemoveExternal { attach_id, name } => {
                Command::RemoveExternal { attach_id, name }
            }
        };
        write_msg(sock_w, &ClientMsg::Command(cmd))
            .await
            .map_err(|e| format!("confirm-action send failed: {e}"))?;
    }
    Ok(StdinFlow::Continue)
}

/// A folded which-key modal key (x-8ccf US3). Arrows/pgup navigate the
/// reference; `Byte`/`Enter` execute; `Esc` dismisses. Distinct from
/// [`fold_selector_keys`] because the modal needs arrows kept as navigation
/// (not folded to hjkl, which are executable bindings) and pgup/pgdn as scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalKey {
    Byte(u8),
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
}

/// Fold raw modal-mode bytes into [`ModalKey`]s, carrying escape state in `esc`
/// ACROSS reads (same split-arrow safety as [`fold_selector_keys`]). Arrows and
/// PageUp/PageDown become navigation tokens; a bare Esc (a lone `0x1b` chunk is
/// special-cased by the caller for instant close) becomes `Esc`; every other
/// printable byte is `Byte`, resolved by the caller through the chord table.
fn fold_modal_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<ModalKey> {
    let mut out = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            match (esc.as_slice(), b) {
                ([0x1b], b'[') => {
                    esc.push(b);
                    continue;
                }
                ([0x1b], _) => {
                    // The pending ESC was a bare Esc press; emit it, then let the
                    // fresh byte fall through to be processed below.
                    out.push(ModalKey::Esc);
                    esc.clear();
                }
                ([0x1b, b'['], b'A') => {
                    out.push(ModalKey::Up);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'B') => {
                    out.push(ModalKey::Down);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'C') => {
                    out.push(ModalKey::Right);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'D') => {
                    out.push(ModalKey::Left);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'5') | ([0x1b, b'['], b'6') => {
                    esc.push(b); // PageUp `ESC[5~` / PageDown `ESC[6~` pending
                    continue;
                }
                ([0x1b, b'[', b'5'], b'~') => {
                    out.push(ModalKey::PageUp);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'[', b'6'], b'~') => {
                    out.push(ModalKey::PageDown);
                    esc.clear();
                    continue;
                }
                _ => {
                    // Unknown escape tail: swallow it whole (never leak).
                    esc.clear();
                    continue;
                }
            }
        }
        match b {
            0x1b => esc.push(0x1b),
            b'\r' | b'\n' => out.push(ModalKey::Enter),
            _ => out.push(ModalKey::Byte(b)),
        }
    }
    out
}

/// Which-key modal keys (x-8ccf US3). Esc closes; arrows/pgup scroll+select;
/// Enter/`click` run the selected row; a bound printable key runs immediately
/// through the shared chord dispatch (which-key), an unbound one dismisses. Esc
/// is folded like every other overlay (carried across reads) so a split arrow
/// sequence can never leak its tail into a pane (codex P2). No key ever reaches
/// a pane.
async fn keys_modal_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.keys_modal_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.keys_modal_esc = esc;
    for tok in toks {
        if view.keys_modal.is_none() {
            break; // closed mid-chunk: swallow the rest, never forward
        }
        match tok {
            ModalKey::Esc => view.keys_modal = None,
            ModalKey::Up => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Up);
                }
                view.follow_modal_selection();
            }
            ModalKey::Down => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Down);
                }
                view.follow_modal_selection();
            }
            ModalKey::Left => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            ModalKey::PageUp => {
                let (page, trows) = ((view.term.0 as isize - 2).max(1), view.term.0 as usize);
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.scroll_by(-page);
                    m.popup.clamp_sel_to_view(trows); // Enter never runs an off-screen row
                }
            }
            ModalKey::PageDown => {
                let (page, trows) = ((view.term.0 as isize - 2).max(1), view.term.0 as usize);
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.scroll_by(page);
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::Enter => {
                if matches!(
                    keys_modal_execute_selected(view, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            ModalKey::Byte(b) => match resolve_chord(b) {
                // Unbound key dismisses (AC2-EDGE): no action fires.
                Event::Bell => view.keys_modal = None,
                // Bound key runs immediately through the SAME dispatch a typed
                // chord uses (Locked 3), then the modal closes.
                ev => {
                    view.keys_modal = None;
                    if matches!(
                        dispatch_event(view, ev, sock_w).await?,
                        DispatchFlow::Detach
                    ) {
                        return Ok(StdinFlow::Detach);
                    }
                }
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Run the modal's selected row (Enter/click) through the shared dispatch, then
/// close - a header/meta row with no chord BELs and stays open (nothing ran, so
/// the "execute always closes" invariant is not tripped). Returns the dispatch
/// flow so a detach chord (leader+d) run from the modal actually detaches.
async fn keys_modal_execute_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    let ev = view.keys_modal.as_ref().and_then(|m| {
        m.popup
            .selected()
            .and_then(|(ri, _)| m.row_events.get(ri).cloned().flatten())
    });
    match ev {
        Some(ev) => {
            view.keys_modal = None;
            dispatch_event(view, ev, sock_w).await
        }
        None => {
            let _ = raw_out(b"\x07");
            Ok(DispatchFlow::Continue)
        }
    }
}

/// One mouse report while the which-key modal is open (x-8ccf US3): hover moves
/// the selection, the wheel scrolls, a left click on a row runs it, a click off
/// the popup dismisses (herdr click-elsewhere).
async fn keys_modal_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.keys_modal_hit(rep.row, rep.col) {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::WheelUp => {
            if let Some(m) = view.keys_modal.as_mut() {
                m.popup.scroll_by(-3);
            }
        }
        MouseKind::WheelDown => {
            if let Some(m) = view.keys_modal.as_mut() {
                m.popup.scroll_by(3);
            }
        }
        MouseKind::Press(MouseButton::Left) => match view.keys_modal_hit(rep.row, rep.col) {
            Some(t) => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.select(t);
                }
                if matches!(
                    keys_modal_execute_selected(view, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            None => view.keys_modal = None, // click off the popup dismisses
        },
        _ => {}
    }
    Ok(StdinFlow::Continue)
}

/// Run a row-menu entry (x-8ccf US2) against the LIVE agent row (resolved by the
/// pinned identity). A stale OR ambiguous target is a Notice (AC1-ERR / codex
/// P1), never a misrouted action; every action maps to an existing Command /
/// overlay / confirm (zero proto).
async fn execute_row_menu_action(
    view: &mut View,
    action: MenuAction,
    target: AgentIdent,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    // Fail closed unless the identity resolves to EXACTLY one live row: two rows
    // sharing a name must never let a menu act on the wrong one (codex P1).
    let mut hits = view.layout.agents.iter().filter(|a| target.matches(a));
    let a = match (hits.next(), hits.next()) {
        (Some(a), None) => a.clone(),
        _ => {
            view.set_notice(format!("agent {} is no longer uniquely here", target.name));
            return Ok(());
        }
    };
    match action {
        MenuAction::OpenHere => {
            let Some(id) = a.attach_id.clone() else {
                view.set_notice("agent is no longer attachable".into());
                return Ok(());
            };
            write_msg(sock_w, &ClientMsg::Command(Command::attach_agent_here(id)))
                .await
                .map_err(|e| format!("attach send failed: {e}"))?;
        }
        MenuAction::NewTab | MenuAction::Split(_) => {
            let Some(id) = a.attach_id.clone() else {
                view.set_notice("agent is no longer attachable".into());
                return Ok(());
            };
            let split = match action {
                MenuAction::Split(d) => Some(d),
                _ => None,
            };
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::AttachAgent {
                    id,
                    placement: PanePlacement {
                        target: PaneTarget::CurrentRoute,
                        split,
                        here: false,
                    },
                }),
            )
            .await
            .map_err(|e| format!("attach send failed: {e}"))?;
        }
        MenuAction::Focus => match a.pane_id {
            Some(pid) => write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pid)))
                .await
                .map_err(|e| format!("focus send failed: {e}"))?,
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::Peek => {
            let idx = view
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Agent(x) if target.matches(x)));
            match idx {
                Some(idx) => fetch_peek(view, idx, a.name.clone(), sock_w).await?,
                None => view.set_notice("agent is no longer here".into()),
            }
        }
        MenuAction::Stop | MenuAction::Remove => {
            // A confirm owns the bottom row; a too-short terminal refuses rather
            // than arm an invisible prompt (matching the selector's stop/reap).
            if view.term.0 < MIN_ROWS_FOR_STATUS {
                view.set_notice("terminal too short for the confirm prompt".into());
                return Ok(());
            }
            let kind = match (action, a.external, a.attach_id.clone()) {
                (MenuAction::Stop, true, Some(id)) => ConfirmKind::StopExternal {
                    attach_id: id,
                    name: a.name.clone(),
                },
                (MenuAction::Stop, _, _) => ConfirmKind::StopAgent {
                    name: a.name.clone(),
                },
                (MenuAction::Remove, true, Some(id)) => ConfirmKind::RemoveExternal {
                    attach_id: id,
                    name: a.name.clone(),
                },
                (_, _, _) => ConfirmKind::RemoveAgent {
                    name: a.name.clone(),
                },
            };
            view.open_confirm(ConfirmAction {
                action: kind,
                label: a.name.clone(),
            });
        }
    }
    Ok(())
}

/// Run the row menu's selected entry (Enter/click), then close - the popup never
/// lingers after execute (AC1-FR).
async fn row_menu_execute_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let picked = view.row_menu.as_ref().and_then(|m| {
        m.actions
            .get(m.popup.sel)
            .copied()
            .map(|a| (a, m.target.clone()))
    });
    view.row_menu = None;
    if let Some((action, target)) = picked {
        execute_row_menu_action(view, action, target, sock_w).await?;
    }
    Ok(())
}

/// Row-menu keys (x-8ccf US2): arrows walk the entries + 2x2 grid (scrolling to
/// keep the selection on-screen), pgup/pgdn scroll, Enter runs the selection,
/// Esc/`q`/any unbound key dismiss (the shared popup contract, codex P2). Esc is
/// carried across reads like every overlay, so a split arrow never leaks; no key
/// reaches a pane.
async fn row_menu_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let trows = view.term.0 as usize;
    let mut esc = std::mem::take(&mut view.row_menu_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.row_menu_esc = esc;
    for tok in toks {
        if view.row_menu.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc => view.row_menu = None,
            ModalKey::Up => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Up);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Down => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Down);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Left => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            ModalKey::PageUp => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.scroll_by(-(trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::PageDown => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.scroll_by((trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::Enter => row_menu_execute_selected(view, sock_w).await?,
            // Any other (unbound) key dismisses, per the shared popup contract.
            ModalKey::Byte(_) => view.row_menu = None,
        }
    }
    Ok(StdinFlow::Continue)
}

/// One mouse report while the row menu is open (x-8ccf US2): hover selects, a
/// left click runs the entry, a right press re-anchors on the row under the
/// pointer (or dismisses off the sideline), a click off the popup dismisses.
async fn row_menu_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.row_menu_hit(rep.row, rep.col) {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::Press(MouseButton::Left) => match view.row_menu_hit(rep.row, rep.col) {
            Some(t) => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.select(t);
                }
                row_menu_execute_selected(view, sock_w).await?;
            }
            None => view.row_menu = None,
        },
        MouseKind::Press(MouseButton::Right) => match view.sideline_row_at(rep.row, rep.col) {
            // Re-anchor on the row under the second right-press (never stack two
            // menus); a non-agent row leaves nothing open.
            Some(i) => {
                if !view.open_row_menu(
                    i,
                    Anchor::At {
                        row: rep.row,
                        col: rep.col,
                    },
                ) {
                    view.row_menu = None;
                }
            }
            None => view.row_menu = None,
        },
        _ => {}
    }
    Ok(())
}

/// Run one aux-popup action (x-8ccf US4/US5). Menu entries open a surface or
/// detach; settings toggles flip a session-local view flag and rebuild the modal
/// so its glyph reflects the new state (the popup stays open for another toggle).
async fn execute_aux_action(
    view: &mut View,
    action: AuxAction,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    match action {
        AuxAction::OpenKeybinds => {
            view.aux = None;
            view.open_keys_modal();
        }
        AuxAction::OpenSettings => view.aux = Some(view.build_settings_modal()),
        AuxAction::OpenConnections => {
            // x-84d7: close the MENU and open the Connections modal in its
            // loading state; arm the first read (the run loop spawns it).
            view.aux = None;
            view.open_connections();
        }
        AuxAction::Detach => {
            view.aux = None;
            return Ok(DispatchFlow::Detach);
        }
        AuxAction::ToggleHoverFocus => {
            view.hover_focus = !view.hover_focus;
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ToggleStatus => {
            view.status_on = !view.status_on;
            // The status row changed the content area; report the new size so the
            // panes reflow (same accounting as Event::ToggleStatus).
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
            view.reopen_settings_keeping_sel();
        }
    }
    Ok(DispatchFlow::Continue)
}

/// Run the aux popup's selected row (Enter/click), propagating a detach.
async fn aux_execute_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    let picked = view
        .aux
        .as_ref()
        .and_then(|m| m.actions.get(m.popup.sel).copied());
    match picked {
        Some(a) => execute_aux_action(view, a, sock_w).await,
        None => {
            let _ = raw_out(b"\x07");
            Ok(DispatchFlow::Continue)
        }
    }
}

/// Aux-popup keys (US4/US5): arrows select (scrolling to keep the selection
/// visible), pgup/pgdn scroll, Enter runs, Esc/`q`/any unbound key dismiss (the
/// shared popup contract, codex P2); a detach entry propagates StdinFlow::Detach.
/// Esc is carried across reads so a split arrow never leaks into a pane.
async fn aux_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let trows = view.term.0 as usize;
    let mut esc = std::mem::take(&mut view.aux_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.aux_esc = esc;
    for tok in toks {
        if view.aux.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc => view.aux = None,
            ModalKey::Up => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Up);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Down => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Down);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Left => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            ModalKey::PageUp => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.scroll_by(-(trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::PageDown => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.scroll_by((trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::Enter => {
                if matches!(
                    aux_execute_selected(view, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            // Any other (unbound) key dismisses, per the shared popup contract.
            ModalKey::Byte(_) => view.aux = None,
        }
    }
    Ok(StdinFlow::Continue)
}

/// One mouse report while an aux popup is open (US4/US5): hover selects, a left
/// click runs the entry (propagating detach), a click off the popup dismisses.
async fn aux_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.aux_hit(rep.row, rep.col) {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::Press(MouseButton::Left) => match view.aux_hit(rep.row, rep.col) {
            Some(t) => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.select(t);
                }
                if matches!(
                    aux_execute_selected(view, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            None => view.aux = None,
        },
        _ => {}
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

/// Open (or move) the peek overlay to `cursor` and fetch its transcript: bumps
/// the seq, resets the body to loading, and sends the matching `PeekAgent`. The
/// caller guarantees `cursor` is a `DisplayRow::Agent`. Shared by Space-open,
/// j/k, and the layout re-anchor so the seq/loading discipline is identical on
/// every path (x-c376).
async fn fetch_peek(
    view: &mut View,
    cursor: usize,
    name: String,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let seq = view.open_peek(cursor, name.clone());
    write_msg(
        sock_w,
        &ClientMsg::Command(Command::PeekAgent { name, seq }),
    )
    .await
    .map_err(|e| format!("peek send failed: {e}"))
}

/// Peek-overlay keys (x-c376): j/k (and folded arrows) peek the adjacent agent
/// row (fresh seq, stale bodies dropped by the seq guard); Esc/q closes back to
/// the selector with its cursor synced to the peeked row (AC2-UI). Digit answers
/// (US3) and attach (US4) are added by later stories; until then those keys are
/// swallowed - no key in peek mode ever reaches a pane (the leader-layer
/// invariant). The catalog is re-read per key so a scrape tick that removed the
/// peeked row re-anchors or closes (never a panic on a dropped index).
/// (x-84d7) Route keys to the Connections modal. Pure state changes run through
/// the modal's own reducer ([`ConnectionsView::on_key`]); the intents that touch
/// the world (close, refresh) are executed here. The run loop redraws after this
/// returns `Continue`, so a `Redraw` intent needs no explicit paint.
async fn connections_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    use crate::connections_view::ConnIntent;
    let mut esc = std::mem::take(&mut view.conn_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.conn_esc = esc;
    for &k in &keys {
        // Closed mid-chunk: swallow the rest, never forward to a pane.
        if view.connections.is_none() {
            break;
        }
        let intent = view
            .connections
            .as_mut()
            .map(|cv| cv.on_key(k))
            .unwrap_or(ConnIntent::Bell);
        match intent {
            ConnIntent::Redraw => {}
            ConnIntent::Bell => {
                let _ = raw_out(b"\x07");
            }
            ConnIntent::Close => view.close_connections(),
            ConnIntent::Refresh => view.refresh_connections(),
            ConnIntent::Run(argv) => {
                // The reducer already armed `acting`; stash the argv for the run
                // loop to spawn at loop top (single-flight, sender in scope there).
                view.conn_action = Some((argv, Vec::new(), false));
            }
            ConnIntent::RunEnv { argv, env } => {
                view.conn_action = Some((argv, env, false));
            }
            ConnIntent::SpawnLogin(argv) => {
                // Opens the login pane via `fno mux pane run`; the reducer already
                // recorded the pending row + notice. Marked is_login so a success
                // keeps that notice.
                view.conn_action = Some((argv, Vec::new(), true));
            }
        }
    }
    Ok(StdinFlow::Continue)
}

async fn peek_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.peek_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.peek_esc = esc;
    for &k in &keys {
        let Some(cursor) = view.peek.as_ref().map(|p| p.cursor) else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        match k {
            b'j' | b'k' => {
                let dir = if k == b'j' { 1 } else { -1 };
                match view.peek_next_agent(cursor, dir) {
                    Some(next) => {
                        let name = match view.display_rows().get(next) {
                            Some(DisplayRow::Agent(a)) => Some(a.name.clone()),
                            _ => None,
                        };
                        if let Some(name) = name {
                            fetch_peek(view, next, name, sock_w).await?;
                        }
                    }
                    None => {
                        let _ = raw_out(b"\x07"); // at the edge: BEL, stay put
                    }
                }
            }
            b'0'..=b'9' => {
                // Answer a blocked peeked row in place (x-c929 reuse): send the
                // EXACT PaneAnswer payload (fingerprint, region_lines, keystroke)
                // only when the row is answerable AND pane-hosted; else BEL,
                // nothing sent (x-c929 AC1-ERR carried over). The overlay stays
                // open; the answered row drops from blocked on the next scrape
                // tick. The daemon-pinned keystroke is relayed opaquely - the
                // client never fabricates bytes.
                let payload = match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => {
                        a.answerable
                            .as_ref()
                            .zip(a.pane_id)
                            .and_then(|(ans, pane)| {
                                ans.options
                                    .iter()
                                    .find(|o| o.idx.as_bytes().first() == Some(&k))
                                    .map(|o| {
                                        (
                                            pane,
                                            ans.fingerprint,
                                            ans.region_lines as u16,
                                            o.keystroke.clone(),
                                        )
                                    })
                            })
                    }
                    _ => None,
                };
                match payload {
                    Some((pane, fingerprint, region_lines, keystroke)) => {
                        write_msg(
                            sock_w,
                            &ClientMsg::PaneAnswer {
                                pane,
                                fingerprint,
                                region_lines,
                                keystroke,
                            },
                        )
                        .await
                        .map_err(|e| format!("answer send failed: {e}"))?;
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'l' | b'\r' | b'\n' => {
                // Attach from peek (US4): resolve the peeked row through the same
                // agent_hit -> apply_hit path a selector Enter / sideline click
                // uses (FocusPane for a pane-hosted row, AttachAgent for a
                // watch-only row with an attach_id). A Notice refusal (a paneless
                // row with no attach target here) keeps BOTH overlays open
                // (x-260a locked 3); a real hit closes peek AND the selector
                // underneath. Right-arrow is already folded to `l`.
                let hit = match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => Some(agent_hit(a, view.layout.active_squad)),
                    _ => None,
                };
                match hit {
                    Some(ChromeHit::Notice(msg)) => view.set_notice(msg.to_string()),
                    Some(hit) => {
                        view.clear_peek();
                        view.selector = None;
                        apply_hit(view, hit, sock_w).await?;
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            0x1b | b'q' => {
                // Close peek. When peek was opened FROM the selector it stays
                // open underneath, so re-point its cursor to the peeked row
                // (AC2-UI). When peek was opened standalone (x-8ccf US2:
                // right-click a row -> Peek, selector closed), Esc must return to
                // normal pane input, NOT drop into panel-selector mode.
                let restore = view.selector.is_some();
                view.clear_peek();
                if restore {
                    view.selector = Some(cursor);
                }
            }
            // Everything else is swallowed - never a pane leak (leader-layer
            // invariant). h (left-arrow) has no peek action.
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Selector-mode keys: j/k (and arrows) move over the unified display rows,
/// skipping inert Headers; h/l (and left/right) collapse/expand squad rows;
/// Enter acts on the row through [`View::row_action`] + [`apply_hit`] - the
/// same resolver a mouse click uses (x-260a), so squad/tab switch, agent
/// focus/attach, card dispatch-confirm, and workspace-create are all keyboard
/// reachable. The x-96e8 squad-management context keys ride here too: on a
/// squad row `r` renames, `x` removes (behind a confirm), `J`/`K` reorder; on
/// a tab row `m` opens the move-to-squad picker. A refusal (Notice/BEL) keeps
/// the selector open; Esc/q closes. Rows and cursor are re-read per key so a
/// close mid-chunk swallows the remainder instead of resurrecting the selector.
/// Detach is leader+d from NORMAL mode only (Locked 11): close the selector.
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
        // Any key other than a J/K reorder drops the cursor-follow intent, so a
        // later Layout re-anchors normally instead of chasing a stale squad.
        if k != b'J' && k != b'K' {
            view.sel_follow = None;
        }
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
            b'r' => {
                // Rename the squad at the cursor (x-96e8). Tab/other rows have
                // no squad rename here (leader+, renames a tab), so they BEL.
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                match squad {
                    Some(sq) => view.open_rename(RenameTarget::Squad(sq)),
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b' ' => {
                // Open the read-only peek overlay on the focused agent row
                // (x-c376): its status sentence + recent transcript, read from
                // disk (peek/logs read disk; only attach spawns a pane). Any
                // non-agent row BELs (selector convention). The selector stays
                // open underneath; Esc drops back into it at the peeked row.
                let name = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) => Some(a.name.clone()),
                    _ => None,
                };
                match name {
                    Some(name) => fetch_peek(view, cur, name, sock_w).await?,
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'\t' => {
                // Toggle a recruit mark on the focused row (x-8f11; moved from
                // Space to Tab by x-c376, which took Space for peek). Markable
                // only if it is an attachable watch-only agent (live, has an
                // attach_id); anything else gives a notice, never zero feedback.
                let id = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if a.attach_id.is_some() && !a.exited => {
                        a.attach_id.clone()
                    }
                    _ => None,
                };
                match id {
                    Some(id) => {
                        if !view.marks.remove(&id) {
                            view.marks.insert(id);
                        }
                    }
                    None => view.set_notice("not attachable".into()),
                }
            }
            b'R' => {
                // Open the recruit name prompt for the marked rows (x-8f11). With
                // no marks, fall back to marking the focused attachable row first
                // (the grid's single-recruit `m`, generalized); a non-attachable
                // focused row with no marks BELs.
                if view.marks.is_empty() {
                    let id = match view.display_rows().get(cur) {
                        Some(DisplayRow::Agent(a)) if a.attach_id.is_some() && !a.exited => {
                            a.attach_id.clone()
                        }
                        _ => None,
                    };
                    match id {
                        Some(id) => {
                            view.marks.insert(id);
                            view.open_recruit();
                        }
                        None => {
                            let _ = raw_out(b"\x07");
                        }
                    }
                } else {
                    view.open_recruit();
                }
            }
            b'p' => {
                let picked = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a))
                        if a.pane_id.is_none() && a.attach_id.is_some() && !a.exited =>
                    {
                        Some((a.attach_id.clone().unwrap(), a.squad))
                    }
                    _ => None,
                };
                let squads: Vec<u64> = view.layout.squads.iter().map(|s| s.id).take(9).collect();
                match picked.filter(|_| !squads.is_empty()) {
                    Some((id, owner)) => {
                        let target = owner
                            .filter(|sid| squads.contains(sid))
                            .or_else(|| {
                                squads
                                    .contains(&view.layout.active_squad)
                                    .then_some(view.layout.active_squad)
                            })
                            .unwrap_or(squads[0]);
                        view.open_attach_place(id, target, squads);
                    }
                    None => view.set_notice("placement requires an attachable agent".into()),
                }
            }
            b'X' => {
                // Bulk reap (x-7561): uppercase `X` from ANY agent row confirms
                // `fno-agents reap`. Contextual on agent rows only (headers stay
                // inert - no selector surgery); a non-agent row BELs. Too-short
                // terminal refuses rather than arm an invisible confirm (x-260a).
                let on_agent = matches!(view.display_rows().get(cur), Some(DisplayRow::Agent(_)));
                if !on_agent {
                    let _ = raw_out(b"\x07");
                } else if view.term.0 < MIN_ROWS_FOR_STATUS {
                    view.set_notice("terminal too short for the confirm prompt".into());
                } else {
                    view.open_confirm(ConfirmAction {
                        action: ConfirmKind::ReapAgents,
                        label: String::new(),
                    });
                }
                continue;
            }
            b'x' => {
                // A TOMBSTONE member row dismisses (x-8f11); a squad-header row
                // removes the squad (x-96e8), behind a confirm - disambiguated by
                // row type so one key serves both. A too-short terminal cannot
                // render the bottom-row prompt, so it refuses with a notice rather
                // than arm an INVISIBLE confirm (x-260a); an unknown squad or a
                // tab/other row BELs.
                let dismiss = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if a.tombstone => a.squad.zip(a.attach_id.clone()),
                    _ => None,
                };
                if let Some((squad, attach_id)) = dismiss {
                    write_msg(
                        sock_w,
                        &ClientMsg::Command(Command::DismissMember { squad, attach_id }),
                    )
                    .await
                    .map_err(|e| format!("dismiss send failed: {e}"))?;
                    continue;
                }
                // A WATCH-ONLY (paneless) agent row gets the lifecycle verb
                // (x-76ea): a live row (`!exited`) stops, an exited row removes.
                // The registry poll's state flip IS the stage separator - stop,
                // wait ≤1s for the row to flip exited, then `x` again removes (no
                // double-tap timer). The captured name (not the row index) rides
                // the confirm, so a row that races out resolves to the server's
                // stale-name refusal; too-short terminal refuses rather than arm
                // an invisible confirm (x-260a), like RemoveSquad.
                //
                // `pane_id.is_none()` is load-bearing (codex review): a PANE-hosted
                // Agent row is either a real agent's pane or a bare shell pane that
                // agent_rows() surfaces as a first-class Agent row labelled from its
                // cmd/cwd - NOT a registry entry. Arming the verb there would shell
                // `fno-agents` on a label that could collide with an unrelated
                // agent's name and stop the wrong one. Pane-hosted rows are managed
                // via their tab (CloseTab); only the paneless rows - the bg/headless
                // agents that today linger until GC - are the gap this closes.
                //
                // An EXTERNAL row (claude-daemon roster or a persisted external
                // tombstone, x-7561) routes by stable `attach_id` to the
                // External verbs instead of `fno-agents` by name: a live row
                // (`!exited`) stops (`claude stop <id>`), a stopped tombstone
                // (`exited`) removes (`claude rm <id>`). The server re-validates
                // the exact id + gates rm on a persisted `stopped` state; a
                // failed/unknown tombstone renders `!exited` so its `x` retries
                // the stop. An external row without an attach_id (degenerate)
                // falls through to the by-name path, which the server refuses.
                let agent = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if !a.tombstone && a.pane_id.is_none() => {
                        Some((a.name.clone(), a.exited, a.external, a.attach_id.clone()))
                    }
                    _ => None,
                };
                if let Some((name, exited, external, attach_id)) = agent {
                    if view.term.0 < MIN_ROWS_FOR_STATUS {
                        view.set_notice("terminal too short for the confirm prompt".into());
                        continue;
                    }
                    let action = match (external, attach_id) {
                        (true, Some(id)) if exited => ConfirmKind::RemoveExternal {
                            attach_id: id,
                            name: name.clone(),
                        },
                        (true, Some(id)) => ConfirmKind::StopExternal {
                            attach_id: id,
                            name: name.clone(),
                        },
                        _ if exited => ConfirmKind::RemoveAgent { name: name.clone() },
                        _ => ConfirmKind::StopAgent { name: name.clone() },
                    };
                    view.open_confirm(ConfirmAction {
                        action,
                        label: name,
                    });
                    continue;
                }
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                match squad.and_then(|sq| {
                    view.layout
                        .squads
                        .iter()
                        .find(|s| s.id == sq)
                        .map(|s| (sq, s.name.clone(), s.panes))
                }) {
                    Some(_) if view.term.0 < MIN_ROWS_FOR_STATUS => {
                        view.set_notice("terminal too short for the confirm prompt".into())
                    }
                    Some((sq, name, panes)) => {
                        let last = view.layout.squads.len() == 1;
                        view.open_confirm(ConfirmAction {
                            action: ConfirmKind::RemoveSquad {
                                squad: sq,
                                panes,
                                last,
                            },
                            label: name,
                        });
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'J' | b'K' => {
                // Reorder the squad at the cursor down (`J`) / up (`K`) the
                // sideline (x-96e8). The cursor follows the squad via sel_follow
                // on the authoritative next Layout. Tab/other rows BEL.
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                match squad {
                    Some(sq) => {
                        let delta = if k == b'J' { 1 } else { -1 };
                        view.sel_follow = Some(sq);
                        write_msg(
                            sock_w,
                            &ClientMsg::Command(Command::MoveSquad { squad: sq, delta }),
                        )
                        .await
                        .map_err(|e| format!("move-squad send failed: {e}"))?;
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'm' => {
                // x-8ccf US2: `m` on an AGENT row opens its context menu
                // (mouse-off parity), anchored at the row and sitting over the
                // selector like peek; Esc drops back into the selector.
                if matches!(view.display_rows().get(cur), Some(DisplayRow::Agent(_))) {
                    let arow =
                        (TAB_BAR_ROWS as usize + cur.saturating_sub(view.sideline_offset)) as u16;
                    view.open_row_menu(cur, Anchor::At { row: arow, col: 1 });
                    continue;
                }
                // Move a tab into another squad (x-96e8): open the numbered
                // picker over the OTHER squads (a squad is moved with J/K, not
                // m). Tab rows left the sideline (x-0090), so `m` on a squad row
                // targets that squad's ACTIVE tab - the one shown in the tab bar.
                // A non-squad row, or nowhere to move to (one squad), BELs.
                let picked = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                }
                .and_then(|squad| {
                    let sq = view.layout.squads.iter().find(|s| s.id == squad)?;
                    let tid = sq.tabs.get(sq.active_tab).or_else(|| sq.tabs.first())?.id;
                    let dsts: Vec<u64> = view
                        .layout
                        .squads
                        .iter()
                        .filter(|s| s.id != squad)
                        .map(|s| s.id)
                        .take(9)
                        .collect();
                    (!dsts.is_empty()).then_some((tid, dsts))
                });
                match picked {
                    Some((tid, dsts)) => view.open_move_pick(tid, dsts),
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            0x1b | b'q' => view.selector = None,
            _ => {}
        }
    }
    // Follow the (possibly moved) cursor / expanded catalog into the scroll
    // window so a row driven below the fold stays visible (x-a621).
    view.clamp_sideline_offset();
    Ok(StdinFlow::Continue)
}

/// Move-tab picker keys (x-96e8): a digit `1..=9` selects the numbered
/// destination squad and sends [`Command::MoveTab`]; the captured id is
/// re-validated against the CURRENT catalog first (stale -> BEL + close, the
/// server refuses a stale id regardless). Esc/q cancels; any other key closes
/// without acting. `take()` clears the picker either way, so a stale overlay
/// can never resurrect a second move.
async fn move_pick_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some((tab, squads)) = view.move_pick.take() else {
        return Ok(StdinFlow::Continue);
    };
    match bytes.first() {
        Some(&b) if (b'1'..=b'9').contains(&b) => {
            let idx = (b - b'1') as usize;
            match squads.get(idx) {
                // The captured id must still name a live squad; the server
                // refuses a stale id regardless, but pre-validating saves a
                // round-trip and keeps the BEL local.
                Some(&sq) if view.layout.squads.iter().any(|s| s.id == sq) => {
                    write_msg(
                        sock_w,
                        &ClientMsg::Command(Command::MoveTab { tab, squad: sq }),
                    )
                    .await
                    .map_err(|e| format!("move-tab send failed: {e}"))?;
                }
                _ => {
                    let _ = raw_out(b"\x07");
                }
            }
        }
        // Esc/q cancel silently; any other key just closes (the picker is
        // single-shot, cleared by take() above).
        _ => {}
    }
    Ok(StdinFlow::Continue)
}

async fn attach_place_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let keys = {
        let Some(picker) = view.attach_place.as_mut() else {
            return Ok(StdinFlow::Continue);
        };
        let mut esc = std::mem::take(&mut picker.esc);
        let keys = fold_selector_keys(&mut esc, bytes);
        picker.esc = esc;
        keys
    };

    for key in keys {
        if (b'1'..=b'9').contains(&key) {
            let idx = (key - b'1') as usize;
            let target = view
                .attach_place
                .as_ref()
                .and_then(|picker| picker.squads.get(idx))
                .copied();
            match target.filter(|sid| view.layout.squads.iter().any(|s| s.id == *sid)) {
                Some(target) => view.attach_place.as_mut().unwrap().target = target,
                None => {
                    view.set_notice("workspace is no longer available".into());
                    return Ok(StdinFlow::Continue);
                }
            }
            continue;
        }

        let split = match key {
            b'h' => Some(Some(Dir::Left)),
            b'j' => Some(Some(Dir::Down)),
            b'k' => Some(Some(Dir::Up)),
            b'l' => Some(Some(Dir::Right)),
            b't' | b'\r' | b'\n' => Some(None),
            0x1b | b'q' => {
                view.attach_place = None;
                return Ok(StdinFlow::Continue);
            }
            _ => None,
        };
        let Some(split) = split else { continue };
        let picker = view.attach_place.take().unwrap();
        let attachable = view.layout.agents.iter().any(|a| {
            a.pane_id.is_none() && !a.exited && a.attach_id.as_deref() == Some(picker.id.as_str())
        });
        if !attachable {
            view.set_notice("agent is no longer attachable".into());
            return Ok(StdinFlow::Continue);
        }
        if !view.layout.squads.iter().any(|s| s.id == picker.target) {
            view.set_notice("workspace is no longer available".into());
            return Ok(StdinFlow::Continue);
        }
        write_msg(
            sock_w,
            &ClientMsg::Command(Command::AttachAgent {
                id: picker.id,
                placement: PanePlacement {
                    target: PaneTarget::SquadId(picker.target),
                    split,
                    here: false,
                },
            }),
        )
        .await
        .map_err(|e| format!("attach placement send failed: {e}"))?;
        return Ok(StdinFlow::Continue);
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

/// Navigator fold keys. Superset of [`SearchKey`]: the same split-arrow escape
/// fold, but a completed CSI whose final byte is Up/Down/Shift-Tab surfaces as a
/// motion token instead of being swallowed (ab-63b44059). Every other CSI is
/// still consumed whole, so no escape tail leaks into the query or the pane.
enum NavKey {
    Byte(u8),
    Esc,
    Up,
    Down,
    ShiftTab,
}

/// Fold navigator-mode bytes. Identical escape-carry semantics to
/// [`fold_search_input`] (whole CSI consumed, split sequences carried across
/// reads via `esc`), except the arrow-Up `ESC [ A`, arrow-Down `ESC [ B`, and
/// Shift-Tab `ESC [ Z` finals become [`NavKey::Up`]/[`Down`]/[`ShiftTab`] so the
/// navigator can move its cursor and reverse-cycle the state chip. A modified
/// arrow (`ESC [ 1 ; 5 A`) shares the final byte and maps to the same motion -
/// harmless. All other finals are swallowed, same leak-safety as search.
fn fold_nav_input(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<NavKey> {
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(NavKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b);
                } else {
                    esc.clear();
                    keys.push(NavKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b);
                    } else {
                        keys.push(NavKey::Byte(b));
                    }
                }
            }
            _ => {
                if b == 0x1b {
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) {
                    // CSI complete. Surface the three motion finals; swallow the
                    // rest (whole sequence consumed either way - no leak).
                    match b {
                        b'A' => keys.push(NavKey::Up),
                        b'B' => keys.push(NavKey::Down),
                        b'Z' => keys.push(NavKey::ShiftTab),
                        _ => {}
                    }
                    esc.clear();
                } else if esc.len() >= 16 {
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

/// Navigator-overlay keys (x-653d): a client-owned typing overlay like search.
/// Printable bytes edit the text filter (Locked 5: letters are ALWAYS query
/// text, never state keys); Backspace widens; `Tab`/`Shift-Tab` cycle the state
/// chip forward/back; `Up`/`Down` (or `Ctrl-p`/`Ctrl-n`) move the cursor over the
/// filtered rows (clamped, no wrap); Enter goto's the row; Esc closes. Uses
/// [`fold_nav_input`]'s split-arrow fold (which surfaces the motion finals while
/// swallowing every other escape) and a per-key re-read so an Esc mid-chunk
/// swallows the chunk's remainder (ab-63b44059).
async fn nav_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.nav_esc);
    let keys = fold_nav_input(&mut esc, bytes);
    view.nav_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it and the rest of
        // the chunk must be swallowed, never forwarded.
        if view.nav.is_none() {
            break;
        }
        match key {
            NavKey::Esc => {
                view.nav = None;
                view.nav_esc.clear();
                break;
            }
            // Arrows mirror Ctrl-p/Ctrl-n; Shift-Tab reverses the state ring.
            NavKey::Up => view.nav_move_cursor(-1),
            NavKey::Down => view.nav_move_cursor(1),
            NavKey::ShiftTab => {
                view.nav_cycle_state_rev();
                view.nav_ring_if_empty();
            }
            NavKey::Byte(b) => match b {
                b'\r' | b'\n' => nav_goto(view, sock_w).await?,
                b'\t' => {
                    view.nav_cycle_state();
                    view.nav_ring_if_empty();
                }
                // Ctrl-n / Ctrl-p move the cursor (readline convention), kept
                // alongside the arrow tokens for muscle memory.
                0x0e => view.nav_move_cursor(1),
                0x10 => view.nav_move_cursor(-1),
                0x7f | 0x08 => {
                    if let Some(n) = view.nav.as_mut() {
                        n.query.pop();
                        n.cursor = 0;
                    }
                    view.nav_ring_if_empty();
                }
                // Printable ASCII edits the query; capped like search so a held
                // key / paste can't grow it unbounded. Cursor re-anchors to 0.
                0x20..=0x7e => {
                    if let Some(n) = view.nav.as_mut() {
                        if n.query.len() < MAX_SEARCH_QUERY {
                            n.query.push(b as char);
                            n.cursor = 0;
                        }
                    }
                    view.nav_ring_if_empty();
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Teleport to the navigator's cursor row (x-653d). Materializes the OWNED
/// target before mutating the view (`nav_rows` borrows the layout), re-reading
/// the filtered catalog at Enter time (per-key re-read; AC4-ERR relies on the
/// server refusing a stale id fail-closed). A refusal (`Notice`: blocked /
/// in-flight card, paneless agent) KEEPS the navigator open and shows the notice
/// (Locked 6), sending nothing. Otherwise it closes the overlay, switches squad
/// when the target lives in another one (a same-squad target collapses to a bare
/// hit), and applies the hit. Existing wire commands only - no new `Command`, no
/// proto bump (Locked 4).
async fn nav_goto(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let target = match view.nav.as_ref() {
        Some(n) => match view.nav_filtered(n).into_iter().nth(n.cursor) {
            Some(r) => r,
            // Empty/stale cursor: BEL, keep the overlay open (never a silent
            // close), matching the selector's stale-cursor BEL.
            None => {
                let _ = raw_out(b"\x07");
                return Ok(());
            }
        },
        None => return Ok(()),
    };
    // A refusal keeps the overlay open (Locked 6), identical to the selector.
    if let ChromeHit::Notice(msg) = &target.hit {
        view.set_notice(msg.clone());
        return Ok(());
    }
    view.nav = None;
    view.nav_esc.clear();
    // Ordered goto prefix (Locked 4: existing wire commands only). An agent/pane
    // row in another squad switches squad first; a pane row then selects its tab
    // (FocusPane alone does not) so the sequence is SelectSquad -> SelectTab ->
    // FocusPane. Squad/tab rows carry their own switch in `hit` (both prefixes
    // None), so no double send; a pane already in the active view collapses to a
    // bare FocusPane.
    let switching_squad = target
        .goto_squad
        .is_some_and(|sq| sq != view.layout.active_squad);
    if let Some(sq) = target.goto_squad.filter(|_| switching_squad) {
        write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(sq)))
            .await
            .map_err(|e| format!("nav select-squad send failed: {e}"))?;
    }
    if let Some(tid) = target.goto_tab {
        // Skip SelectTab only when the target is already the active view's tab
        // (same squad, same tab); a squad switch always needs it.
        let active_tab_id = view
            .layout
            .squads
            .iter()
            .find(|s| s.id == view.layout.active_squad)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.id);
        if switching_squad || active_tab_id != Some(tid) {
            write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                .await
                .map_err(|e| format!("nav select-tab send failed: {e}"))?;
        }
    }
    apply_hit(view, target.hit, sock_w).await
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

/// Recruit workspace-name keys (x-8f11): the create overlay's shape - printable
/// append, Backspace pops, Esc cancels locally (marks kept), Enter sends
/// [`Command::RecruitAgents`] with the marked ids and CLEARS the marks. An empty
/// name keeps the overlay open (the server would refuse it). An empty mark set
/// falls back to nothing sendable, so Enter just closes (the `R` key already
/// fell back to marking the focused row before opening).
async fn recruit_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.recruit_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.recruit_esc = esc;
    for key in keys {
        if view.recruit.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.recruit = None;
                view.recruit_esc.clear();
                break; // marks kept - Esc cancels the prompt only
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some(name) = view.recruit.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            let ids: Vec<String> = view.marks.iter().cloned().collect();
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::RecruitAgents {
                                    squad: name.to_string(),
                                    ids,
                                }),
                            )
                            .await
                            .map_err(|e| format!("recruit send failed: {e}"))?;
                            view.recruit = None;
                            view.recruit_esc.clear();
                            view.marks.clear(); // submit clears the marks (AC2-HP)
                            break;
                        }
                    }
                    // Empty name: keep the overlay open (server would refuse).
                }
                0x7f | 0x08 => {
                    if let Some(buf) = view.recruit.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some(buf) = view.recruit.as_mut() {
                        if buf.len() < MAX_SQUAD_NAME {
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

/// Rename-tab name-input keys (x-c150). The create overlay's shape (split-arrow
/// folding, printable append, Backspace pops, Esc cancels locally) with one
/// deliberate divergence: Enter ALWAYS sends [`Command::RenameTab`] - an empty
/// buffer is the "reset to auto" verb (blank clears server-side), not a kept-open
/// input. The buffer caps at [`MAX_TAB_NAME`] so the operator sees exactly what
/// the server will store (the server-side cap stays authoritative for the wire).
async fn rename_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.rename_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.rename_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest
        // of the chunk must be swallowed, never forwarded.
        if view.rename.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                // AC1-UI: no command sent, chrome restored, no state retained.
                view.rename = None;
                view.rename_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some((target, name)) = view.rename.take() {
                        view.rename_esc.clear();
                        let cmd = match target {
                            RenameTarget::Tab(tab) => Command::RenameTab { tab, name },
                            RenameTarget::Squad(squad) => Command::RenameSquad { squad, name },
                        };
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("rename send failed: {e}"))?;
                    }
                    break;
                }
                0x7f | 0x08 => {
                    if let Some((_, buf)) = view.rename.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some((target, buf)) = view.rename.as_mut() {
                        // Cap to the target's stored ceiling so the operator sees
                        // exactly what the server will keep (server stays
                        // authoritative for the wire).
                        let cap = match target {
                            RenameTarget::Tab(_) => MAX_TAB_NAME,
                            RenameTarget::Squad(_) => MAX_SQUAD_NAME,
                        };
                        if buf.len() < cap {
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

/// Needs-me overlay keys (x-feec, grown from x-c929): a digit answers the
/// selected answerable row (unchanged [`ClientMsg::PaneAnswer`] - daemon-pinned
/// keystroke, fingerprint fail-closed, focus unchanged), `n`/`N` (and j/k/
/// arrows) cycle the queue, Enter routes per kind (goto its pane/attach, else a
/// focus-manually notice for a squadless live row), q/Esc closes. The queue is
/// read once per chunk from the same [`View::needs_view`] the overlay draws, so
/// the cursor and the rendered rows never diverge. An empty overlay (the
/// "nothing needs you" state) closes on ANY key (AC4-EDGE). Closing bumps the
/// generation token so an in-flight fold result is discarded (AC6-FR).
async fn answer_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.ans_esc);
    let keys = fold_selector_keys(&mut esc, bytes); // arrows -> hjkl twins
    view.ans_esc = esc;
    let (queue, _) = view.needs_view();
    // Active squad/tab, captured once (the layout is stable within a key chunk):
    // an Enter goto sends SelectSquad/SelectTab only when they would change the
    // view, mirroring the x-653d nav goto so a same-context row emits just
    // FocusPane (no redundant selects).
    let active_squad = view.layout.active_squad;
    let active_tab = view
        .layout
        .squads
        .iter()
        .find(|s| s.id == active_squad)
        .and_then(|s| s.tabs.get(s.active_tab))
        .map(|t| t.id);
    for &k in &keys {
        // The empty "nothing needs you" state: any key dismisses it (AC4-EDGE).
        if queue.is_empty() {
            view.answers = None;
            view.needs_gen = view.needs_gen.wrapping_add(1);
            break;
        }
        let Some(cur0) = view.answers else {
            break; // closed mid-chunk
        };
        let cur = cur0.min(queue.len() - 1);
        view.answers = Some(cur);
        match k {
            // Cycle: n/N are the documented keys; j/k and folded arrows too.
            b'n' | b'j' => view.answers = Some((cur + 1) % queue.len()),
            b'N' | b'k' => view.answers = Some((cur + queue.len() - 1) % queue.len()),
            b'0'..=b'9' => {
                let sel = &queue[cur];
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
                    // A digit with no matching option (or a non-answerable row,
                    // e.g. review-wedged / budget / focus-only) is a local BEL,
                    // never a stray key sent to any pane (x-c929 invariant).
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'\r' | b'\n' => {
                // Goto the row's target (x-653d): SelectSquad/SelectTab only when
                // they change the view, then FocusPane; a paneless watch-only row
                // attaches; a squadless live fold row has no reachable pane here,
                // so it degrades to a notice (Invariant: every item actionable).
                let row = &queue[cur];
                if let Some(pane) = row.pane_id {
                    let switching = row.squad.is_some_and(|s| s != active_squad);
                    if let Some(sq) = row.squad.filter(|_| switching) {
                        write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(sq)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    if let Some(tid) = row.tab.filter(|&t| switching || active_tab != Some(t)) {
                        write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pane)))
                        .await
                        .map_err(|e| format!("command send failed: {e}"))?;
                } else if let Some(id) = &row.attach_id {
                    write_msg(sock_w, &ClientMsg::Command(Command::attach_agent(id)))
                        .await
                        .map_err(|e| format!("command send failed: {e}"))?;
                } else {
                    view.set_notice("no pane here - focus it manually".into());
                }
                view.answers = None;
                view.needs_gen = view.needs_gen.wrapping_add(1);
            }
            0x1b | b'q' => {
                view.answers = None;
                view.needs_gen = view.needs_gen.wrapping_add(1);
            }
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
    use crate::proto::{AnswerOption, AnswerablePrompt, PaneMeta, TabMeta};
    use crate::vt::frame_text;

    #[test]
    fn config_says_off_matches_only_trimmed_off() {
        // Bridges config.toml -> the env the interactive server latches
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

    #[test]
    fn pane_state_derives_worst_first_from_badge_and_seen() {
        // The x-653d state vocabulary: badge + seen -> PaneState. x-4328 flips
        // the seen bit later; today every Done is called with seen=false.
        assert_eq!(
            pane_state(Some(AgentBadge::Blocked), false),
            PaneState::Blocked
        );
        assert_eq!(
            pane_state(Some(AgentBadge::Working), false),
            PaneState::Working
        );
        assert_eq!(
            pane_state(Some(AgentBadge::Done), false),
            PaneState::DoneUnseen
        );
        assert_eq!(pane_state(Some(AgentBadge::Done), true), PaneState::Idle);
        assert_eq!(pane_state(None, false), PaneState::Idle);
        // Worst-first ordering (Invariant): the squad rollup takes the `min`, so
        // the worst state must be the Ord-minimum - x-d140's `min` and the
        // navigator filter must agree on this ordering.
        assert!(PaneState::Blocked < PaneState::Working);
        assert!(PaneState::Working < PaneState::DoneUnseen);
        assert!(PaneState::DoneUnseen < PaneState::Idle);
        let rollup = [PaneState::Idle, PaneState::Blocked, PaneState::Working]
            .into_iter()
            .min();
        assert_eq!(rollup, Some(PaneState::Blocked), "the worst state wins");
    }

    #[test]
    fn agent_hit_resolves_pane_then_attach_then_notice() {
        // The shared seam (x-653d): a keyboard goto and a mouse click resolve an
        // agent to the SAME ChromeHit. pane > attach > notice.
        let hosted = AgentRow {
            squad: Some(1),
            name: "a".into(),
            pane_id: Some(7),
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        // A pane-hosted row focuses regardless of the active squad.
        assert!(
            matches!(agent_hit(&hosted, 2), ChromeHit::Cmds(c) if c == vec![Command::FocusPane(7)])
        );
        // A watch-only row whose squad is NOT the active one keeps the new-tab
        // default (cross-workspace / owner-routed).
        let bg = AgentRow {
            pane_id: None,
            attach_id: Some("job1".into()),
            ..hosted.clone()
        };
        assert!(
            matches!(agent_hit(&bg, 2), ChromeHit::Cmds(c) if c == vec![Command::attach_agent("job1")])
        );
        let orphan = AgentRow {
            pane_id: None,
            attach_id: None,
            ..hosted
        };
        assert!(matches!(agent_hit(&orphan, 2), ChromeHit::Notice(_)));
    }

    #[test]
    fn agent_hit_same_workspace_watch_only_splits_current_tab() {
        // AC2-UI (x-9f75): a watch-only attachable row whose squad IS the active
        // squad attaches as a Right split of the current tab (siblings sit side
        // by side), not a new tab.
        let row = AgentRow {
            squad: Some(1),
            name: "sib".into(),
            pane_id: None,
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: Some("job1".into()),
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let ChromeHit::Cmds(c) = agent_hit(&row, 1) else {
            panic!("expected Cmds for a same-workspace watch-only row");
        };
        match c.as_slice() {
            [Command::AttachAgent { id, placement }] => {
                assert_eq!(id, "job1");
                assert_eq!(placement.split, Some(Dir::Right));
                assert!(!placement.here);
                assert!(matches!(placement.target, PaneTarget::CurrentRoute));
            }
            other => panic!("expected one AttachAgent, got {other:?}"),
        }
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
                    panes: Vec::new(),
                })
                .collect(),
            active_tab,
            // One pane per tab is the test fixture's shape (each tab is a leaf).
            panes: tabs,
        }
    }

    #[test]
    fn tab_bar_spans_label_named_tabs_and_collapse_bare_digits() {
        // AC1-HP (render half) + AC1-EDGE: a no-signal name (== its ordinal)
        // renders byte-for-byte today's `[N]` / ` N ` span; a real name
        // renders `{ordinal}:{label}` (ordinal visible - Locked 5) truncated
        // to TAB_LABEL_W.
        let mut view = two_pane_view();
        let spans = view.tab_bar_spans();
        assert_eq!(spans[1].text, " 1 ", "digit collapse: zero regression");
        assert_eq!(spans[2].text, "[2]");
        view.layout.squads[0].tabs[0].name = "x-abcd".into();
        view.layout.squads[0].tabs[1].name = "a-very-long-worktree-name".into();
        let spans = view.tab_bar_spans();
        assert_eq!(spans[1].text, " 1:x-abcd ");
        assert_eq!(spans[2].text, "[2:a-very-long-wo]", "name truncates to 14");
    }

    #[test]
    fn tab_label_text_collapses_only_the_exact_ordinal() {
        assert_eq!(tab_label_text("1", 0), "1");
        assert_eq!(tab_label_text("2", 0), "1:2", "a RENAME to a digit shows");
        assert_eq!(tab_label_text("debug", 2), "3:debug");
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
        // Sideline (x-0090 agents-first): tab rows left the sideline, so an
        // expanded squad with no agents shows only its name row; the next squad
        // follows directly. Active squad carries the `*` glyph (x-2f99).
        assert!(lines[1].contains("▾*footnote"), "{:?}", lines[1]);
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
    fn overlay_viewport_matches_content_origin_and_dims() {
        // x-e9c3: overlay_viewport() is the single source of centering
        // geometry every popover shares - it must track content_dims()/
        // panel_w() exactly, not a separately hand-computed value.
        let view = two_pane_view();
        let (origin, dims) = view.overlay_viewport();
        let (content_rows, content_cols) = view.content_dims();
        assert_eq!(origin, (TAB_BAR_ROWS as usize, view.panel_w() as usize));
        assert_eq!(dims, (content_rows as usize, content_cols as usize));
    }

    #[test]
    fn draw_lines_overlay_centers_within_viewport() {
        // x-e9c3: popovers used to anchor at the outer terminal's top-left
        // corner (origin_r = TAB_BAR_ROWS + 1, col 2), overlapping the
        // sideline. They now center within the content viewport passed in.
        let (rows, cols) = (20usize, 40usize);
        let mut cells = vec![Cell::default(); rows * cols];
        let content_origin = (2usize, 4usize);
        let content_dims = (10usize, 30usize); // roomy viewport, right of a sideline
        let lines = ["ab", "cd"]; // box_h=2, box_w=2
        draw_lines_overlay(&mut cells, rows, cols, content_origin, content_dims, &lines);

        // origin_r = 2 + (10-2)/2 = 6; origin_c = 4 + (30-2)/2 = 18
        let (r, c) = (6, 18);
        assert_eq!(cells[r * cols + c].c, 'a');
        assert_eq!(
            cells[r * cols + c].flags & cell_flags::INVERSE,
            cell_flags::INVERSE
        );
        assert_eq!(cells[(r + 1) * cols + c].c, 'c');
        // Nothing painted at the old hardcoded top-left corner.
        assert_eq!(cells[(TAB_BAR_ROWS as usize + 1) * cols + 2].c, ' ');
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
        // Pins press-cell == anchor-cell for a pane with a NONZERO x origin: the
        // first visible column of an offset pane maps to pane-col 0, so a drag
        // anchored there selects from that glyph, not N chars late.
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
        // With one squad (auto-expanded: 2 tab rows), display_rows is
        // [squad, tab, tab, + new workspace] (len 4), so a hover on index 4
        // is now stale and must be cleared by the push.
        view.hover_row = Some(4);
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
                pane_id: None,
                attach_id: None,
                where_hint: None,
            }],
        });
        // display_rows (x-0090, no tab rows): [footnote squad, + new workspace,
        // Header, Card] -> the card is index 3, at outer row TAB_BAR_ROWS + 3 = 4.
        match view.chrome_hit(4, 5) {
            Some(ChromeHit::Confirm(a)) => {
                assert!(
                    matches!(&a.action, ConfirmKind::Dispatch { node } if node == "x-a496"),
                    "confirm dispatches the card's node"
                );
                assert_eq!(a.label, "hover-cards");
            }
            other => panic!("expected Confirm, got {}", chrome_hit_label(&other)),
        }
    }

    #[test]
    fn chrome_hit_non_ready_card_is_notice_not_confirm() {
        // A blocked/in-flight card is NOT dispatchable (codex peer review): the
        // click is a local notice, never a Confirm that would start work leader+g
        // would skip. Two cards under the work-queue header.
        let mut view = two_pane_view();
        let card = |id: &str, state| BacklogCard {
            id: id.into(),
            slug: String::new(),
            priority: "p2".into(),
            state,
            pane_id: None,
            attach_id: None,
            where_hint: None,
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
        // display_rows (x-0090, no tab rows): [squad, + new workspace, Header,
        // blocked, in-flight] -> the cards paint at outer rows 4, 5.
        assert!(
            matches!(view.chrome_hit(4, 5), Some(ChromeHit::Notice(_))),
            "blocked card -> notice, not confirm"
        );
        assert!(
            matches!(view.chrome_hit(5, 5), Some(ChromeHit::Notice(_))),
            "in-flight card -> notice, not confirm"
        );
    }

    #[test]
    fn chrome_hit_inflight_card_routes_pane_then_attach_then_hint() {
        // x-54fa: an in-flight card is no longer a dead-end. Route priority
        // (plan Locked 5): a pane in this session focuses; a paneless bg
        // worker attaches (same command the agents-row click sends, so the
        // v14 server gates apply); nothing routable says WHERE the work is
        // (the server's where_hint), never a bare "already dispatching".
        let mut view = two_pane_view();
        let card =
            |id: &str, pane: Option<u64>, attach: Option<&str>, hint: Option<&str>| BacklogCard {
                id: id.into(),
                slug: String::new(),
                priority: "p2".into(),
                state: CardState::InFlight,
                pane_id: pane,
                attach_id: attach.map(str::to_owned),
                where_hint: hint.map(str::to_owned),
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
                // Pane beats attach when the server sent both (it never does,
                // but the client must not attach when a local pane exists).
                card("x-aaa", Some(11), Some("deadbee1"), None),
                card("x-bbb", None, Some("deadbee2"), None),
                card("x-ccc", None, None, Some("in flight - worked by t:abc")),
                card("x-ddd", None, None, None),
            ],
        });
        // display_rows (x-0090, no tab rows): [squad, + new workspace, Header,
        // 4 cards] -> rows 4-7.
        assert_eq!(cmds(view.chrome_hit(4, 5)), vec![Command::FocusPane(11)]);
        assert_eq!(
            cmds(view.chrome_hit(5, 5)),
            vec![Command::attach_agent("deadbee2")]
        );
        match view.chrome_hit(6, 5) {
            Some(ChromeHit::Notice(msg)) => assert_eq!(msg, "in flight - worked by t:abc"),
            other => panic!("expected hint notice, got {}", chrome_hit_label(&other)),
        }
        match view.chrome_hit(7, 5) {
            Some(ChromeHit::Notice(msg)) => {
                assert_eq!(msg, "card in flight - no session visible here")
            }
            other => panic!("expected default notice, got {}", chrome_hit_label(&other)),
        }
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
            Some(ChromeHit::ToggleExpand(_)) => "ToggleExpand",
            Some(ChromeHit::OpenSidelineMenu { .. }) => "OpenSidelineMenu",
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

    // A left click on an inactive sideline squad row switches to it; the
    // already-active squad row toggles its caret locally instead of the old
    // silent SelectSquad no-op (x-2f99, AC3-HP/AC4-HP).
    #[test]
    fn chrome_hit_sideline_squad_rows() {
        // No agents/cards (x-0090, no tab rows): rows are [squad 1 (active,
        // expanded), squad 2, footer].
        let view = two_pane_view();
        assert!(matches!(
            view.chrome_hit(1, 4),
            Some(ChromeHit::ToggleExpand(1))
        ));
        assert_eq!(cmds(view.chrome_hit(2, 4)), vec![Command::SelectSquad(2)]);
        // The divider column and the pane content beyond it are not chrome hits.
        assert!(view.chrome_hit(1, 27).is_none());
        assert!(view.chrome_hit(1, 40).is_none());
    }

    #[test]
    fn chrome_hit_adds_sideline_offset_when_scrolled() {
        // Regression (codex P2): a click must invert draw_sideline's scroll
        // offset, so a click on a scrolled row activates the row painted there,
        // not the unscrolled row at the same terminal cell.
        // Rows (x-0090, no tab rows): [0 squad1(active), 1 squad2, 2 footer].
        let mut v = two_pane_view();
        // Unscrolled: terminal row 2 -> display index 1 -> squad2.
        assert_eq!(cmds(v.chrome_hit(2, 4)), vec![Command::SelectSquad(2)]);
        // Scrolled by 1: terminal row 1 -> display index 1 -> squad2 (without the
        // offset it would resolve to index 0, squad1's own name row).
        v.sideline_offset = 1;
        assert_eq!(
            cmds(v.chrome_hit(1, 4)),
            vec![Command::SelectSquad(2)],
            "click resolves through the scroll offset"
        );
    }

    // ---- x-2f99: active-squad visibility ----

    /// two_pane_view's layout with a chosen active squad (LayoutView is not
    /// Clone; squad 1 has 2 tabs, squad 2 has 1).
    fn two_squad_layout(active_squad: u64) -> LayoutView {
        LayoutView {
            squads: vec![meta(1, "footnote", 2, 1), meta(2, "notes", 1, 0)],
            active_squad,
            panes: vec![],
            focus: 0,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
        }
    }

    // AC2-HP: a fresh attach seeds the active squad expanded, so the first
    // frame shows its tabs (and the `*` marker) without any keypress.
    #[test]
    fn view_new_seeds_expanded_with_active_squad() {
        let view = two_pane_view();
        assert!(view.expanded.contains(&1));
        assert!(!view.expanded.contains(&2));
    }

    // AC1-HP + Locked 3: activation auto-expands the newly active squad and
    // never collapses the others.
    #[test]
    fn set_layout_auto_expands_on_activation_change() {
        let mut view = two_pane_view();
        view.set_layout(two_squad_layout(2));
        assert!(view.expanded.contains(&2), "newly active squad expands");
        assert!(
            view.expanded.contains(&1),
            "activation never collapses others"
        );
    }

    // AC1-EDGE: manual collapse of the active squad survives the ~250ms
    // scrape-tick layout pushes - auto-expand fires on CHANGE only.
    #[test]
    fn manual_collapse_survives_same_active_layout_push() {
        let mut view = two_pane_view();
        view.toggle_expand(1);
        assert!(!view.expanded.contains(&1));
        view.set_layout(two_squad_layout(1));
        assert!(
            !view.expanded.contains(&1),
            "a push with an unchanged active_squad must not re-expand"
        );
        // A real activation change still re-expands on re-activation.
        view.set_layout(two_squad_layout(2));
        view.set_layout(two_squad_layout(1));
        assert!(view.expanded.contains(&1));
    }

    // AC3-EDGE: an expanded squad removed server-side leaves `expanded`.
    #[test]
    fn set_layout_prunes_dead_squad_ids_from_expanded() {
        let mut view = two_pane_view();
        let mut layout = two_squad_layout(2);
        layout.squads.remove(0); // squad 1 (expanded) vanishes
        view.set_layout(layout);
        assert!(!view.expanded.contains(&1), "dead id pruned");
        assert!(view.expanded.contains(&2));
    }

    // AC3-HP: acting on the active squad row toggles locally - twice
    // round-trips - and apply_hit's ToggleExpand arm does no I/O (AC1-FR is
    // structural: toggle_expand never touches the socket).
    #[test]
    fn toggle_expand_round_trips() {
        let mut view = two_pane_view();
        assert!(matches!(
            view.row_action(0),
            Some(ChromeHit::ToggleExpand(1))
        ));
        view.toggle_expand(1);
        assert!(!view.expanded.contains(&1), "first toggle collapses");
        // Collapsed, the active row still resolves to the toggle (rows are
        // now [sq1, sq2, footer]).
        assert!(matches!(
            view.row_action(0),
            Some(ChromeHit::ToggleExpand(1))
        ));
        view.toggle_expand(1);
        assert!(view.expanded.contains(&1), "second toggle re-expands");
    }

    // AC1-UI: exactly one squad row carries the `*` glyph - the active one -
    // in both its expanded and collapsed states.
    #[test]
    fn client_compose_active_squad_glyph_in_both_caret_states() {
        let mut view = two_pane_view();
        let text = frame_text(&view.compose());
        assert!(text.contains("▾*footnote"), "expanded active carries *");
        assert!(text.contains("▸ notes"), "inactive carries no *");
        view.toggle_expand(1);
        let text = frame_text(&view.compose());
        assert!(text.contains("▸*footnote"), "collapsed active keeps *");
    }

    // AC2-EDGE: a zero-tab active squad expands to a bare `▾` caret - no tab
    // rows, no panic.
    #[test]
    fn client_compose_zero_tab_active_squad() {
        let view = View::new(
            (30, 100),
            "main".into(),
            LayoutView {
                squads: vec![meta(1, "empty", 0, 0), meta(2, "notes", 1, 0)],
                active_squad: 1,
                panes: vec![],
                focus: 0,
                area: (28, 72),
                agents: vec![],
                focus_node: None,
                backlog: Vec::new(),
            },
        );
        let text = frame_text(&view.compose());
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("▾*empty"), "{:?}", lines[1]);
        assert!(lines[2].contains("▸ notes"), "no tab rows in between");
    }

    // AC2-UI: the status row names the active squad iff more than one squad
    // exists (the sideline-hidden answer to "which squad?").
    #[test]
    fn client_compose_status_row_squad_cell_multi_squad_only() {
        let view = two_pane_view();
        let text = frame_text(&view.compose());
        let bottom = text.lines().last().unwrap();
        assert!(
            bottom.starts_with(" main │ footnote │ /code/footnote"),
            "{bottom:?}"
        );
        // A single squad has nothing to disambiguate: the cell is absent.
        let mut view = two_pane_view();
        let mut layout = two_squad_layout(1);
        layout.squads.remove(1);
        view.set_layout(layout);
        let text = frame_text(&view.compose());
        let bottom = text.lines().last().unwrap();
        assert!(bottom.starts_with(" main │ /code/footnote"), "{bottom:?}");
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
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
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
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
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
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let view = view_with_agents(vec![hosted, bg_attach, bg_plain]);
        // Agents-first display order (x-0090; no tab rows). Display indices:
        // squad 1 (0), agent "worker" (1, expanded), squad 2 (2, collapsed),
        // "+ new workspace" footer (3), "~ elsewhere" header (4), orphan
        // "bg-claude" (5), orphan "bg-other" (6). chrome_hit takes the TERMINAL
        // row = display index + 1 (the tab bar).
        assert_eq!(cmds(view.chrome_hit(2, 4)), vec![Command::FocusPane(10)]);
        assert_eq!(
            cmds(view.chrome_hit(6, 4)),
            vec![Command::attach_agent("c19cd2c3")]
        );
        assert!(matches!(view.chrome_hit(7, 4), Some(ChromeHit::Notice(_))));
        // The "~ elsewhere" header row is inert.
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
                external: false,
                seen: false,
                cwd_base: None,
                tombstone: false,
                tab: None,
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
    fn client_compose_keys_modal_renders_the_which_key_reference() {
        // x-8ccf US3: leader+? opens the centered which-key modal (replacing the
        // top-left poster) built from the single-source binding table.
        let mut view = two_pane_view();
        view.term = (40, 80);
        view.open_keys_modal();
        let text = frame_text(&view.compose());
        assert!(text.contains("keybinds"), "modal title present");
        assert!(text.contains("esc close"), "dismiss affordance present");
        // Section headers + a sampling of bindings the table advertises.
        assert!(text.contains("panes"), "section header");
        assert!(text.contains("detach"), "the d binding's action");
        assert!(
            text.contains("find: goto pane/agent"),
            "the f binding's action"
        );
    }

    #[test]
    fn client_keys_modal_execute_selected_maps_selected_row_to_its_chord() {
        // The default selection is the first binding; row_events[selected] must
        // be exactly the Event a direct chord of that key would produce (Locked
        // 3 parity, at the modal boundary).
        let m = super::build_keys_modal();
        let (ri, _) = m.popup.selected().expect("a selectable row");
        let ev = m.row_events[ri].clone().expect("first row is executable");
        // The first section is Global; its first binding is `w` -> OpenSelector.
        assert_eq!(ev, crate::keys::resolve_chord(b'w'));
    }

    #[tokio::test]
    async fn keys_modal_which_key_executes_a_bound_key_to_the_wire() {
        // AC2-HP: tapping a bound key in the modal runs it immediately through
        // the SAME dispatch a direct chord uses, and the modal closes.
        let mut v = two_pane_view();
        v.term = (40, 80);
        v.open_keys_modal();
        let mut buf: Vec<u8> = Vec::new();
        keys_modal_keys(&mut v, b"%", &mut buf).await.unwrap();
        assert!(v.keys_modal.is_none(), "executing a chord closes the modal");
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::SplitH) => {}
            other => panic!("expected SplitH from `%`, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keys_modal_unbound_key_and_esc_dismiss_without_acting() {
        // AC2-EDGE: an unbound key dismisses and NO action fires. Esc is FOLDED
        // (carried across reads like every overlay, codex P2) so it resolves once
        // a following byte disambiguates it from a split arrow.
        let mut v = two_pane_view();
        v.term = (40, 80);
        let mut buf: Vec<u8> = Vec::new();
        v.open_keys_modal();
        keys_modal_keys(&mut v, b"Z", &mut buf).await.unwrap();
        assert!(v.keys_modal.is_none(), "unbound key dismisses");
        assert!(buf.is_empty(), "unbound key sends nothing");
        // A lone Esc is carried (no leak); the next byte flushes it as a dismiss.
        v.open_keys_modal();
        keys_modal_keys(&mut v, b"\x1b", &mut buf).await.unwrap();
        assert!(
            v.keys_modal.is_some(),
            "a lone Esc is carried, not acted on"
        );
        keys_modal_keys(&mut v, b"z", &mut buf).await.unwrap();
        assert!(
            v.keys_modal.is_none(),
            "the carried Esc dismisses on the next key"
        );
        assert!(buf.is_empty(), "Esc sends nothing to a pane");
    }

    #[tokio::test]
    async fn keys_modal_wheel_scrolls_and_click_off_dismisses() {
        use crate::mouse::MouseReport;
        let mut v = two_pane_view();
        v.term = (8, 80); // short: the binding list overflows and scrolls
        v.open_keys_modal();
        let mut buf: Vec<u8> = Vec::new();
        let wheel = MouseReport {
            row: 4,
            col: 40,
            kind: MouseKind::WheelDown,
            shift: false,
        };
        keys_modal_mouse(&mut v, wheel, &mut buf).await.unwrap();
        assert_eq!(
            v.keys_modal.as_ref().unwrap().popup.scroll,
            3,
            "wheel scrolls"
        );
        // A left click off the popup (top-left corner) dismisses.
        let click = MouseReport {
            row: 0,
            col: 0,
            kind: MouseKind::Press(MouseButton::Left),
            shift: false,
        };
        keys_modal_mouse(&mut v, click, &mut buf).await.unwrap();
        assert!(v.keys_modal.is_none(), "click off the popup dismisses");
    }

    #[test]
    fn row_menu_entries_gate_by_agent_state() {
        // US2: no dead item - a bg row gets new-tab + the 2x2 split grid; a pane
        // row gets focus and NO splits (already placed); an exited row gets
        // remove and no stop.
        let mk = |name: &str, pane_id: Option<u64>, attach: Option<&str>, exited: bool| AgentRow {
            squad: None,
            name: name.into(),
            pane_id,
            badge: None,
            reason: None,
            exited,
            answerable: None,
            attach_id: attach.map(Into::into),
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let bg = super::build_row_menu(&mk("bg", None, Some("id"), false), Anchor::Center);
        assert!(bg.actions.contains(&super::MenuAction::NewTab));
        assert!(bg.actions.contains(&super::MenuAction::Split(Dir::Right)));
        assert!(bg.actions.contains(&super::MenuAction::Split(Dir::Up)));
        assert!(bg.actions.contains(&super::MenuAction::Stop));
        assert!(!bg.actions.contains(&super::MenuAction::Focus));
        // AC1-UI (x-9f75): Open Here is present and leads above New Tab.
        let open_here = bg
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::OpenHere);
        let new_tab = bg
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::NewTab);
        assert!(
            matches!((open_here, new_tab), (Some(o), Some(n)) if o < n),
            "Open Here sits above New Tab"
        );
        let pane = super::build_row_menu(&mk("p", Some(9), None, false), Anchor::Center);
        assert!(pane.actions.contains(&super::MenuAction::Focus));
        assert!(
            !pane.actions.contains(&super::MenuAction::OpenHere),
            "a placed pane row offers no open-here"
        );
        assert!(
            !pane
                .actions
                .iter()
                .any(|a| matches!(a, super::MenuAction::Split(_))),
            "a placed pane row offers no splits"
        );
        let dead = super::build_row_menu(&mk("d", None, None, true), Anchor::Center);
        assert!(dead.actions.contains(&super::MenuAction::Remove));
        assert!(!dead.actions.contains(&super::MenuAction::Stop));
    }

    #[tokio::test]
    async fn row_menu_bg_split_right_attaches_to_current_route() {
        // AC1-HP: "Split Right" on a bg row sends AttachAgent placing it as a
        // right split of the current tab - an existing command, zero proto bump.
        let mut v = unified_rows_view();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        assert!(v.open_row_menu(idx, Anchor::Center));
        let sel = v
            .row_menu
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::Split(Dir::Right))
            .unwrap();
        v.row_menu.as_mut().unwrap().popup.sel = sel;
        let mut buf: Vec<u8> = Vec::new();
        row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
        assert!(v.row_menu.is_none(), "executing closes the menu");
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::AttachAgent { id, placement }) => {
                assert_eq!(id, "c19cd2c3");
                assert_eq!(placement.split, Some(Dir::Right));
                assert!(matches!(
                    placement.target,
                    crate::proto::PaneTarget::CurrentRoute
                ));
            }
            other => panic!("expected AttachAgent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn row_menu_open_here_sends_here_placement() {
        // AC1-UI (x-9f75): "Open Here" on a bg row sends AttachAgent with
        // here:true and the default (CurrentRoute, no split) placement; the
        // menu closes.
        let mut v = unified_rows_view();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        assert!(v.open_row_menu(idx, Anchor::Center));
        let sel = v
            .row_menu
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::OpenHere)
            .unwrap();
        v.row_menu.as_mut().unwrap().popup.sel = sel;
        let mut buf: Vec<u8> = Vec::new();
        row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
        assert!(v.row_menu.is_none(), "executing closes the menu");
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::AttachAgent { id, placement }) => {
                assert_eq!(id, "c19cd2c3");
                assert!(placement.here, "open-here sets here:true");
                assert!(placement.split.is_none());
                assert!(matches!(
                    placement.target,
                    crate::proto::PaneTarget::CurrentRoute
                ));
            }
            other => panic!("expected AttachAgent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn row_menu_stale_target_notices_without_acting() {
        // AC1-ERR: the target racing out between open and execute becomes a
        // Notice, and nothing goes on the wire.
        let mut v = unified_rows_view();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        v.open_row_menu(idx, Anchor::Center);
        let sel = v
            .row_menu
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::Split(Dir::Right))
            .unwrap();
        v.row_menu.as_mut().unwrap().popup.sel = sel;
        v.layout.agents.retain(|a| a.name != "bg-claude"); // it vanishes
        let mut buf: Vec<u8> = Vec::new();
        row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
        assert!(buf.is_empty(), "a stale target sends nothing");
        assert!(v.notice.is_some(), "and surfaces a notice");
    }

    #[test]
    fn row_menu_opens_only_on_agent_rows() {
        // Row 0 is a squad row, not an agent - the menu refuses (right-click
        // there is swallowed as chrome).
        let mut v = unified_rows_view();
        assert!(!v.open_row_menu(0, Anchor::Center));
        assert!(v.row_menu.is_none());
    }

    #[tokio::test]
    async fn row_menu_unbound_key_dismisses() {
        // codex P2: the shared popup contract says an unbound key dismisses; the
        // row menu must not just ring BEL and stay open.
        let mut v = unified_rows_view();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        v.open_row_menu(idx, Anchor::Center);
        let mut buf: Vec<u8> = Vec::new();
        row_menu_keys(&mut v, b"z", &mut buf).await.unwrap();
        assert!(v.row_menu.is_none(), "an unbound key dismisses the menu");
    }

    #[tokio::test]
    async fn row_menu_disambiguates_same_named_agents() {
        // codex P1: two rows share a name; the menu is pinned by the full
        // identity (pane_id/attach_id) so Focus acts on the row it was opened on,
        // never the other same-named row.
        let mk = |name: &str, pane_id: Option<u64>| AgentRow {
            squad: Some(1),
            name: name.into(),
            pane_id,
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let mut v = view_with_agents(vec![mk("dup", Some(5)), mk("dup", Some(9))]);
        // Open the menu on the SECOND "dup" (pane 9) and pick Focus.
        let second = mk("dup", Some(9));
        v.row_menu = Some(build_row_menu(&second, Anchor::Center));
        let sel = v
            .row_menu
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::Focus)
            .unwrap();
        v.row_menu.as_mut().unwrap().popup.sel = sel;
        let mut buf: Vec<u8> = Vec::new();
        row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::FocusPane(pid)) => {
                assert_eq!(
                    pid, 9,
                    "focused the row the menu was opened on, not its twin"
                );
            }
            other => panic!("expected FocusPane(9), got {other:?}"),
        }
    }

    #[test]
    fn footer_menu_region_routes_a_click_to_the_sideline_menu() {
        // US4: a click on the footer's `☰ menu` region opens the MENU popup; the
        // rest of the `+ new workspace` row still opens create.
        let mut v = two_pane_view();
        v.term = (30, 100);
        let footer = v
            .display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::NewSquad))
            .unwrap();
        let panel_w = v.panel_w() as usize;
        let range = v
            .footer_menu_range(panel_w)
            .expect("a wide panel shows the menu button");
        let trow = (TAB_BAR_ROWS as usize + footer - v.sideline_offset) as u16;
        assert!(matches!(
            v.chrome_hit(trow, range.start as u16),
            Some(ChromeHit::OpenSidelineMenu { .. })
        ));
        assert!(matches!(v.chrome_hit(trow, 2), Some(ChromeHit::OpenCreate)));
    }

    #[tokio::test]
    async fn sideline_menu_settings_toggle_flips_session_state_and_stays_open() {
        // US4->US5: MENU -> settings chains, and a toggle flips session state and
        // keeps the modal open (labeled session-only, no config write).
        let mut v = two_pane_view();
        v.term = (30, 100);
        v.open_sideline_menu(Anchor::Center);
        let settings = v
            .aux
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == AuxAction::OpenSettings)
            .unwrap();
        v.aux.as_mut().unwrap().popup.sel = settings;
        let mut buf: Vec<u8> = Vec::new();
        aux_execute_selected(&mut v, &mut buf).await.unwrap();
        assert!(v
            .aux
            .as_ref()
            .unwrap()
            .actions
            .contains(&AuxAction::ToggleHoverFocus));
        let before = v.hover_focus;
        let hf = v
            .aux
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == AuxAction::ToggleHoverFocus)
            .unwrap();
        v.aux.as_mut().unwrap().popup.sel = hf;
        aux_execute_selected(&mut v, &mut buf).await.unwrap();
        assert_eq!(v.hover_focus, !before, "toggle flips session state");
        assert!(v.aux.is_some(), "settings stays open for another toggle");
    }

    #[tokio::test]
    async fn peek_from_right_click_esc_returns_to_pane_not_selector() {
        // US2 review fix: peek opened standalone (right-click a row, selector
        // closed) must close back to the pane on Esc, not drop into the panel
        // selector (which assumed peek was opened from it).
        let mut v = unified_rows_view();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        let mut buf: Vec<u8> = Vec::new();
        assert!(v.selector.is_none());
        fetch_peek(&mut v, idx, "bg-claude".to_string(), &mut buf)
            .await
            .unwrap();
        assert!(v.peek.is_some());
        peek_keys(&mut v, b"q", &mut buf).await.unwrap();
        assert!(v.peek.is_none(), "peek closed");
        assert!(
            v.selector.is_none(),
            "did NOT drop into panel-selector mode"
        );
    }

    #[tokio::test]
    async fn settings_toggle_preserves_keyboard_selection() {
        // US5 review fix: toggling rebuilds the modal but keeps the selection, so
        // a keyboard Enter re-toggles the SAME row instead of alternating.
        let mut v = two_pane_view();
        v.aux = Some(v.build_settings_modal());
        assert!(v.aux.as_ref().unwrap().popup.targets().len() >= 2);
        v.aux.as_mut().unwrap().popup.sel = 1; // the second toggle
        let mut buf: Vec<u8> = Vec::new();
        aux_execute_selected(&mut v, &mut buf).await.unwrap();
        assert_eq!(
            v.aux.as_ref().unwrap().popup.sel,
            1,
            "selection stays on the toggled row after the rebuild"
        );
    }

    #[tokio::test]
    async fn sideline_menu_detach_entry_detaches() {
        let mut v = two_pane_view();
        v.open_sideline_menu(Anchor::Center);
        let idx = v
            .aux
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|a| *a == AuxAction::Detach)
            .unwrap();
        v.aux.as_mut().unwrap().popup.sel = idx;
        let mut buf: Vec<u8> = Vec::new();
        assert!(matches!(
            aux_execute_selected(&mut v, &mut buf).await.unwrap(),
            DispatchFlow::Detach
        ));
        assert!(v.aux.is_none());
    }

    #[test]
    fn client_compose_notice_overlays_a_full_tab_bar() {
        let mut view = two_pane_view();
        view.term = (30, 80);
        view.layout.squads[0] = meta(1, "long-workspace", 6, 5);
        for tab in &mut view.layout.squads[0].tabs {
            tab.name = "very-long-tab-name".into();
        }
        view.set_notice("no such tab".into());

        let text = frame_text(&view.compose());
        assert!(
            text.lines().next().unwrap().contains("no such tab"),
            "the stale-refusal notice remains visible over a dense tab bar"
        );
        let notice_start = 80 - "no such tab".chars().count() - 1;
        assert!(
            view.chrome_hit(0, notice_start as u16).is_none(),
            "clicks on the visible notice do not activate hidden tabs"
        );
    }

    #[test]
    fn client_compose_hint_lists_the_find_chord() {
        // x-653d AC5-UI: the which-key hint lists `f find` (past the width
        // budget on a narrow terminal, so composed wide here to see it).
        let mut view = two_pane_view();
        view.term = (30, 240);
        view.hint = true;
        let text = frame_text(&view.compose());
        assert!(
            text.lines().last().unwrap().contains("f find"),
            "hint lists the navigator chord"
        );
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
                    external: false,
                    seen: false,
                    cwd_base: None,
                    tombstone: false,
                    tab: None,
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
                    external: false,
                    seen: false,
                    cwd_base: None,
                    tombstone: false,
                    tab: None,
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
                    external: false,
                    seen: false,
                    cwd_base: None,
                    tombstone: false,
                    tab: None,
                },
            ],
            focus_node: None,
            backlog: Vec::new(),
        });
        let frame = view.compose();
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        // Agents-first row order (x-0090; no tab rows): footnote (auto-expanded,
        // x-2f99), its two agent rows, notes squad, the "+ new workspace"
        // footer, the "~ elsewhere" header, the orphan row.
        assert!(lines[1].contains("\u{25be}*footnote"), "{:?}", lines[1]);
        assert!(
            lines[2].contains("\u{25b2} peer: perm prompt"),
            "{:?}",
            lines[2]
        );
        assert!(lines[3].contains("\u{2717} dead"), "{:?}", lines[3]);
        assert!(lines[4].contains("\u{25b8} notes"), "{:?}", lines[4]);
        assert!(lines[5].contains("+ new workspace"), "{:?}", lines[5]);
        assert!(lines[6].contains("~ elsewhere"), "{:?}", lines[6]);
        assert!(lines[7].contains("\u{25cf} bg-watch"), "{:?}", lines[7]);
        // The exited row is DIM (fact beats badge, visually too). "dead" is
        // display index 2 -> frame row 3 (tab bar + index).
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
    fn squad_rollup_glyph_on_collapsed_row() {
        // x-d140: a COLLAPSED squad's name row carries a trailing rollup glyph
        // for its worst agent state, so a blocked pane is visible without
        // expanding; an EXPANDED squad shows no rollup (its agent rows already
        // carry the glyphs). `seen: false` here (done == unseen); the real bit
        // is x-4328's, exercised elsewhere.
        fn ar(squad: u64, name: &str, badge: Option<AgentBadge>, exited: bool) -> AgentRow {
            AgentRow {
                squad: Some(squad),
                name: name.into(),
                pane_id: None,
                badge,
                reason: None,
                exited,
                answerable: None,
                attach_id: None,
                external: false,
                seen: false,
                cwd_base: None,
                tombstone: false,
                tab: None,
            }
        }
        let mut view = two_pane_view();
        let panes = view.layout.panes.clone();
        let long = "this-is-a-very-long-squad-name-xyz";
        view.set_layout(LayoutView {
            squads: vec![
                meta(1, "footnote", 1, 0),
                meta(2, "notes", 1, 0),
                meta(3, "quiet", 1, 0),
                meta(4, long, 1, 0),
            ],
            active_squad: 1, // only footnote auto-expands; 2/3/4 stay collapsed
            panes,
            focus: 11,
            area: (29, 72),
            agents: vec![
                ar(1, "lb", Some(AgentBadge::Blocked), false), // squad 1 is active -> expanded
                ar(2, "w", Some(AgentBadge::Working), false),
                ar(2, "b", Some(AgentBadge::Blocked), false), // worst-of -> Blocked
                ar(3, "gone", Some(AgentBadge::Blocked), true), // exited -> Idle
                ar(4, "b2", Some(AgentBadge::Blocked), false),
            ],
            focus_node: None,
            backlog: Vec::new(),
        });
        let lines: Vec<String> = frame_text(&view.compose())
            .lines()
            .map(str::to_string)
            .collect();
        let find = |needle: &str| {
            lines
                .iter()
                .find(|l| l.contains(needle))
                .cloned()
                .unwrap_or_else(|| panic!("row {needle:?} not found in {lines:#?}"))
        };

        // AC1-HP + AC2-HP: collapsed `notes` shows ▲ (blocked outranks working)
        // even though it is neither active nor expanded.
        let notes = find("\u{25b8} notes");
        assert!(
            notes.contains('\u{25b2}'),
            "notes rollup \u{25b2}: {notes:?}"
        );

        // AC1-ERR + AC2-EDGE: `quiet`'s only agent is exited -> Idle -> the name
        // row is the pre-feature render, no rollup glyph.
        let quiet = find("\u{25b8} quiet");
        assert!(
            !quiet.contains('\u{25b2}')
                && !quiet.contains('\u{25cf}')
                && !quiet.contains('\u{2713}'),
            "quiet has no rollup glyph: {quiet:?}"
        );

        // AC1-EDGE: a name longer than text_w-2 is truncated but the trailing
        // ▲ survives at the right edge (reserve-then-truncate).
        let long_row = find("this-is-a-very-long");
        assert!(
            long_row.contains('\u{25b2}'),
            "long name keeps \u{25b2}: {long_row:?}"
        );
        assert!(
            !long_row.contains(long),
            "long name is truncated: {long_row:?}"
        );

        // Collapsed-only: `footnote` is the active (auto-expanded) squad and
        // holds a live blocked agent, but its EXPANDED name row shows no rollup
        // glyph - the blocked `lb` agent's own row carries the ▲ instead.
        let footnote = find("\u{25be}*footnote"); // ▾*footnote (expanded caret)
        assert!(
            !footnote.contains('\u{25b2}')
                && !footnote.contains('\u{25cf}')
                && !footnote.contains('\u{2713}'),
            "expanded squad has no rollup glyph: {footnote:?}"
        );
    }

    #[test]
    fn external_live_row_is_dim_and_distinct_from_exited_and_fno_live() {
        // x-0a2e AC1-UI: the three sideline row kinds are pairwise distinct -
        // `✗`+DIM (exited), `·`+DIM (external, roster-surfaced live), `·` bright
        // (fno-owned live). External dims a live `·` row without stealing the
        // exit glyph or the bright-live glyph.
        let mut view = two_pane_view();
        let panes = view.layout.panes.clone();
        view.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 1, 1)],
            active_squad: 1,
            panes,
            focus: 11,
            area: (29, 72),
            agents: vec![
                AgentRow {
                    squad: None,
                    name: "z-exited".into(),
                    pane_id: None,
                    badge: None,
                    reason: None,
                    exited: true,
                    answerable: None,
                    attach_id: None,
                    external: false,
                    seen: false,
                    cwd_base: None,
                    tombstone: false,
                    tab: None,
                },
                AgentRow {
                    squad: None,
                    name: "z-external".into(),
                    pane_id: None,
                    badge: None,
                    reason: None,
                    exited: false,
                    answerable: None,
                    attach_id: Some("ab12cd34".into()),
                    external: true,
                    seen: false,
                    cwd_base: None,
                    tombstone: false,
                    tab: None,
                },
                AgentRow {
                    squad: None,
                    name: "z-fnolive".into(),
                    pane_id: None,
                    badge: None,
                    reason: None,
                    exited: false,
                    answerable: None,
                    attach_id: None,
                    external: false,
                    seen: false,
                    cwd_base: None,
                    tombstone: false,
                    tab: None,
                },
            ],
            focus_node: None,
            backlog: Vec::new(),
        });
        let frame = view.compose();
        let cols = frame.cols as usize;
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        // Locate each row by name and read its glyph cell (col 2) + DIM flag.
        let probe = |needle: &str| -> (char, bool) {
            let r = lines.iter().position(|l| l.contains(needle)).unwrap();
            let cell = frame.cells[r * cols + 2];
            (cell.c, cell.flags & cell_flags::DIM == cell_flags::DIM)
        };
        assert_eq!(probe("z-exited"), ('\u{2717}', true), "exited: ✗ + DIM");
        assert_eq!(probe("z-external"), ('\u{00b7}', true), "external: · + DIM");
        assert_eq!(
            probe("z-fnolive"),
            ('\u{00b7}', false),
            "fno-live: · + bright"
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
    fn client_compose_agents_first_omits_tab_rows_and_highlights_squad() {
        // x-0090 (Locked 4): tab rows left the sideline. The active squad arrives
        // expanded (View::new seeds it, x-2f99) but two_pane_view has no agents,
        // so its expanded body is empty and the SelRows are just the squad names.
        let mut view = two_pane_view();
        view.selector = Some(1); // squad 2's name row (display index)
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
                    squad: 2,
                    tab: None
                },
            ]
        );
        let frame = view.compose();
        let text = frame_text(&frame);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("▾*footnote"), "{:?}", lines[1]);
        assert!(
            lines[2].contains("▸ notes"),
            "next squad follows directly, no tab rows: {:?}",
            lines[2]
        );
        assert!(
            !lines.iter().any(|l| l.contains("*2")),
            "no active-tab row renders in the sideline"
        );
        // The selector row (squad 2, display index 1 -> frame row 2) carries
        // INVERSE.
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
        // Display rows are now [notes squad (auto-expanded, no agents),
        // + new workspace] (x-0090: no tab rows): the cursor clamps to the last
        // live row (the footer, an actionable stop).
        assert_eq!(view.selector, Some(1), "cursor clamped to the live rows");
    }

    // ---- x-260a: unified selector rows (keyboard reaches every actionable row) ----

    /// A sideline with every row kind: squad 1 + its hosted agent, squad 2,
    /// the footer, an orphan-agents section (attachable bg + watch-only), and
    /// a work-queue lane (ready + blocked cards).
    ///
    /// Display rows (x-0090 agents-first; sq1 active/expanded, no tab rows):
    /// 0 sq1 · 1 hosted agent · 2 sq2 · 3 "+ new workspace" ·
    /// 4 "~ elsewhere" · 5 bg-attach · 6 bg-plain · 7 "~ work queue" ·
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
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let card = |id: &str, state| BacklogCard {
            id: id.into(),
            slug: String::new(),
            priority: "p2".into(),
            state,
            pane_id: None,
            attach_id: None,
            where_hint: None,
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
        assert_eq!(
            v.selector_down(3),
            5,
            "j from the footer skips '~ elsewhere'"
        );
        assert_eq!(v.selector_down(6), 8, "j skips '~ work queue'");
        assert_eq!(v.selector_down(10), 10, "clamp at the last row");
        assert_eq!(v.selector_up(5), 3, "k skips '~ elsewhere' upward");
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

    fn agent_row_at(v: &View, pred: impl Fn(&AgentRow) -> bool) -> usize {
        v.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent(a) if pred(a)))
            .expect("a matching agent row")
    }

    #[tokio::test]
    async fn selector_tab_marks_attachable_and_notices_unmarkable() {
        // AC1-UI: Tab marks an attachable watch-only row (toggling), and gives
        // a notice on a pane-hosted (unmarkable) row without marking. (x-c376
        // moved the mark toggle from Space to Tab; Space now opens peek.)
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let idx = agent_row_at(&v, |a| a.attach_id.as_deref() == Some("c19cd2c3"));
        v.selector = Some(idx);
        selector_keys(&mut v, b"\t", &mut buf).await.unwrap();
        assert!(v.marks.contains("c19cd2c3"), "Tab marks the attachable row");
        v.selector = Some(idx);
        selector_keys(&mut v, b"\t", &mut buf).await.unwrap();
        assert!(!v.marks.contains("c19cd2c3"), "Tab toggles the mark off");
        // A pane-hosted row (no attach_id) is unmarkable -> notice, no mark.
        let hosted = agent_row_at(&v, |a| a.pane_id == Some(10));
        v.selector = Some(hosted);
        selector_keys(&mut v, b"\t", &mut buf).await.unwrap();
        assert!(v.marks.is_empty(), "an unmarkable row is not marked");
        assert!(v.notice.is_some(), "an unmarkable row gives a notice");
    }

    // x-c376 AC1-HP: Space on a selector agent row opens the peek overlay, sends
    // a PeekAgent for that row's name, and leaves the selector open underneath.
    #[tokio::test]
    async fn peek_space_opens_overlay_and_sends_peekagent() {
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        v.selector = Some(idx);
        selector_keys(&mut v, b" ", &mut buf).await.unwrap();
        let peek = v.peek.as_ref().expect("Space opens peek");
        assert_eq!(peek.cursor, idx);
        assert!(peek.body.is_none(), "starts loading");
        assert_eq!(v.selector, Some(idx), "selector stays open underneath");
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::PeekAgent { name, seq }) => {
                assert_eq!(name, "bg-claude");
                assert_eq!(seq, peek.seq);
            }
            other => panic!("expected PeekAgent, got {other:?}"),
        }
    }

    // x-c376: Space on a non-agent row (a section header) BELs, never opens peek.
    #[tokio::test]
    async fn peek_space_on_header_does_not_open() {
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let header = v
            .display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Header(_)))
            .expect("a header row exists");
        v.selector = Some(header);
        selector_keys(&mut v, b" ", &mut buf).await.unwrap();
        assert!(v.peek.is_none(), "Space on a header never opens peek");
    }

    // x-c376 AC2-HP: j moves the peek to the next agent row and refetches with a
    // fresh, higher seq (stale bodies then drop by seq).
    #[tokio::test]
    async fn peek_j_moves_to_adjacent_agent_and_refetches() {
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let first = agent_row_at(&v, |a| a.name == "worker");
        v.selector = Some(first);
        selector_keys(&mut v, b" ", &mut buf).await.unwrap();
        let seq0 = v.peek.as_ref().unwrap().seq;
        buf.clear();
        peek_keys(&mut v, b"j", &mut buf).await.unwrap();
        let peek = v.peek.as_ref().expect("still open after j");
        assert!(peek.cursor > first, "moved down to the next agent row");
        assert!(peek.seq > seq0, "a fresh request seq");
        assert!(peek.body.is_none(), "the new row starts loading again");
        let mut cur = std::io::Cursor::new(buf);
        assert!(
            matches!(
                crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur),
                Ok(ClientMsg::Command(Command::PeekAgent { .. }))
            ),
            "j fires a fresh PeekAgent"
        );
    }

    // x-c376 AC2-UI: Esc closes peek back to the selector at the peeked row.
    #[tokio::test]
    async fn peek_esc_returns_to_selector_at_peeked_row() {
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let idx = agent_row_at(&v, |a| a.name == "bg-claude");
        v.selector = Some(idx);
        selector_keys(&mut v, b" ", &mut buf).await.unwrap();
        // A bare Esc resolves on the following byte (fold_selector_keys); "\x1bq"
        // yields one bare-Esc key (the q is swallowed by the pending-esc branch).
        peek_keys(&mut v, b"\x1bq", &mut buf).await.unwrap();
        assert!(v.peek.is_none(), "Esc closes peek");
        assert_eq!(v.selector, Some(idx), "selector cursor sits on the row");
    }

    // x-c376 AC1-FR: a PeekBody whose seq is not current is dropped; the matching
    // seq applies.
    #[test]
    fn peek_body_seq_guard_drops_stale() {
        let mut v = unified_rows_view();
        v.peek = Some(PeekView {
            cursor: 0,
            seq: 5,
            body: None,
            name: String::new(),
        });
        assert!(
            !v.apply_peek_body(4, vec!["stale".into()]),
            "an older seq is dropped"
        );
        assert!(v.peek.as_ref().unwrap().body.is_none());
        assert!(
            v.apply_peek_body(5, vec!["fresh".into()]),
            "the current seq applies"
        );
        assert_eq!(
            v.peek.as_ref().unwrap().body.as_deref(),
            Some(["fresh".to_string()].as_slice())
        );
    }

    // x-c376: peek_overlay_lines renders loading, then the transcript, and folds
    // in the x-c929 answerable block for a blocked row.
    #[test]
    fn peek_overlay_renders_loading_transcript_and_answerable() {
        let row = AgentRow {
            squad: None,
            name: "w".into(),
            pane_id: Some(3),
            badge: Some(AgentBadge::Blocked),
            reason: Some("waiting on a menu".into()),
            exited: false,
            answerable: Some(answerable(&[("1", "Yes"), ("2", "No")], 7)),
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let loading = PeekView {
            cursor: 0,
            seq: 1,
            body: None,
            name: "w".into(),
        };
        let out = peek_overlay_lines(Some(&row), &loading).join("\n");
        assert!(
            out.contains("waiting on a menu"),
            "shows the status sentence"
        );
        assert!(
            out.contains("1. Yes") && out.contains("2. No"),
            "answerable"
        );
        assert!(out.contains("loading"), "loading placeholder before a body");
        let loaded = PeekView {
            cursor: 0,
            seq: 1,
            body: Some(vec!["line one".into(), "line two".into()]),
            name: "w".into(),
        };
        let out = peek_overlay_lines(Some(&row), &loaded).join("\n");
        assert!(out.contains("line one") && out.contains("line two"));
        assert!(!out.contains("loading"), "no placeholder once loaded");
        // A vanished row renders a safe placeholder, never a panic.
        assert!(peek_overlay_lines(None, &loaded)[0].contains("row gone"));
    }

    // x-c376 (codex review): a layout shift that lands a DIFFERENT agent on the
    // peeked index refetches (header + transcript never disagree); the same agent
    // holds.
    #[test]
    fn peek_reanchor_refetches_on_identity_change_holds_on_same() {
        let mut v = unified_rows_view();
        let idx = agent_row_at(&v, |a| a.name == "worker");
        v.open_peek(idx, "worker".into());
        assert_eq!(v.peek_reanchor(), None, "same agent at the index holds");
        v.open_peek(idx, "was-someone-else".into());
        assert_eq!(
            v.peek_reanchor(),
            Some((idx, "worker".to_string())),
            "a changed row identity refetches"
        );
    }

    // x-c376 (codex review): raw transcript control chars (ESC/CR/TAB) are
    // stripped before rendering so they never reach the operator's terminal.
    #[test]
    fn peek_overlay_sanitizes_control_chars_in_body() {
        let row = agent_row("w", 3, Some(AgentBadge::Working), false);
        let peek = PeekView {
            cursor: 0,
            seq: 1,
            body: Some(vec!["a\x1b[31mred\x1b[0m\tb\rc".into()]),
            name: "w".into(),
        };
        let out = peek_overlay_lines(Some(&row), &peek).join("\n");
        assert!(!out.contains('\x1b'), "ESC stripped");
        assert!(!out.contains('\r'), "CR stripped");
        assert!(!out.contains('\t'), "TAB replaced");
        assert!(
            out.contains("red") && out.contains('c'),
            "printable text kept"
        );
    }

    // x-c376 AC3-HP / AC2-ERR: a digit on a blocked, pane-hosted peeked row sends
    // the exact x-c929 PaneAnswer payload and keeps the overlay open; a digit on a
    // non-answerable row sends nothing (BEL).
    #[tokio::test]
    async fn peek_digit_answers_blocked_row_and_bels_non_answerable() {
        let mut v = view_with_agents(vec![
            blocked_row("peer", 4, Some(answerable(&[("1", "Yes"), ("2", "No")], 9))),
            blocked_row("plain", 5, None),
        ]);
        let mut buf: Vec<u8> = Vec::new();
        let blocked = agent_row_at(&v, |a| a.name == "peer");
        v.selector = Some(blocked);
        v.open_peek(blocked, "peer".into());
        peek_keys(&mut v, b"1", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
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
        assert!(v.peek.is_some(), "overlay stays open after answering");
        // A non-answerable (focus-only) row: a digit sends nothing.
        let plain = agent_row_at(&v, |a| a.name == "plain");
        v.open_peek(plain, "plain".into());
        let mut buf2: Vec<u8> = Vec::new();
        peek_keys(&mut v, b"1", &mut buf2).await.unwrap();
        assert!(buf2.is_empty(), "no PaneAnswer for a non-answerable row");
    }

    // x-c376 AC4-HP: Enter on a pane-hosted peeked row focuses its pane and
    // closes BOTH overlays; right-arrow (folds to l) on a watch-only row attaches
    // it; AC2-EDGE: a row with no pane and no attach target refuses with a notice
    // and keeps both overlays open.
    #[tokio::test]
    async fn peek_attaches_and_refuses_a_paneless_row() {
        // Pane-hosted "worker" (pane_id 10): Enter -> FocusPane, both close.
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let worker = agent_row_at(&v, |a| a.pane_id == Some(10));
        v.selector = Some(worker);
        v.open_peek(worker, "worker".into());
        peek_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert!(v.peek.is_none(), "attach closes peek");
        assert_eq!(v.selector, None, "attach closes the selector too");
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::FocusPane(p)) => assert_eq!(p, 10),
            other => panic!("expected FocusPane, got {other:?}"),
        }
        // Watch-only "bg-claude" (attach_id): right-arrow folds to l -> AttachAgent.
        let mut v = unified_rows_view();
        let mut buf2: Vec<u8> = Vec::new();
        let bg = agent_row_at(&v, |a| a.name == "bg-claude");
        v.selector = Some(bg);
        v.open_peek(bg, "bg-claude".into());
        peek_keys(&mut v, b"\x1b[C", &mut buf2).await.unwrap();
        assert!(v.peek.is_none() && v.selector.is_none());
        let mut cur = std::io::Cursor::new(buf2);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::AttachAgent { id, .. }) => assert_eq!(id, "c19cd2c3"),
            other => panic!("expected AttachAgent, got {other:?}"),
        }
        // Orphan "bg-other" (no pane, no attach_id): Enter refuses, overlays stay.
        let mut v = unified_rows_view();
        let mut buf3: Vec<u8> = Vec::new();
        let orphan = agent_row_at(&v, |a| a.name == "bg-other");
        v.selector = Some(orphan);
        v.open_peek(orphan, "bg-other".into());
        peek_keys(&mut v, b"\r", &mut buf3).await.unwrap();
        assert!(v.peek.is_some(), "a refusal keeps peek open");
        assert_eq!(v.selector, Some(orphan), "and the selector open");
        assert!(v.notice.is_some(), "with a notice");
        assert!(buf3.is_empty(), "no command sent on a refusal");
    }

    #[tokio::test]
    async fn selector_recruit_key_opens_recruit_and_falls_back_to_focused_row() {
        // R with marks opens the prompt; with no marks it marks the focused
        // attachable row first (the grid single-recruit generalized).
        let mut v = unified_rows_view();
        let mut buf: Vec<u8> = Vec::new();
        let idx = agent_row_at(&v, |a| a.attach_id.as_deref() == Some("c19cd2c3"));
        v.selector = Some(idx);
        selector_keys(&mut v, b"R", &mut buf).await.unwrap();
        assert!(v.recruit.is_some(), "R opens the recruit prompt");
        assert!(
            v.marks.contains("c19cd2c3"),
            "zero-mark R marks the focused row"
        );
        assert_eq!(v.selector, None, "recruit is modal - the selector closes");
    }

    #[tokio::test]
    async fn recruit_keys_enter_sends_marked_ids_and_clears_marks() {
        // AC2-HP (client half): a name + Enter sends one RecruitAgents with the
        // marked ids, and the marks clear.
        let mut v = unified_rows_view();
        v.marks.insert("c19cd2c3".into());
        v.open_recruit();
        let mut buf: Vec<u8> = Vec::new();
        recruit_keys(&mut v, b"team\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        match msg {
            ClientMsg::Command(Command::RecruitAgents { squad, ids }) => {
                assert_eq!(squad, "team");
                assert_eq!(ids, vec!["c19cd2c3".to_string()]);
            }
            other => panic!("expected RecruitAgents, got {other:?}"),
        }
        assert!(v.marks.is_empty(), "submit clears the marks");
        assert_eq!(v.recruit, None, "submit closes the overlay");
    }

    #[tokio::test]
    async fn recruit_keys_esc_keeps_marks() {
        // AC2-UI: Esc cancels the prompt but keeps the marks for a re-open.
        let mut v = unified_rows_view();
        v.marks.insert("c19cd2c3".into());
        v.open_recruit();
        let mut buf: Vec<u8> = Vec::new();
        // A lone ESC is CSI-ambiguous until a following byte resolves it (the
        // fold's arrow-key safety); the trailing `x` surfaces the bare Esc and
        // then dies with the overlay.
        recruit_keys(&mut v, b"\x1bx", &mut buf).await.unwrap();
        assert_eq!(v.recruit, None, "esc closes the overlay");
        assert!(v.marks.contains("c19cd2c3"), "esc keeps the marks");
        assert!(buf.is_empty(), "esc sends nothing");
    }

    #[tokio::test]
    async fn selector_x_on_a_tombstone_sends_dismiss() {
        // AC4-EDGE (client half): x on a tombstone member row sends
        // DismissMember for its squad + attach_id (not a squad remove).
        let tomb = AgentRow {
            squad: Some(1),
            name: "cc-deadbeef".into(),
            pane_id: None,
            badge: None,
            reason: None,
            exited: true,
            answerable: None,
            attach_id: Some("deadbeef".into()),
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: true,
            tab: None,
        };
        let mut v = view_with_agents(vec![tomb]);
        v.expanded.insert(1);
        let idx = agent_row_at(&v, |a| a.tombstone);
        v.selector = Some(idx);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Command(Command::DismissMember {
                squad: 1,
                attach_id: "deadbeef".into()
            })
        );
    }

    // -- x-76ea agent-row lifecycle -------------------------------------

    /// A plain (non-tombstone) registry agent row under squad 1, varied by state.
    fn lifecycle_row(name: &str, exited: bool, external: bool) -> AgentRow {
        AgentRow {
            squad: Some(1),
            name: name.into(),
            pane_id: None,
            badge: None,
            reason: None,
            exited,
            answerable: None,
            attach_id: None,
            external,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        }
    }

    #[tokio::test]
    async fn selector_x_on_live_agent_arms_stop_confirm() {
        // US1 / AC1-HP (client half): x on a live (non-tombstone) agent row arms
        // a StopAgent confirm carrying the row's name; nothing is sent until the
        // confirm commits, and the selector closes (open_confirm).
        let mut v = view_with_agents(vec![lifecycle_row("worker-a", false, false)]);
        v.expanded.insert(1);
        v.selector = Some(agent_row_at(&v, |a| a.name == "worker-a"));
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "arming a confirm sends nothing");
        assert_eq!(v.selector, None, "the confirm closes the selector");
        match v.confirm.as_ref().map(|c| (&c.action, c.label.as_str())) {
            Some((ConfirmKind::StopAgent { name }, label)) => {
                assert_eq!(name, "worker-a");
                assert_eq!(label, "worker-a");
            }
            _ => panic!("expected a StopAgent confirm"),
        }
    }

    #[tokio::test]
    async fn selector_x_on_exited_agent_arms_remove_confirm() {
        // US2 / AC2-HP (client half): x on an exited row arms a RemoveAgent
        // confirm - the row's own state (exited) selects the verb, no timer.
        let mut v = view_with_agents(vec![lifecycle_row("worker-b", true, false)]);
        v.expanded.insert(1);
        v.selector = Some(agent_row_at(&v, |a| a.name == "worker-b"));
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert!(buf.is_empty());
        match v.confirm.as_ref().map(|c| &c.action) {
            Some(ConfirmKind::RemoveAgent { name }) => assert_eq!(name, "worker-b"),
            _ => panic!("expected a RemoveAgent confirm"),
        }
    }

    #[tokio::test]
    async fn selector_x_on_pane_hosted_agent_does_not_arm_lifecycle() {
        // codex review: a PANE-hosted Agent row (a real agent's pane or a bare
        // shell pane agent_rows() surfaces) must NOT arm stop/remove - its name
        // can be a cmd/cwd label with no registry entry, and it is managed via
        // its tab. `x` there falls through to a bell, arming nothing, sending
        // nothing.
        let mut pane_hosted = lifecycle_row("shell", false, false);
        pane_hosted.pane_id = Some(99);
        let mut v = view_with_agents(vec![pane_hosted]);
        v.expanded.insert(1);
        v.selector = Some(agent_row_at(&v, |a| a.name == "shell"));
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert!(
            buf.is_empty(),
            "a pane-hosted row sends no lifecycle command"
        );
        assert!(
            v.confirm.is_none(),
            "a pane-hosted row arms no lifecycle confirm"
        );
    }

    #[tokio::test]
    async fn selector_x_on_agent_refuses_on_short_terminal() {
        // AC4-UI (x-260a): a too-short terminal refuses with a notice rather than
        // arm an invisible confirm - same rule the squad arm follows.
        let mut v = view_with_agents(vec![lifecycle_row("worker-c", false, false)]);
        v.expanded.insert(1);
        v.term.0 = MIN_ROWS_FOR_STATUS - 1;
        v.selector = Some(agent_row_at(&v, |a| a.name == "worker-c"));
        selector_keys(&mut v, b"x", &mut Vec::new()).await.unwrap();
        assert!(
            v.confirm.is_none(),
            "no invisible confirm on a short terminal"
        );
        assert!(v.notice.is_some(), "the refusal is surfaced");
    }

    #[tokio::test]
    async fn selector_uppercase_x_on_agent_arms_reap_confirm() {
        // AC1-HP (client half): uppercase `X` on ANY agent row arms a ReapAgents
        // confirm (no payload) and sends nothing until it commits. Contextual on
        // an agent row - headers stay inert, no selector surgery.
        let mut v = view_with_agents(vec![lifecycle_row("worker-a", true, false)]);
        v.expanded.insert(1);
        v.selector = Some(agent_row_at(&v, |a| a.name == "worker-a"));
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"X", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "arming a confirm sends nothing");
        assert_eq!(v.selector, None, "the confirm closes the selector");
        assert!(
            matches!(
                v.confirm.as_ref().map(|c| &c.action),
                Some(ConfirmKind::ReapAgents)
            ),
            "expected a ReapAgents confirm"
        );
    }

    #[tokio::test]
    async fn selector_uppercase_x_on_non_agent_row_arms_nothing() {
        // Contextual: `X` on a squad-header row (not an agent row) BELs and arms
        // no confirm - the bulk-reap gesture only fires from an agent row.
        let mut v = view_with_agents(vec![lifecycle_row("worker-a", true, false)]);
        let squad_row = v
            .display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Sel(s) if s.tab.is_none()))
            .expect("a squad-header row");
        v.selector = Some(squad_row);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"X", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "a non-agent row sends nothing");
        assert!(v.confirm.is_none(), "a non-agent row arms no reap confirm");
    }

    #[tokio::test]
    async fn selector_uppercase_x_refuses_on_short_terminal() {
        // AC1-UI (x-260a): a too-short terminal refuses with a notice rather than
        // arm an invisible confirm, matching the per-row `x` arm.
        let mut v = view_with_agents(vec![lifecycle_row("worker-a", true, false)]);
        v.expanded.insert(1);
        v.term.0 = MIN_ROWS_FOR_STATUS - 1;
        v.selector = Some(agent_row_at(&v, |a| a.name == "worker-a"));
        selector_keys(&mut v, b"X", &mut Vec::new()).await.unwrap();
        assert!(
            v.confirm.is_none(),
            "no invisible confirm on a short terminal"
        );
        assert!(v.notice.is_some(), "the refusal is surfaced");
    }

    #[tokio::test]
    async fn confirm_keys_enter_sends_reap_agents() {
        // AC1-HP (client half): Enter on an armed ReapAgents confirm sends the
        // payload-free ReapAgents command.
        let mut v = view_with_agents(vec![]);
        v.confirm = Some(ConfirmAction {
            action: ConfirmKind::ReapAgents,
            label: String::new(),
        });
        let mut buf: Vec<u8> = Vec::new();
        confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(decoded, ClientMsg::Command(Command::ReapAgents));
    }

    #[tokio::test]
    async fn selector_x_on_live_external_arms_stop_external() {
        // AC2-HP (client half): x on a live external row routes to StopExternal
        // by its stable attach_id, NOT fno-agents-by-name.
        let mut row = lifecycle_row("ext-a", false, true);
        row.attach_id = Some("deadbeef".into());
        let mut v = view_with_agents(vec![row]);
        v.expanded.insert(1);
        v.selector = Some(agent_row_at(&v, |a| a.name == "ext-a"));
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "arming a confirm sends nothing");
        match v.confirm.as_ref().map(|c| &c.action) {
            Some(ConfirmKind::StopExternal { attach_id, name }) => {
                assert_eq!(attach_id, "deadbeef");
                assert_eq!(name, "ext-a");
            }
            _ => panic!("expected a StopExternal confirm"),
        }
    }

    #[tokio::test]
    async fn selector_x_on_stopped_external_arms_remove_external() {
        // AC3-HP (client half): x on an exited external tombstone routes to
        // RemoveExternal by attach_id (the stopped tombstone `exited` maps to rm).
        let mut row = lifecycle_row("ext-b", true, true);
        row.attach_id = Some("cafef00d".into());
        let mut v = view_with_agents(vec![row]);
        v.expanded.insert(1);
        v.selector = Some(agent_row_at(&v, |a| a.name == "ext-b"));
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        match v.confirm.as_ref().map(|c| &c.action) {
            Some(ConfirmKind::RemoveExternal { attach_id, name }) => {
                assert_eq!(attach_id, "cafef00d");
                assert_eq!(name, "ext-b");
            }
            _ => panic!("expected a RemoveExternal confirm"),
        }
    }

    #[tokio::test]
    async fn confirm_keys_enter_sends_external_commands() {
        for (kind, want) in [
            (
                ConfirmKind::StopExternal {
                    attach_id: "deadbeef".into(),
                    name: "e".into(),
                },
                Command::StopExternal {
                    attach_id: "deadbeef".into(),
                    name: "e".into(),
                },
            ),
            (
                ConfirmKind::RemoveExternal {
                    attach_id: "cafef00d".into(),
                    name: "e".into(),
                },
                Command::RemoveExternal {
                    attach_id: "cafef00d".into(),
                    name: "e".into(),
                },
            ),
        ] {
            let mut v = view_with_agents(vec![]);
            v.confirm = Some(ConfirmAction {
                action: kind,
                label: "e".into(),
            });
            let mut buf: Vec<u8> = Vec::new();
            confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
            let mut cur = std::io::Cursor::new(buf);
            let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
            assert_eq!(decoded, ClientMsg::Command(want));
        }
    }

    #[tokio::test]
    async fn confirm_keys_enter_sends_stop_then_remove_agent() {
        // US1/US2 (client half): Enter on an armed StopAgent/RemoveAgent confirm
        // sends the captured-name command (the row index is never re-read).
        for (kind, want) in [
            (
                ConfirmKind::StopAgent { name: "w".into() },
                Command::StopAgent { name: "w".into() },
            ),
            (
                ConfirmKind::RemoveAgent { name: "w".into() },
                Command::RemoveAgent { name: "w".into() },
            ),
        ] {
            let mut v = view_with_agents(vec![]);
            v.confirm = Some(ConfirmAction {
                action: kind,
                label: "w".into(),
            });
            let mut buf: Vec<u8> = Vec::new();
            confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
            let mut cur = std::io::Cursor::new(buf);
            let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
            assert_eq!(decoded, ClientMsg::Command(want));
        }
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
            ClientMsg::Command(Command::AttachAgent { id, placement }) => {
                assert_eq!(id, "c19cd2c3");
                assert_eq!(placement, crate::proto::PanePlacement::default());
            }
            other => panic!("expected AttachAgent, got {other:?}"),
        }
        assert_eq!(v.selector, None);
    }

    #[tokio::test]
    async fn selector_p_opens_attach_placement_without_sending() {
        let mut v = unified_rows_view();
        v.selector = Some(5);
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        let picker = v.attach_place.as_ref().expect("placement picker opens");
        assert_eq!(picker.id, "c19cd2c3");
        assert_eq!(picker.target, 1);
        assert_eq!(picker.squads, vec![1, 2]);
        let overlay = v.attach_place_lines(picker).join("\n");
        for label in ["left", "right", "up", "down", "new tab", "cancel"] {
            assert!(overlay.contains(label), "missing {label}: {overlay}");
        }
        assert!(buf.is_empty());
        assert_eq!(v.selector, None);
    }

    #[tokio::test]
    async fn attach_placement_selects_target_and_direction() {
        let mut v = unified_rows_view();
        v.selector = Some(5);
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        attach_place_keys(&mut v, b"2h", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Command(Command::AttachAgent {
                id: "c19cd2c3".into(),
                placement: PanePlacement {
                    target: PaneTarget::SquadId(2),
                    split: Some(Dir::Left),
                    here: false,
                },
            })
        );
        assert!(v.attach_place.is_none());
    }

    #[tokio::test]
    async fn attach_placement_invalid_target_digit_drops_the_input_batch() {
        let mut v = unified_rows_view();
        v.selector = Some(5);
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        attach_place_keys(&mut v, b"9h", &mut buf).await.unwrap();
        assert!(buf.is_empty());
        assert_eq!(v.attach_place.as_ref().unwrap().target, 1);
        assert!(v.notice.is_some());
    }

    #[tokio::test]
    async fn attach_placement_new_tab_and_cancel_are_distinct() {
        let mut v = unified_rows_view();
        v.selector = Some(5);
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        attach_place_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Command(Command::AttachAgent {
                id: "c19cd2c3".into(),
                placement: PanePlacement {
                    target: PaneTarget::SquadId(1),
                    split: None,
                    here: false,
                },
            })
        );

        v.selector = Some(5);
        let mut cancelled = Vec::new();
        selector_keys(&mut v, b"p", &mut cancelled).await.unwrap();
        attach_place_keys(&mut v, b"q", &mut cancelled)
            .await
            .unwrap();
        assert!(cancelled.is_empty());
        assert!(v.attach_place.is_none());
    }

    #[tokio::test]
    async fn attach_placement_refuses_stale_target_without_sending() {
        let mut v = unified_rows_view();
        v.selector = Some(5);
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        v.attach_place.as_mut().unwrap().target = 2;
        v.layout.squads.retain(|s| s.id != 2);
        attach_place_keys(&mut v, b"l", &mut buf).await.unwrap();
        assert!(buf.is_empty());
        assert!(v.notice.is_some());
        assert!(v.attach_place.is_none());
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
        assert!(
            matches!(&v.confirm.as_ref().unwrap().action, ConfirmKind::Dispatch { node } if node == "x-rdy"),
            "the Ready card's node is armed for dispatch"
        );
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
        view.nav = Some(NavView {
            query: "half".into(),
            state_filter: None,
            cursor: 0,
        });
        view.open_confirm(ConfirmAction {
            action: ConfirmKind::Dispatch {
                node: "x-rdy".into(),
            },
            label: "x-rdy".into(),
        });
        assert!(view.selector.is_none(), "confirm clears an open selector");
        assert!(view.answers.is_none(), "confirm clears the answer overlay");
        assert!(view.create.is_none(), "confirm drops a half-typed create");
        assert!(view.search.is_none());
        assert!(
            view.nav.is_none(),
            "confirm clears an open navigator (x-653d)"
        );
        assert!(
            matches!(&view.confirm.as_ref().unwrap().action, ConfirmKind::Dispatch { node } if node == "x-rdy"),
            "the armed confirm carries the node"
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
        assert_eq!(v.selector, Some(5), "j skips the '~ elsewhere' header");
        selector_keys(&mut v, b"k", &mut buf).await.unwrap();
        assert_eq!(v.selector, Some(3), "k skips it back");
        assert!(buf.is_empty(), "navigation sends nothing");
    }

    // ---- x-653d: session navigator (leader+f) ----

    #[test]
    fn nav_rows_lists_every_squads_tabs_ignoring_expand() {
        // AC1-HP + Locked 3: the flat catalog lists a COLLAPSED squad's tabs too
        // (the sideline gates tabs behind expand; the navigator never does).
        let v = two_pane_view(); // footnote(active, tabs 1&2), notes(collapsed, tab 1)
        assert!(!v.expanded.contains(&2), "notes is collapsed");
        let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
        for want in [
            "footnote",
            "footnote › 1",
            "footnote › 2",
            "notes",
            "notes › 1",
        ] {
            assert!(
                labels.iter().any(|l| l == want),
                "missing {want:?} in {labels:?}"
            );
        }
    }

    #[test]
    fn nav_rows_agent_label_carries_tab_ordinal() {
        // x-0090 US4: a pane-hosted agent's nav label names its tab with a `·N`
        // ordinal (fixture tab id 1 is the 2nd tab -> ·2), coherent with the
        // sideline; a watch-only row (no tab) carries none.
        let mut v = two_pane_view();
        v.layout.agents = vec![
            AgentRow {
                squad: Some(1),
                name: "build".into(),
                pane_id: Some(10),
                badge: Some(AgentBadge::Working),
                reason: None,
                exited: false,
                answerable: None,
                attach_id: None,
                external: false,
                seen: false,
                cwd_base: None,
                tombstone: false,
                tab: Some(1),
            },
            AgentRow {
                squad: Some(1),
                name: "watcher".into(),
                pane_id: None,
                badge: None,
                reason: None,
                exited: false,
                answerable: None,
                attach_id: Some("deadbee1".into()),
                external: false,
                seen: false,
                cwd_base: None,
                tombstone: false,
                tab: None,
            },
        ];
        let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
        assert!(
            labels.iter().any(|l| l == "footnote › build ·2"),
            "pane row names its tab: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "footnote › watcher"),
            "watch-only row has no ordinal: {labels:?}"
        );
    }

    #[test]
    fn squad_rollup_bare_pane_folds_to_idle() {
        // x-0090 US4: the pane-union adds bare panes to the agent set; a bare
        // pane (badge None) folds to Idle so it never overrides a blocked
        // sibling in the x-d140 collapsed-squad rollup (Ord-min over states).
        let row = |name: &str, pane, badge| AgentRow {
            squad: Some(1),
            name: name.into(),
            pane_id: Some(pane),
            badge,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        };
        let bare = row("zsh", 10, None);
        let blocked = row("claude", 11, Some(AgentBadge::Blocked));
        assert_eq!(nav_agent_state(&bare), PaneState::Idle);
        let worst = [&bare, &blocked]
            .iter()
            .map(|a| nav_agent_state(a))
            .min()
            .unwrap();
        assert_eq!(
            worst,
            PaneState::Blocked,
            "blocked wins; the bare pane folds to Idle"
        );
    }

    #[test]
    fn nav_filter_text_is_case_insensitive_substring() {
        // AC2-HP + AC2-UI: typed text narrows to matching labels; case-folded.
        let v = two_pane_view();
        let nav = NavView {
            query: "NOTES".into(),
            state_filter: None,
            cursor: 0,
        };
        let rows = v.nav_filtered(&nav);
        assert_eq!(
            rows.len(),
            2,
            "notes squad + its one tab, footnote excluded"
        );
        assert!(rows
            .iter()
            .all(|r| r.label.to_lowercase().contains("notes")));
    }

    #[test]
    fn nav_filter_state_composes_with_text() {
        // AC3-HP + AC3-EDGE: text AND state both apply. Squad/tab rows are Idle,
        // so a [blocked] chip leaves only the blocked agent.
        let mut v = two_pane_view();
        v.layout.agents = vec![AgentRow {
            squad: Some(2),
            name: "stuck".into(),
            pane_id: Some(9),
            badge: Some(AgentBadge::Blocked),
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        }];
        let composed = NavView {
            query: "notes".into(),
            state_filter: Some(PaneState::Blocked),
            cursor: 0,
        };
        let rows = v.nav_filtered(&composed);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "notes › stuck");
        assert_eq!(rows[0].state, PaneState::Blocked);
        let state_only = NavView {
            query: String::new(),
            state_filter: Some(PaneState::Blocked),
            cursor: 0,
        };
        assert_eq!(
            v.nav_filtered(&state_only).len(),
            1,
            "[blocked] excludes the Idle squad/tab rows"
        );
    }

    #[test]
    fn nav_rows_fold_done_through_the_seen_bit() {
        // AC1-HP/AC2-HP (x-4328), at the navigator seam: a seen Done row
        // folds to Idle (the unseen glyph clears); an unseen Done row stays
        // DoneUnseen (surfaced) - `nav_agent_state` must forward `a.seen`,
        // not hardcode it.
        let mut v = two_pane_view();
        v.layout.agents = vec![
            AgentRow {
                squad: Some(2),
                name: "finished-seen".into(),
                pane_id: Some(9),
                badge: Some(AgentBadge::Done),
                reason: None,
                exited: false,
                answerable: None,
                attach_id: None,
                external: false,
                seen: true,
                cwd_base: None,
                tombstone: false,
                tab: None,
            },
            AgentRow {
                squad: Some(2),
                name: "finished-unseen".into(),
                pane_id: Some(10),
                badge: Some(AgentBadge::Done),
                reason: None,
                exited: false,
                answerable: None,
                attach_id: None,
                external: false,
                seen: false,
                cwd_base: None,
                tombstone: false,
                tab: None,
            },
        ];
        let rows = v.nav_rows();
        let seen_row = rows.iter().find(|r| r.label.ends_with("finished-seen"));
        let unseen_row = rows.iter().find(|r| r.label.ends_with("finished-unseen"));
        assert_eq!(seen_row.map(|r| r.state), Some(PaneState::Idle));
        assert_eq!(unseen_row.map(|r| r.state), Some(PaneState::DoneUnseen));
    }

    #[test]
    fn nav_overlay_lines_show_query_chip_cursor_and_no_matches() {
        // AC1-UI: query line + [all] chip + cursor `▸` on row 0. AC2-ERR: an
        // empty filtered result renders `no matches`.
        let v = two_pane_view();
        let nav = NavView {
            query: String::new(),
            state_filter: None,
            cursor: 0,
        };
        let rows = v.nav_filtered(&nav);
        let lines = nav_overlay_lines(&rows, &nav);
        assert!(lines[0].contains("find ›") && lines[0].contains("[all]"));
        assert!(
            lines[1].trim_start().starts_with('▸'),
            "cursor on row 0: {:?}",
            lines[1]
        );
        let empty = NavView {
            query: "zzzz".into(),
            state_filter: None,
            cursor: 0,
        };
        let rows = v.nav_filtered(&empty);
        let lines = nav_overlay_lines(&rows, &empty);
        assert!(lines.iter().any(|l| l.contains("no matches")));
    }

    #[test]
    fn nav_cursor_clamps_no_wrap() {
        // Boundaries: Ctrl-p at the top and Ctrl-n past the last filtered row
        // both clamp, never wrap.
        let mut v = two_pane_view();
        let n = v.nav_rows().len();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: 0,
        });
        v.nav_move_cursor(-1);
        assert_eq!(v.nav.as_ref().unwrap().cursor, 0, "clamp at the top");
        for _ in 0..(n + 5) {
            v.nav_move_cursor(1);
        }
        assert_eq!(
            v.nav.as_ref().unwrap().cursor,
            n - 1,
            "clamp at the last row"
        );
    }

    #[tokio::test]
    async fn nav_goto_teleports_cross_squad_then_focuses() {
        // AC4-HP: goto an agent in a collapsed, non-active squad sends
        // SelectSquad then FocusPane in order, and closes the navigator.
        let mut v = two_pane_view(); // active squad = 1 (footnote)
        v.layout.agents = vec![AgentRow {
            squad: Some(2),
            name: "stuck".into(),
            pane_id: Some(9),
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        }];
        let idx = v
            .nav_rows()
            .iter()
            .position(|r| r.label == "notes › stuck")
            .unwrap();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: idx,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_goto(&mut v, &mut buf).await.unwrap();
        assert!(v.nav.is_none(), "goto closes the navigator");
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::SelectSquad(2))
        ));
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::FocusPane(9))
        ));
    }

    #[tokio::test]
    async fn nav_goto_same_squad_is_a_bare_focus() {
        // AC4-UI: a pane already in the active squad collapses to a bare
        // FocusPane - no redundant SelectSquad.
        let mut v = unified_rows_view(); // worker: sq1 (active), pane 10
        let idx = v
            .nav_rows()
            .iter()
            .position(|r| r.label == "footnote › worker")
            .unwrap();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: idx,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_goto(&mut v, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::FocusPane(10))
        ));
        assert!(
            crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).is_err(),
            "bare focus only - no SelectSquad"
        );
    }

    #[tokio::test]
    async fn nav_goto_refusal_keeps_navigator_open() {
        // AC4-FR + Locked 6: Enter on a Blocked card shows a notice, sends
        // nothing, and the navigator stays open.
        let mut v = unified_rows_view();
        let idx = v
            .nav_rows()
            .iter()
            .position(|r| r.label.starts_with("x-blk"))
            .unwrap();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: idx,
        });
        v.notice = None;
        let mut buf: Vec<u8> = Vec::new();
        nav_goto(&mut v, &mut buf).await.unwrap();
        assert!(buf.is_empty(), "refusal sends nothing");
        assert!(v.notice.is_some(), "refusal explains itself");
        assert!(v.nav.is_some(), "navigator stays open");
    }

    #[tokio::test]
    async fn nav_keys_type_tab_and_esc() {
        // AC2-HP: printable bytes edit the query (never leak). AC3-UI: Tab cycles
        // the state chip. Esc closes (a lone ESC stays pending until the next
        // byte disambiguates it - same fold as search; the trailing byte is
        // swallowed on close).
        let mut v = two_pane_view();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: 0,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_keys(&mut v, b"notes", &mut buf).await.unwrap();
        assert_eq!(v.nav.as_ref().unwrap().query, "notes");
        assert!(buf.is_empty(), "typing sends nothing");
        nav_keys(&mut v, b"\t", &mut buf).await.unwrap();
        assert_eq!(
            v.nav.as_ref().unwrap().state_filter,
            Some(PaneState::Blocked),
            "Tab advances the chip to [blocked]"
        );
        nav_keys(&mut v, b"\x1bx", &mut buf).await.unwrap();
        assert!(v.nav.is_none(), "Esc closes; the trailing x is swallowed");
    }

    #[tokio::test]
    async fn nav_keys_arrows_move_cursor() {
        // AC1 (ab-63b44059): Down/Up move the cursor one filtered row (same as
        // Ctrl-n/Ctrl-p), clamped no-wrap; arrows never leak to the pane; and
        // printable input still edits the query afterwards (Locked-5).
        let mut v = two_pane_view();
        assert!(v.nav_rows().len() >= 2, "fixture needs >=2 nav rows");
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: 0,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_keys(&mut v, b"\x1b[B", &mut buf).await.unwrap();
        assert_eq!(v.nav.as_ref().unwrap().cursor, 1, "Down -> row 1");
        nav_keys(&mut v, b"\x1b[A", &mut buf).await.unwrap();
        assert_eq!(v.nav.as_ref().unwrap().cursor, 0, "Up -> row 0");
        nav_keys(&mut v, b"\x1b[A", &mut buf).await.unwrap();
        assert_eq!(
            v.nav.as_ref().unwrap().cursor,
            0,
            "Up at top clamps (no wrap)"
        );
        assert!(buf.is_empty(), "arrows send nothing to the pane");
        nav_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert_eq!(
            v.nav.as_ref().unwrap().query,
            "x",
            "letter still edits query"
        );
    }

    #[tokio::test]
    async fn nav_keys_shift_tab_reverse_cycles_state() {
        // AC2 (ab-63b44059): Shift-Tab steps to the PREVIOUS state in the ring
        // (reverse of Tab, which advances None -> Blocked) and re-clamps cursor 0.
        let mut v = two_pane_view();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: 1,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_keys(&mut v, b"\x1b[Z", &mut buf).await.unwrap();
        assert_eq!(
            v.nav.as_ref().unwrap().state_filter,
            Some(PaneState::Idle),
            "Shift-Tab reverses to [idle]"
        );
        assert_eq!(v.nav.as_ref().unwrap().cursor, 0, "cursor re-clamped to 0");
        assert!(buf.is_empty(), "Shift-Tab sends nothing to the pane");
    }

    #[tokio::test]
    async fn nav_keys_split_arrow_carries_across_reads() {
        // AC4 (ab-63b44059): a Down arrow split across two reads (ESC[ then B)
        // carries via nav_esc and still moves the cursor, with no stray byte
        // leaking to the query or the pane.
        let mut v = two_pane_view();
        assert!(v.nav_rows().len() >= 2);
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: 0,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_keys(&mut v, b"\x1b[", &mut buf).await.unwrap();
        assert_eq!(
            v.nav.as_ref().unwrap().cursor,
            0,
            "partial seq: no motion yet"
        );
        nav_keys(&mut v, b"B", &mut buf).await.unwrap();
        assert_eq!(
            v.nav.as_ref().unwrap().cursor,
            1,
            "completed Down moves cursor"
        );
        assert!(
            v.nav.as_ref().unwrap().query.is_empty(),
            "no escape tail leaked into the query"
        );
        assert!(buf.is_empty(), "nothing leaked to the pane");
    }

    #[test]
    fn sideline_scroll_follows_cursor_and_maps_hit() {
        // AC1+AC2 (x-a621): a selector driven below the fold scrolls the sideline
        // to keep it visible, and a click on a scrolled row hit-tests to the right
        // display index (no off-by-offset).
        let mut v = two_pane_view();
        let total = v.display_rows().len();
        assert!(total >= 2, "fixture needs >=2 sideline rows");
        v.term = (total as u16, 100); // visible = total - 1: one row below the fold
        let visible = v.sideline_visible_rows();
        v.selector = Some(total - 1);
        v.clamp_sideline_offset();
        assert_eq!(
            v.sideline_offset,
            total - visible,
            "offset follows the cursor"
        );
        assert!(
            (total - 1) >= v.sideline_offset && (total - 1) < v.sideline_offset + visible,
            "the cursor row is inside the visible window"
        );
        assert!(v.panel_w() > 1, "fixture panel is visible");
        assert_eq!(
            v.sideline_row_at(TAB_BAR_ROWS, 0),
            Some(v.sideline_offset),
            "the top drawn row hit-tests to the scrolled index"
        );
    }

    #[test]
    fn sideline_scroll_zero_when_rows_fit() {
        // AC3 (x-a621): when every row fits the height the offset stays 0, so the
        // frame renders exactly as a non-scrolling sideline.
        let mut v = two_pane_view(); // tall terminal, small catalog
        assert!(
            v.display_rows().len() <= v.sideline_visible_rows(),
            "catalog fits the window"
        );
        v.selector = Some(0);
        v.sideline_offset = 9; // stale offset from a prior scrolled session
        v.clamp_sideline_offset();
        assert_eq!(v.sideline_offset, 0, "fits -> offset resets to 0");
    }

    #[test]
    fn sideline_scroll_never_past_last_row() {
        // AC4 (x-a621): an offset left too large by a catalog shrink re-clamps into
        // [0, rows - visible]; it never scrolls past the last row.
        let mut v = two_pane_view();
        let total = v.display_rows().len();
        assert!(total >= 2);
        v.term = (total as u16, 100); // visible = total - 1
        v.selector = None;
        v.hover_row = None;
        v.sideline_offset = 999; // absurd, e.g. after the catalog shrank
        v.clamp_sideline_offset();
        assert_eq!(
            v.sideline_offset,
            total - v.sideline_visible_rows(),
            "clamped to the last full window"
        );
    }

    #[test]
    fn sideline_scroll_window_excludes_chrome_bottom_row() {
        // Regression (code-reviewer): the bottom status row is chrome-owned and
        // overwritten after the sideline paints, and sideline_row_at excludes it,
        // so it must not count as a scroll slot - otherwise follow-cursor scroll
        // parks the last row under the status bar.
        let mut v = two_pane_view();
        v.term = ((MIN_ROWS_FOR_STATUS as usize).max(10) as u16, 100);
        // Clear every chrome trigger, then toggle only status_on so the branch
        // under test is the bottom-chrome subtraction, nothing else.
        v.confirm = None;
        v.create = None;
        v.rename = None;
        v.search = None;
        v.hint = false;
        v.status_on = true;
        assert!(
            v.bottom_row_is_chrome(),
            "status bar occupies the bottom row"
        );
        assert_eq!(
            v.sideline_visible_rows(),
            v.term.0 as usize - TAB_BAR_ROWS as usize - 1,
            "chrome bottom row is not a scroll slot"
        );
        v.status_on = false;
        assert!(
            !v.bottom_row_is_chrome(),
            "no chrome -> bottom row reclaimed"
        );
        assert_eq!(
            v.sideline_visible_rows(),
            v.term.0 as usize - TAB_BAR_ROWS as usize,
            "with no chrome the full height below the tab bar is usable"
        );
    }

    #[test]
    fn nav_cursor_re_clamps_on_layout_shrink() {
        // AC1-FR / AC2-EDGE: a layout push that shrinks the catalog under an open
        // navigator re-clamps the cursor into the live rows (no past-the-end
        // marker, no mis-targeted Enter) without reopening the overlay.
        let mut v = two_pane_view();
        let last = v.nav_rows().len() - 1;
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: last,
        });
        v.set_layout(LayoutView {
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
        let n = v.nav_rows().len();
        assert!(n < last + 1, "catalog shrank");
        assert_eq!(
            v.nav.as_ref().unwrap().cursor,
            n - 1,
            "cursor clamped into the shrunk catalog"
        );
        assert!(v.nav.is_some(), "navigator stays open across the push");
    }

    #[test]
    fn nav_rows_lists_plain_panes_and_dedups_agent_panes() {
        // Fold-in (codex): plain panes become goto rows; a pane already shown as
        // an agent row is NOT double-listed (the agent row is the richer view).
        let mut v = two_pane_view();
        v.layout.squads[0].tabs[1].panes = vec![
            PaneMeta {
                id: 10,
                label: "claude".into(),
            },
            PaneMeta {
                id: 20,
                label: "htop".into(),
            },
        ];
        v.layout.agents = vec![AgentRow {
            squad: Some(1),
            name: "worker".into(),
            pane_id: Some(10),
            badge: None,
            reason: None,
            exited: false,
            answerable: None,
            attach_id: None,
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        }];
        let labels: Vec<String> = v.nav_rows().into_iter().map(|r| r.label).collect();
        assert!(
            labels.iter().any(|l| l == "footnote › 2 › htop"),
            "plain pane listed: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.ends_with("› claude")),
            "agent-hosted pane not double-listed: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "footnote › worker"),
            "the agent keeps its own row"
        );
    }

    #[tokio::test]
    async fn nav_goto_pane_cross_squad_sends_squad_tab_focus() {
        // Fold-in AC4-HP (now fulfilled): a pane in a non-active squad+tab sends
        // SelectSquad, SelectTab, FocusPane in order.
        let mut v = two_pane_view(); // active squad 1; notes = squad 2, tab id 0
        v.layout.squads[1].tabs[0].panes = vec![PaneMeta {
            id: 55,
            label: "vim".into(),
        }];
        let idx = v
            .nav_rows()
            .iter()
            .position(|r| r.label == "notes › 1 › vim")
            .unwrap();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: idx,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_goto(&mut v, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::SelectSquad(2))
        ));
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::SelectTab(0))
        ));
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::FocusPane(55))
        ));
    }

    #[tokio::test]
    async fn nav_goto_pane_active_view_is_bare_focus() {
        // AC4-UI: a pane in the active squad AND active tab collapses to a bare
        // FocusPane - no redundant SelectSquad/SelectTab.
        let mut v = two_pane_view(); // active squad 1, active_tab idx 1 (tab id 1)
        v.layout.squads[0].tabs[1].panes = vec![PaneMeta {
            id: 77,
            label: "shell".into(),
        }];
        let idx = v
            .nav_rows()
            .iter()
            .position(|r| r.label == "footnote › 2 › shell")
            .unwrap();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: idx,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_goto(&mut v, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::FocusPane(77))
        ));
        assert!(
            crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).is_err(),
            "bare focus, no prefix"
        );
    }

    #[tokio::test]
    async fn nav_goto_pane_same_squad_other_tab_selects_tab_only() {
        // A pane in the active squad but a different tab: SelectTab then
        // FocusPane, no SelectSquad.
        let mut v = two_pane_view(); // active squad 1, active_tab idx 1 (id 1)
        v.layout.squads[0].tabs[0].panes = vec![PaneMeta {
            id: 88,
            label: "logs".into(),
        }]; // tab idx 0, id 0
        let idx = v
            .nav_rows()
            .iter()
            .position(|r| r.label == "footnote › 1 › logs")
            .unwrap();
        v.nav = Some(NavView {
            query: String::new(),
            state_filter: None,
            cursor: idx,
        });
        let mut buf: Vec<u8> = Vec::new();
        nav_goto(&mut v, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::SelectTab(0))
        ));
        assert!(matches!(
            crate::proto::read_msg_sync(&mut cur).unwrap(),
            ClientMsg::Command(Command::FocusPane(88))
        ));
        assert!(
            crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).is_err(),
            "no SelectSquad for the active squad"
        );
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
            external: false,
            seen: false,
            cwd_base: None,
            tombstone: false,
            tab: None,
        }
    }

    fn view_with_agents(agents: Vec<AgentRow>) -> View {
        let mut v = two_pane_view();
        v.layout.agents = agents;
        v
    }

    // A roster row with an arbitrary badge/seen (x-feec): a join target for a
    // fold item, or a done-unseen leg-1 row.
    fn agent_row(name: &str, pane: u64, badge: Option<AgentBadge>, seen: bool) -> AgentRow {
        let mut r = blocked_row(name, pane, None);
        r.badge = badge;
        r.seen = seen;
        r
    }

    fn fold_item(kind: &str, name: &str, live: bool) -> crate::needs_overlay::FoldItem {
        crate::needs_overlay::FoldItem {
            kind: kind.into(),
            session_id: format!("sess-{name}"),
            node: Some(name.into()),
            name: Some(name.into()),
            title: None,
            ts: "2026-07-03T02:00:00Z".into(),
            evidence: format!("{kind} evidence"),
            live,
        }
    }

    #[test]
    fn xc929_pad_to_truncates_and_pads() {
        assert_eq!(pad_to("hi", 5), "hi   ");
        assert_eq!(pad_to("hello", 5), "hello");
        assert_eq!(pad_to("hello world", 5), "hell…");
    }

    // AC1-HP + AC1-EDGE (x-feec): the selected row is marked, an answerable row
    // lists its numbered options, a focus-only row is tagged; the answerable
    // kind sorts ahead of focus-only (severity order).
    #[test]
    fn needs_overlay_lines_mark_selection_and_tag_focus_only() {
        let v = view_with_agents(vec![
            blocked_row("peer", 4, Some(answerable(&[("1", "Yes"), ("2", "No")], 7))),
            blocked_row("other", 5, None),
        ]);
        let (queue, dropped) = v.needs_view();
        let lines = needs_overlay_lines(&queue, 0, dropped, NeedsFooter::AsOf);
        assert!(
            lines[1].contains('▸') && lines[1].contains("peer"),
            "{:?}",
            lines[1]
        );
        assert!(lines[2].contains("other") && lines[2].contains("⚠ focus"));
        assert!(lines.iter().any(|l| l.contains("1. Yes")));
        assert!(lines.iter().any(|l| l.contains("2. No")));
        // Selecting the focus-only row shows no answer options.
        let lines = needs_overlay_lines(&queue, 1, dropped, NeedsFooter::AsOf);
        assert!(!lines.iter().any(|l| l.contains("1. Yes")));
    }

    // The empty union renders the "nothing needs you" state, never a blank
    // overlay (AC4-EDGE), and states the drop count when the cap trims (footer).
    #[test]
    fn needs_overlay_lines_empty_and_capped_footers() {
        let empty = needs_overlay_lines(&[], 0, 0, NeedsFooter::AsOf);
        assert!(empty.iter().any(|l| l.contains("nothing needs you")));
        let one = vec![NeedRow {
            kind: NeedKind::BudgetStop,
            name: "x".into(),
            reason: "stopped".into(),
            ts: String::new(),
            id_key: "x".into(),
            answerable: None,
            pane_id: Some(1),
            attach_id: None,
            squad: Some(1),
            tab: None,
        }];
        let degraded = needs_overlay_lines(&one, 0, 0, NeedsFooter::Degraded);
        assert!(degraded
            .iter()
            .any(|l| l.contains("events fold unavailable")));
        let capped = needs_overlay_lines(&one, 0, 7, NeedsFooter::AsOf);
        assert!(capped.iter().any(|l| l.contains("7 more hidden")));
    }

    #[test]
    fn needs_queue_filters_to_live_blocked_rows() {
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
        assert_eq!(
            v.needs_queue()
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "only live blocked rows"
        );
    }

    // AC3-UI (x-feec): a scrape tick that drops the selected pane re-anchors the
    // cursor by identity; an emptied queue keeps the overlay open in its
    // "nothing needs you" state (does NOT close under the user).
    #[test]
    fn needs_reanchor_keeps_cursor_and_stays_open_when_empty() {
        let mut v = view_with_agents(vec![blocked_row("a", 1, None), blocked_row("b", 2, None)]);
        v.answers = Some(1);
        let with = |v: &View, agents: Vec<AgentRow>| LayoutView {
            squads: v.layout.squads.clone(),
            active_squad: 1,
            panes: v.layout.panes.clone(),
            focus: v.layout.focus,
            area: v.layout.area,
            agents,
            focus_node: None,
            backlog: Vec::new(),
        };
        // "b" drops -> its identity is gone, so the cursor clamps to the new last.
        let l1 = with(&v, vec![blocked_row("a", 1, None)]);
        v.set_layout(l1);
        assert_eq!(v.answers, Some(0));
        // Queue empties -> overlay stays open (AC4-EDGE empty state), cursor at 0.
        let l2 = with(&v, vec![]);
        v.set_layout(l2);
        assert_eq!(v.answers, Some(0), "empty keeps the overlay open");
    }

    // INV (x-feec): NeedKind declaration order IS the severity contract.
    #[test]
    fn needs_kind_ord_is_declaration_order() {
        use NeedKind::*;
        let mut ks = vec![
            DoneUnseen,
            BudgetStop,
            ReviewWedged,
            BlockedFocusOnly,
            BlockedAnswerable,
            Decision,
        ];
        ks.sort();
        assert_eq!(
            ks,
            vec![
                Decision,
                BlockedAnswerable,
                BlockedFocusOnly,
                ReviewWedged,
                BudgetStop,
                DoneUnseen
            ]
        );
    }

    // AC1-HP (x-feec): the live badge leg + the event-fold leg merge into one
    // worst-first queue: answerable, focus-only, review-wedged, budget-stopped,
    // done-unseen.
    #[test]
    fn needs_queue_merges_and_ranks_five_kinds() {
        let mut v = view_with_agents(vec![
            blocked_row("ans", 1, Some(answerable(&[("1", "Y")], 3))),
            blocked_row("foc", 2, None),
            agent_row("dn", 3, Some(AgentBadge::Done), false),
            agent_row("rw", 4, Some(AgentBadge::Working), false),
            agent_row("bs", 5, Some(AgentBadge::Working), false),
        ]);
        v.needs_fold = Some(vec![
            fold_item("budget_stop", "bs", false),
            fold_item("review_wedged", "rw", false),
        ]);
        assert_eq!(
            v.needs_queue()
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>(),
            vec!["ans", "foc", "rw", "bs", "dn"]
        );
    }

    // Locked 5 (x-feec): an unjoined fold item renders only when live (squadless
    // with no pane), else it is dropped (a dead session's stale stop never nags).
    #[test]
    fn needs_fold_drops_dead_and_renders_live_squadless() {
        let mut v = view_with_agents(vec![]);
        v.needs_fold = Some(vec![
            fold_item("budget_stop", "gone", false),
            fold_item("review_wedged", "alive", true),
        ]);
        let q = v.needs_queue();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].name, "alive");
        assert_eq!(q[0].kind, NeedKind::ReviewWedged);
        assert!(q[0].pane_id.is_none(), "squadless row has no pane");
    }

    // AC5-FR (x-feec): Enter on a joined fold row focuses its pane (goto).
    #[tokio::test]
    async fn needs_enter_goto_focuses_joined_fold_row() {
        let mut v = view_with_agents(vec![agent_row("bs", 5, Some(AgentBadge::Working), false)]);
        v.needs_fold = Some(vec![fold_item("budget_stop", "bs", false)]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert!(
            matches!(msg, ClientMsg::Command(Command::FocusPane(5))),
            "{msg:?}"
        );
        assert_eq!(v.answers, None);
    }

    // Invariant (x-feec): a squadless live row has no reachable pane, so Enter
    // degrades to a notice and sends nothing.
    #[tokio::test]
    async fn needs_enter_squadless_row_notices_no_send() {
        let mut v = view_with_agents(vec![]);
        v.needs_fold = Some(vec![fold_item("review_wedged", "alive", true)]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "squadless row sends nothing");
        assert_eq!(v.answers, None);
        assert!(v.notice.is_some(), "shows a focus-manually notice");
    }

    // Invariant (x-feec): a digit on a non-answerable fold row is a local BEL,
    // never a stray keystroke to a pane.
    #[tokio::test]
    async fn needs_digit_on_non_answerable_sends_nothing() {
        let mut v = view_with_agents(vec![agent_row("rw", 4, Some(AgentBadge::Working), false)]);
        v.needs_fold = Some(vec![fold_item("review_wedged", "rw", false)]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"1", &mut buf).await.unwrap();
        assert!(buf.is_empty());
    }

    // AC6-FR (x-feec): closing bumps the generation token so a fold result that
    // lands after the overlay closed is discarded by the recv guard.
    #[tokio::test]
    async fn needs_close_bumps_generation() {
        let mut v = view_with_agents(vec![blocked_row("a", 1, None)]);
        v.answers = Some(0);
        let g = v.needs_gen;
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"q", &mut buf).await.unwrap();
        assert_eq!(
            v.needs_gen,
            g + 1,
            "close bumps gen (in-flight fold discarded)"
        );
    }

    // codex P2 (x-feec): a fold row's cursor re-anchor survives a squadless ->
    // joined transition, because identity is the stable session id, not the
    // display name (which flips from session id to the roster row's name).
    #[test]
    fn needs_fold_row_id_is_stable_across_join_flip() {
        // Squadless first: no roster row, item is live -> name is the session id.
        let mut v = view_with_agents(vec![]);
        v.needs_fold = Some(vec![fold_item("budget_stop", "wkr", true)]);
        let squadless_id = v.needs_view().0[0].id();
        // Now the roster row appears: the item joins and its name flips to "wkr"
        // (already the same here, so use a distinct roster name to prove it).
        let mut joined_row = agent_row("wkr-pane", 5, Some(AgentBadge::Working), false);
        joined_row.cwd_base = Some("wkr".into()); // join by cwd_base, name differs
        v.layout.agents = vec![joined_row];
        let joined = &v.needs_view().0[0];
        assert_eq!(
            joined.name, "wkr-pane",
            "display name flips to the roster row"
        );
        assert_eq!(
            joined.id(),
            squadless_id,
            "but the re-anchor identity (session id) is unchanged"
        );
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

    // x-dcff AC (happy): Enter on a blocked row focuses that exact pane, not just
    // its squad; the overlay closes.
    #[tokio::test]
    async fn xdcff_answer_keys_enter_focuses_the_exact_pane() {
        let mut v = view_with_agents(vec![blocked_row("peer", 4, None)]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        match msg {
            ClientMsg::Command(Command::FocusPane(pane)) => assert_eq!(pane, 4),
            other => panic!("expected FocusPane(4), got {other:?}"),
        }
        assert_eq!(v.answers, None, "Enter closes the overlay");
    }

    // x-dcff AC (edge): a blocked row with no pane_id sends nothing on Enter and
    // still closes.
    #[tokio::test]
    async fn xdcff_answer_keys_enter_no_pane_id_sends_nothing() {
        let mut row = blocked_row("peer", 4, None);
        row.pane_id = None;
        let mut v = view_with_agents(vec![row]);
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "no pane_id -> nothing sent (AC1-EDGE)");
        assert_eq!(v.answers, None, "Enter still closes the overlay");
    }

    #[tokio::test]
    async fn rename_keys_enter_sends_the_typed_name_for_the_captured_tab() {
        // AC2-HP (client half): type + Enter -> one RenameTab for the tab id
        // captured at open time.
        let mut v = two_pane_view();
        v.open_rename(RenameTarget::Tab(7));
        let mut buf: Vec<u8> = Vec::new();
        rename_keys(&mut v, b"debug\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Command(Command::RenameTab {
                tab: 7,
                name: "debug".into()
            })
        );
        assert_eq!(v.rename, None, "submit closes the overlay");
    }

    #[tokio::test]
    async fn leader_reorder_sends_the_active_tab_id_and_delta() {
        let mut v = two_pane_view();
        let mut scanner = Scanner::default();
        let mut carry = Vec::new();
        let mut buf: Vec<u8> = Vec::new();

        handle_stdin(&mut v, &mut scanner, &mut carry, b"\x02>leak", &mut buf)
            .await
            .unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Command(Command::ReorderTab {
                squad: 1,
                tab: 1,
                delta: 1
            })
        );
        assert_eq!(
            cur.position() as usize,
            cur.get_ref().len(),
            "same-chunk bytes after the chord are swallowed"
        );
    }

    #[tokio::test]
    async fn rename_keys_empty_enter_still_sends_the_clear() {
        // Locked 2 / AC3-HP: Enter on an EMPTY buffer sends (blank = reset to
        // auto) - the one deliberate divergence from create_keys.
        let mut v = two_pane_view();
        v.open_rename(RenameTarget::Tab(7));
        let mut buf: Vec<u8> = Vec::new();
        rename_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Command(Command::RenameTab {
                tab: 7,
                name: String::new()
            })
        );
        assert_eq!(v.rename, None);
    }

    #[tokio::test]
    async fn rename_keys_esc_cancels_without_sending_and_swallows_the_tail() {
        // AC1-UI: Esc closes, sends nothing; same-chunk bytes after the Esc
        // die with the overlay instead of leaking into the pane.
        let mut v = two_pane_view();
        v.open_rename(RenameTarget::Tab(7));
        let mut buf: Vec<u8> = Vec::new();
        rename_keys(&mut v, b"deb\x1bx", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "cancel sends no command");
        assert_eq!(v.rename, None);
    }

    #[tokio::test]
    async fn rename_keys_caps_the_buffer_at_max_tab_name() {
        // The TUI affordance half of AC2-ERR: the operator sees exactly what
        // the server will store (the server cap stays authoritative).
        let mut v = two_pane_view();
        v.open_rename(RenameTarget::Tab(7));
        let mut buf: Vec<u8> = Vec::new();
        let long = "a".repeat(MAX_TAB_NAME + 8);
        rename_keys(&mut v, long.as_bytes(), &mut buf)
            .await
            .unwrap();
        assert_eq!(v.rename.as_ref().unwrap().1.len(), MAX_TAB_NAME);
    }

    // ---- x-96e8: squad-management selector context keys ----

    #[tokio::test]
    async fn selector_r_opens_squad_rename_overlay() {
        // AC1-HP (client half): `r` on a squad row opens the rename overlay for
        // that squad, closing the selector, without sending anything.
        let mut v = two_pane_view(); // rows: [squad1, squad2, +footer]
        v.selector = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"r", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "opening the overlay sends nothing");
        assert_eq!(v.selector, None, "the selector closes");
        assert_eq!(v.rename.map(|(t, _)| t), Some(RenameTarget::Squad(1)));
    }

    #[tokio::test]
    async fn rename_keys_squad_target_sends_rename_squad() {
        // AC1-HP: Enter on a squad rename sends RenameSquad for the captured id.
        let mut v = two_pane_view();
        v.open_rename(RenameTarget::Squad(2));
        let mut buf: Vec<u8> = Vec::new();
        rename_keys(&mut v, b"oss\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Command(Command::RenameSquad {
                squad: 2,
                name: "oss".into()
            })
        );
    }

    #[tokio::test]
    async fn selector_j_sends_move_squad_and_tracks_the_squad() {
        // AC3-HP (client half): `J` reorders the squad down and arms sel_follow
        // so the next Layout re-points the cursor at the moved workspace; the
        // selector stays open for repeated presses.
        let mut v = two_pane_view();
        v.selector = Some(0); // squad 1
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"J", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Command(Command::MoveSquad { squad: 1, delta: 1 })
        );
        assert_eq!(v.sel_follow, Some(1), "the cursor tracks the moved squad");
        assert_eq!(v.selector, Some(0), "the selector stays open");
    }

    #[test]
    fn set_layout_follows_the_reordered_squad() {
        // AC3-HP: after a J/K reorder, the next Layout re-points the cursor onto
        // the moved squad's new row rather than clamping the old index.
        let mut v = two_pane_view(); // rows: [squad1@0, squad2@1, footer]
        v.selector = Some(0);
        v.sel_follow = Some(1); // tracking squad 1
                                // The reorder landed: squad 1 is now second, so its row moves to index 1.
        v.set_layout(LayoutView {
            squads: vec![meta(2, "notes", 1, 0), meta(1, "footnote", 2, 1)],
            active_squad: 1,
            panes: vec![],
            focus: 11,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
        });
        assert_eq!(v.selector, Some(1), "cursor follows squad 1 to its new row");
    }

    #[tokio::test]
    async fn selector_x_arms_remove_confirm_and_degrades_on_short_terminal() {
        // AC2-UI: `x` on a squad row arms the remove confirm carrying the blast
        // radius; a too-short terminal refuses instead of arming an invisible
        // confirm (x-260a row_action rule).
        let mut v = two_pane_view(); // squad1 (footnote) has 2 panes; 2 squads
        v.selector = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert!(buf.is_empty());
        assert_eq!(v.selector, None);
        match v.confirm.as_ref().map(|c| (&c.action, c.label.as_str())) {
            Some((ConfirmKind::RemoveSquad { squad, panes, last }, label)) => {
                assert_eq!((*squad, *panes, *last), (1, 2, false));
                assert_eq!(label, "footnote");
            }
            _ => panic!("expected a RemoveSquad confirm"),
        }

        // Too short: refuse with a notice, arm nothing.
        let mut v = two_pane_view();
        v.term.0 = MIN_ROWS_FOR_STATUS - 1;
        v.selector = Some(0);
        selector_keys(&mut v, b"x", &mut Vec::new()).await.unwrap();
        assert!(
            v.confirm.is_none(),
            "no invisible confirm on a short terminal"
        );
        assert!(v.notice.is_some(), "the refusal is surfaced");
    }

    #[tokio::test]
    async fn confirm_keys_enter_sends_remove_squad() {
        // AC2-HP: Enter on an armed remove confirm sends RemoveSquad.
        let mut v = two_pane_view();
        v.confirm = Some(ConfirmAction {
            action: ConfirmKind::RemoveSquad {
                squad: 2,
                panes: 1,
                last: false,
            },
            label: "notes".into(),
        });
        let mut buf: Vec<u8> = Vec::new();
        confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(decoded, ClientMsg::Command(Command::RemoveSquad(2)));
    }

    #[tokio::test]
    async fn selector_m_opens_move_picker_on_a_squad_row() {
        // x-0090: tab rows left the sideline, so `m` on a SQUAD row opens the
        // picker over the OTHER squads, targeting that squad's ACTIVE tab (the
        // one shown in the tab bar). A squad itself still moves with J/K, not m.
        let mut v = two_pane_view(); // rows: [squad1@0, squad2@1, footer@2]
        v.selector = Some(0); // squad 1 (fixture active_tab 1 -> tab id 1)
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"m", &mut buf).await.unwrap();
        assert!(buf.is_empty(), "opening the picker sends nothing");
        assert_eq!(
            v.move_pick,
            Some((1, vec![2])),
            "picker captures the squad's active tab id and the non-source squads"
        );

        // With only one squad there is nowhere to move to: `m` BELs, no picker.
        let mut v = two_pane_view();
        v.set_layout(LayoutView {
            squads: vec![meta(1, "footnote", 2, 1)],
            active_squad: 1,
            panes: vec![],
            focus: 0,
            area: (29, 72),
            agents: vec![],
            focus_node: None,
            backlog: Vec::new(),
        });
        v.selector = Some(0);
        selector_keys(&mut v, b"m", &mut Vec::new()).await.unwrap();
        assert!(v.move_pick.is_none(), "no destination squad -> no picker");
    }

    #[tokio::test]
    async fn move_pick_keys_digit_sends_move_tab_and_stale_id_bels() {
        // A digit sends MoveTab for the numbered squad; a captured id that
        // vanished is refused locally (no wire message).
        let mut v = two_pane_view();
        v.move_pick = Some((7, vec![2])); // move tab 7 to squad 2 (digit 1)
        let mut buf: Vec<u8> = Vec::new();
        move_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Command(Command::MoveTab { tab: 7, squad: 2 })
        );
        assert_eq!(v.move_pick, None, "the picker is single-shot");

        // A stale captured id (not in the current catalog) sends nothing.
        let mut v = two_pane_view();
        v.move_pick = Some((7, vec![999]));
        let mut buf: Vec<u8> = Vec::new();
        move_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
        assert!(
            buf.is_empty(),
            "a stale destination id never reaches the wire"
        );
        assert_eq!(v.move_pick, None);
    }

    #[tokio::test]
    async fn selector_non_reorder_key_clears_sel_follow() {
        // sel_follow only survives across J/K; any other key drops it so a later
        // Layout re-anchors normally.
        let mut v = two_pane_view();
        v.selector = Some(0);
        v.sel_follow = Some(1);
        selector_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
        assert_eq!(v.sel_follow, None);
    }
}
