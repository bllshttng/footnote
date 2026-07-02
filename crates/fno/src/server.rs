//! The mux server: owns N PTY-backed panes organized as squads -> tabs ->
//! pane trees, streams pane-tagged self-contained frames to attached clients
//! over the session socket, and executes layout commands on one core loop.
//!
//! Concurrency shape (the epic's locked channel discipline):
//! - client -> server input/control rides bounded mpsc channels that are
//!   AWAITED - never dropped. Backpressure flows to the socket, then the
//!   client.
//! - server -> client has TWO outbound paths per client. `Layout`/`ModeSync`/
//!   `Notice`/`Bye` ride a bounded RELIABLE mpsc the writer task always
//!   drains first; render `Frame`s ride a droppable per-(client, pane)
//!   newest-wins dirty map + `Notify`. A flooded pane coalesces to its newest
//!   frame per client without starving siblings or the reliable stream.
//! - PTY masters are blocking, so reads live on dedicated threads (`pty.rs`)
//!   feeding ONE shared pane-tagged channel into the core loop; tokio stays
//!   at the edges. Layout mutations happen exclusively ON the core loop, so
//!   a split racing a child exit is serialized, never interleaved.
//!
//! The server is the single source of truth for every grid. It outlives every
//! client: attach/detach/kill -9 of a client never touches a PTY. The session
//! ends when the last pane of the last tab of the last squad closes (Locked
//! Decision 8) - THAT sends `Bye` and exits, superseding Phase 1's rendered
//! "exited" state.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use crate::agents_view::{self, RegistryAgent};
use crate::proto::{
    bind_or_probe, check_attach_version, err_code, read_msg, write_msg, AgentRow, BindOutcome,
    BlockSel, ClientMsg, Command, ControlVerb, Frame, MouseButton, MouseEvent, MouseKind, PaneInfo,
    ServerMsg, SquadMeta, TabMeta, WaitOutcome,
};
use crate::pty::{shell_candidates, PtyShell};
use crate::squad::{self, RemoveOutcome, Resolver, Session};
use crate::tree::{self, Axis, Node, Rect, Tab, TabId};
use crate::vt::{self, frame_text, Modes};

/// A control connection's reply channel: exactly one [`ServerMsg`], then close.
type ControlReply = oneshot::Sender<ServerMsg>;

/// A silent connection (e.g. a liveness probe) gets this long to Attach
/// before the server closes it.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// Reliable per-client channel depth. The writer task drains this fast; a
/// client hundreds of Layouts behind is not consuming its socket.
///
/// ponytail: the locked discipline says reliable sends are awaited, but
/// awaiting a wedged client's channel ON the core loop would freeze every
/// pane and peer (the grid rail's drive-freeze class). `try_send` + treating
/// Full as a dead client preserves "never dropped for a live client" without
/// head-of-line blocking the world.
const RELIABLE_CAP: usize = 256;

/// How long the core loop lingers after queueing `Bye`s so writer tasks can
/// flush them before process exit. A lost Bye degrades gracefully (the client
/// reports "session ended (server closed)"), so this is politeness, not
/// correctness.
const BYE_FLUSH: Duration = Duration::from_millis(250);

/// The droppable outbound path: newest unsent frame per pane, per client.
type DirtyMap = Arc<Mutex<HashMap<u64, Frame>>>;

/// Lines a single wheel notch scrolls a mux-interpreted pane (brief Claude's
/// discretion #4). Three matches the common terminal default; a mouse-mode app
/// gets the raw wheel event and picks its own step.
const MOUSE_WHEEL_LINES: i32 = 3;

/// What the server does with a mouse event, decided purely from the pane's
/// modes + the gesture so the routing (the brief's crux, Locked 2) is unit
/// testable without a PTY. The `mouse` method executes the chosen action.
#[derive(Debug, PartialEq, Eq)]
enum MouseAction {
    /// The app owns its mouse: SGR-encode and write to its PTY (AC3-HP).
    Passthrough,
    /// Mux-interpret a wheel notch: scroll the pane by N lines (US1).
    Scroll(i32),
    /// Left press begins a selection anchor (US2).
    SelectStart,
    /// Left drag extends the selection (US2).
    SelectUpdate,
    /// Left release finalizes: auto-copy a real selection, else clear (US2).
    SelectRelease,
    /// No mux meaning (middle/right buttons in v1).
    Ignore,
}

/// Route by the pane's mode (brief Locked 2): a pane that negotiated SGR mouse
/// reporting gets passthrough; otherwise the mux interprets the gesture. Only
/// SGR is honored for passthrough - a mouse app that never negotiated SGR falls
/// through to interpretation rather than receiving garbage (Domain: legacy X10
/// truncates at column 223).
fn route_mouse(modes: Modes, kind: MouseKind) -> MouseAction {
    let reports_mouse = modes.mouse_click || modes.mouse_drag || modes.mouse_motion;
    if reports_mouse && modes.sgr_mouse {
        return MouseAction::Passthrough;
    }
    match kind {
        MouseKind::WheelUp => MouseAction::Scroll(MOUSE_WHEEL_LINES),
        MouseKind::WheelDown => MouseAction::Scroll(-MOUSE_WHEEL_LINES),
        MouseKind::Press(MouseButton::Left) => MouseAction::SelectStart,
        MouseKind::Drag(MouseButton::Left) => MouseAction::SelectUpdate,
        MouseKind::Release(MouseButton::Left) => MouseAction::SelectRelease,
        _ => MouseAction::Ignore,
    }
}

/// SGR-encode one mouse event for an app that negotiated mouse reporting
/// (`ESC [ < b ; x ; y {M|m}`, brief Locked 12 / Domain: SGR 1006 only). `b` is
/// the button code plus the drag-motion bit; coordinates are 1-based. Press and
/// motion terminate with `M`, release with `m`.
fn sgr_mouse_bytes(event: &MouseEvent) -> Vec<u8> {
    let (button, released) = match event.kind {
        MouseKind::Press(b) => (button_code(b), false),
        MouseKind::Release(b) => (button_code(b), true),
        // Motion bit (32) rides on top of the held button (SGR drag report).
        MouseKind::Drag(b) => (button_code(b) + 32, false),
        MouseKind::WheelUp => (64, false),
        MouseKind::WheelDown => (65, false),
    };
    let x = event.col as u32 + 1;
    let y = event.row as u32 + 1;
    let terminator = if released { 'm' } else { 'M' };
    format!("\x1b[<{button};{x};{y}{terminator}").into_bytes()
}

fn button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// What connected clients register with the core loop.
enum CoreMsg {
    Attach {
        id: u64,
        rows: u16,
        cols: u16,
        /// The client's literal launch directory - where a FRESH squad's
        /// first shell starts (more precise than the canonical root when the
        /// user launched from a subdirectory).
        cwd: String,
        /// Already resolved to the canonical squad key by `handle_client`'s
        /// own task - the blocking git run never touches the core loop.
        squad_key: String,
        reliable_tx: mpsc::Sender<ServerMsg>,
        dirty: DirtyMap,
        notify: Arc<Notify>,
    },
    /// Raw bytes for the SENDER's viewed tab's focused pane (per-client
    /// views, Phase 3 Locked 3/4).
    Input {
        id: u64,
        bytes: Vec<u8>,
    },
    Resize {
        id: u64,
        rows: u16,
        cols: u16,
    },
    Command {
        id: u64,
        cmd: Command,
    },
    /// (v7) A mouse event from a client's pane rect, routed by the pane's mode
    /// (brief Locked 2): an app in mouse mode gets an SGR-encoded event on its
    /// PTY; otherwise the mux interprets it (wheel -> scroll, press -> focus +
    /// selection anchor, drag -> selection update, release -> finalize + copy).
    Mouse {
        id: u64,
        pane: u64,
        event: MouseEvent,
    },
    Gone(u64),
    /// A pre-Attach `Query` (mux ls): reply with the whole `Info` message.
    Query(tokio::sync::oneshot::Sender<ServerMsg>),
    /// A pre-Attach `KillServer`: Bye every client, kill every pane child,
    /// exit 0 (Locked 12's second and last exit path).
    Kill,
    // -- v4 control verbs (one-shot: reply on the oneshot, then the
    // connection task closes). Snapshot reads and the spawn/kill mutations
    // reply inline on the core loop; `PaneWait` hands its reply to an
    // off-loop watcher so nothing blocking ever lands on the loop.
    PaneLs(ControlReply),
    PaneRead {
        pane: u64,
        lines: Option<u16>,
        /// (v6) Select an OSC 133 command block instead of a plain read.
        block: Option<BlockSel>,
        reply: ControlReply,
    },
    /// `squad_key` was resolved OFF the core loop (like `Attach`); `cwd` is the
    /// literal launch dir for the child shell. `claim: true` marks the pane
    /// writer-claim eligible (an agent pane - 4a-G2).
    PaneRun {
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        cols: Option<u16>,
        rows: Option<u16>,
        claim: bool,
        reply: ControlReply,
    },
    PaneSend {
        pane: u64,
        bytes: Vec<u8>,
        reply: ControlReply,
    },
    PaneWait {
        pane: u64,
        quiet_ms: Option<u64>,
        /// Pre-compiled OFF the core loop (in `handle_control`): `Regex::new`
        /// is bounded CPU the single-threaded loop must never run.
        regex: Option<regex::Regex>,
        timeout_ms: u64,
        /// (v6) Also resolve on the next OSC 133 `D` -> `CommandDone`.
        command_done: bool,
        reply: ControlReply,
    },
    PaneKill {
        pane: u64,
        reply: ControlReply,
    },
    /// Acquire/release the per-pane writer claim (4a-G3, brief Locked 5).
    PaneClaim {
        pane: u64,
        holder_pid: u32,
        reply: ControlReply,
    },
    PaneRelease {
        pane: u64,
        reply: ControlReply,
    },
    /// A fresh registry-derived agent row set from the off-loop reader task
    /// (4a-G2). Sent only when the set changed; the core stores it and
    /// re-pushes layouts (rects unchanged, so no frame re-emit).
    AgentRows(Vec<RegistryAgent>),
}

/// The per-pane signal an off-loop `PaneWait` watcher observes. The core loop
/// refreshes `text` on every output burst and flips `exited` when the pane
/// closes; a dropped sender (pane reaped) reads as exited too. `text` is the
/// visible grid so a pattern watcher needs no round-trip back to the loop
/// (only refreshed while a watcher is subscribed - see
/// [`Core::note_pane_output`]). `watch`'s own change signal is the wakeup, so
/// no sequence counter is needed - `send_modify` always notifies.
#[derive(Clone)]
struct WaitTick {
    exited: bool,
    text: Arc<str>,
    /// (v6) The most recently completed OSC 133 block's `(seq, exit)`, or `None`.
    /// A `command_done` watcher resolves when this advances past its baseline.
    last_done: Option<(u64, Option<i32>)>,
}

impl Default for WaitTick {
    fn default() -> Self {
        WaitTick {
            exited: false,
            text: Arc::from(""),
            last_done: None,
        }
    }
}

struct Client {
    id: u64,
    reliable_tx: mpsc::Sender<ServerMsg>,
    dirty: DirtyMap,
    notify: Arc<Notify>,
    /// The mode state this client's terminal was last synced to. Fresh
    /// clients start at `Modes::default()` (a raw terminal), so the first
    /// sync diff IS the attach replay.
    synced_modes: Modes,
    /// This client's own (squad, tab) view (Locked 3). View commands mutate
    /// only the sender's copy; tree commands resolve against it. Always
    /// names a live tab - any mutation that kills a viewed tab re-anchors
    /// the view in the same core-loop mutation (Invariants).
    view: (u64, TabId),
    /// Panes of the viewed tab: this client's frame-emission gate, rebuilt
    /// on every layout push. Grids of unviewed panes are still fed; their
    /// frames never cross this client's wire (AC2-FR).
    visible: HashSet<u64>,
    /// This client's own content-area (rows, cols) - one input to the
    /// view-scoped smallest-client clamp (Locked 1).
    dims: (u16, u16),
}

struct PaneEntry {
    pty: PtyShell,
    vt: vt::Pane,
}

/// Whether an event ended the session.
#[derive(PartialEq)]
enum Flow {
    Continue,
    Shutdown,
}

/// Unlink the socket on every exit path out of `run` (a SIGKILL leaves it
/// behind by design; the stale-socket path in `bind_or_probe` covers that).
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run the server on `socket`. Returns the process exit code.
///
/// The session NAME is the socket's file stem (`work.sock` -> `work`): every
/// creation path routes through `proto::socket_path`, so deriving it here
/// needs no extra flag on the internal `--server` surface. It feeds the
/// `Info` answer and every pane's `FNO_SESSION`.
pub fn run(socket: PathBuf) -> i32 {
    if let Some(parent) = socket.parent() {
        // The socket accepts keystrokes into your shell: never group/world.
        // Born-0700 (atomic) rather than create-then-tighten (gemini
        // security-medium).
        if let Err(e) = crate::proto::ensure_private_dir(parent) {
            eprintln!("fno mux: cannot create {}: {e}", parent.display());
            return 1;
        }
    }
    let listener = match bind_or_probe(&socket) {
        Ok(BindOutcome::Bound(l)) => l,
        Ok(BindOutcome::AlreadyRunning) => {
            // Idempotent explicit start: a live server for this session IS
            // the requested end state.
            eprintln!(
                "fno mux: a server is already running at {}",
                socket.display()
            );
            return 0;
        }
        Err(e) => {
            eprintln!("fno mux: cannot bind {}: {e}", socket.display());
            return 1;
        }
    };
    let _guard = SocketGuard(socket.clone());

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno mux: cannot start runtime: {e}");
            return 1;
        }
    };
    let session_name = socket
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::proto::DEFAULT_SESSION.to_string());
    runtime.block_on(serve(listener, &socket, session_name))
}

// ---------------------------------------------------------------------------
// Core state + mutations (all on the core loop)
// ---------------------------------------------------------------------------

struct Core {
    session: Session,
    panes: HashMap<u64, PaneEntry>,
    /// Per-pane output signal for off-loop `PaneWait` watchers. One entry per
    /// live pane, created with the pane, dropped (flipped `exited`) when it is
    /// reaped. Kept in lockstep with `panes` via [`Core::register_pane`] /
    /// [`Core::reap_pane`].
    pane_watch: HashMap<u64, watch::Sender<WaitTick>>,
    clients: Vec<Client>,
    /// Monotonic, never reused (Locked Decision 6).
    next_pane_id: u64,
    next_squad_id: u64,
    /// Each tab's last-applied content area (Locked 1's "no viewers -> keep
    /// last size"). Written by the geometry pass for every viewed tab; read
    /// as the fallback when a tab loses its last viewer. Purged when a tab
    /// dies (ids are never reused, so stale entries would only accumulate).
    tab_areas: HashMap<TabId, (u16, u16)>,
    /// This server's session name (the socket's file stem): the `Info`
    /// answer, and `FNO_SESSION` in every pane it spawns.
    session_name: String,
    shells: Vec<OsString>,
    out_tx: mpsc::Sender<(u64, Vec<u8>)>,
    exit_tx: mpsc::Sender<u64>,
    /// Latest registry-derived agent rows (4a-G2), stored raw; the pane-exit
    /// fact and squad assignment are joined at layout time, where the live
    /// pane set and the squad catalog live.
    agents: Vec<RegistryAgent>,
    /// Panes spawned claim-ELIGIBLE (`pane run --claim`, agent panes). A
    /// general pane never appears here and never consults a claim (Locked 5).
    claim_eligible: HashSet<u64>,
    /// Held writer claims: pane -> holder pid. Enforced on `Input` as an
    /// in-memory lookup + a `kill(pid, 0)` liveness probe (one syscall, never
    /// a subprocess - the origin freeze class); a dead holder releases lazily
    /// on the next contested keystroke, so typing resumes without a server
    /// restart (AC3-FR) and no sweep timer exists to tune.
    claims: HashMap<u64, u32>,
}

/// True while `pid` is alive (or unprobeable - erring toward "held" keeps a
/// live holder's claim from being stolen by a permissions error; ESRCH is the
/// definitive "gone").
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) performs no signal delivery, only validation.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

impl Core {
    /// The view-scoped smallest-client clamp (Locked 1): a tab's content
    /// area is the elementwise min over the dims of every client currently
    /// viewing it; with no viewers it keeps its last-applied size, and a tab
    /// that has never been sized falls back to the VT defaults.
    fn tab_area(&self, tid: TabId) -> (u16, u16) {
        let clamp = self
            .clients
            .iter()
            .filter(|c| c.view.1 == tid)
            .map(|c| c.dims)
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)));
        clamp
            .or_else(|| self.tab_areas.get(&tid).copied())
            .unwrap_or((crate::vt::DEFAULT_ROWS, crate::vt::DEFAULT_COLS))
    }

    fn tab_rect(&self, tid: TabId) -> Rect {
        let (rows, cols) = self.tab_area(tid);
        Rect {
            x: 0,
            y: 0,
            rows,
            cols,
        }
    }

    /// Spawn a pane's shell in `cwd` (codex P2: a long-lived server serves
    /// squads from MANY repos; inheriting the server process cwd would start
    /// every later squad's shell in the first client's directory). Empty /
    /// vanished dirs degrade to the server cwd inside `PtyShell::spawn`.
    fn spawn_pane(&mut self, rows: u16, cols: u16, cwd: &str) -> Result<u64, String> {
        let id = self.next_pane_id;
        let dir = Some(std::path::Path::new(cwd)).filter(|_| !cwd.is_empty());
        let pty = PtyShell::spawn(
            &self.shells,
            rows,
            cols,
            dir,
            &self.session_name,
            id,
            self.out_tx.clone(),
            self.exit_tx.clone(),
        )
        .map_err(|e| e.to_string())?;
        self.register_pane(id, pty, rows, cols);
        Ok(id)
    }

    /// Spawn an explicit `argv` as a pane (the `pane run` / agents-spawn path)
    /// - no shell candidate fallback: an unspawnable argv is the caller's
    /// error, surfaced verbatim. Same atomic ordering as [`Core::spawn_pane`]
    /// (PTY first, model second), so a spawn failure mutates nothing.
    fn spawn_pane_cmd(
        &mut self,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
    ) -> Result<u64, String> {
        if argv.is_empty() {
            return Err("pane run needs a command (empty argv)".into());
        }
        let id = self.next_pane_id;
        let dir = Some(std::path::Path::new(cwd)).filter(|_| !cwd.is_empty());
        let pty = PtyShell::spawn_cmd(
            argv,
            rows,
            cols,
            dir,
            &self.session_name,
            id,
            self.out_tx.clone(),
            self.exit_tx.clone(),
        )
        .map_err(|e| e.to_string())?;
        self.register_pane(id, pty, rows, cols);
        Ok(id)
    }

    /// Record a freshly-spawned pane: bump the id, insert its VT grid, and
    /// arm its output watch (dropped receiver, so the watch costs nothing
    /// until a `PaneWait` subscribes).
    fn register_pane(&mut self, id: u64, pty: PtyShell, rows: u16, cols: u16) {
        self.next_pane_id += 1;
        self.panes.insert(
            id,
            PaneEntry {
                pty,
                vt: vt::Pane::new(rows, cols),
            },
        );
        let (tx, _rx) = watch::channel(WaitTick::default());
        self.pane_watch.insert(id, tx);
    }

    /// Kill+reap a pane's PTY and retire its watch (flipping `exited` so any
    /// subscribed `PaneWait` returns `PaneExited`). The single place panes
    /// leave `panes`/`pane_watch`, so the two maps never drift. Idempotent.
    fn reap_pane(&mut self, pid: u64) {
        if let Some(entry) = self.panes.remove(&pid) {
            entry.pty.kill();
        }
        // Pane exit releases the writer claim UNCONDITIONALLY (Locked 5): a
        // held claim never blocks the close cascade.
        self.claims.remove(&pid);
        self.claim_eligible.remove(&pid);
        if let Some(tx) = self.pane_watch.remove(&pid) {
            // Last observable tick before the sender drops: a watcher that
            // reads it sees `exited`; one blocked in `changed()` sees the
            // sender-dropped error and treats it identically.
            tx.send_modify(|t| t.exited = true);
        }
    }

    /// Refresh a pane's output watch after a burst, but only while a
    /// `PaneWait` is actually subscribed - `frame_text` is O(grid), so an
    /// unwatched pane pays nothing (the common case).
    fn note_pane_output(&self, pid: u64) {
        let Some(tx) = self.pane_watch.get(&pid) else {
            return;
        };
        if tx.receiver_count() == 0 {
            return;
        }
        let Some(entry) = self.panes.get(&pid) else {
            return;
        };
        let text: Arc<str> = Arc::from(frame_text(&entry.vt.frame()));
        let last_done = entry.vt.last_done();
        // `send_modify` always notifies watchers, so refreshing the text IS
        // the wakeup - no counter needed. `last_done` rides along so a
        // `command_done` watcher sees a finished command in the same tick.
        tx.send_modify(|t| {
            t.text = text;
            t.last_done = last_done;
        });
    }

    /// Every pane's metadata for `pane ls`, ordered by pane id so the listing
    /// is stable and machine-readable. A pane mid-teardown (not in the tree)
    /// is still listed with what is known rather than dropped silently.
    fn pane_infos(&self) -> Vec<PaneInfo> {
        let mut out: Vec<PaneInfo> = self
            .panes
            .iter()
            .map(|(&pid, entry)| {
                let (squad_id, tab_id, cwd) = match self.session.find_pane(pid) {
                    Some((sid, ti)) => {
                        let sq = self.session.squad(sid).expect("find_pane live squad");
                        (sid, sq.tabs[ti].id, sq.canonical_cwd.clone())
                    }
                    None => (0, 0, String::new()),
                };
                PaneInfo {
                    pane_id: pid,
                    squad_id,
                    tab_id,
                    cwd,
                    child_pid: entry.pty.child_pid(),
                    title: None,
                }
            })
            .collect();
        out.sort_by_key(|p| p.pane_id);
        out
    }

    /// `pane run`: spawn `argv` as a new tab-pane in the squad `squad_key`
    /// names (created if absent). PTY-first ordering (Locked 7): a spawn
    /// failure mutates no model. Each call is its own pane, so three runs into
    /// one cwd land three panes in one squad (no mux-layer dedup - that lives
    /// in the spawn front half).
    fn run_pane(
        &mut self,
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        claim: bool,
    ) -> Result<u64, String> {
        let pid = self.spawn_pane_cmd(&argv, rows, cols, &cwd)?;
        if claim {
            // Writer-claim ELIGIBILITY, set only at agent spawn (Locked 5).
            // The claim itself is acquired per-burst via PaneClaim.
            self.claim_eligible.insert(pid);
        }
        let tid = self.session.mint_tab_id();
        let tab = Tab {
            id: tid,
            root: Node::Leaf(pid),
            focus: pid,
        };
        match self.session.find_by_cwd(&squad_key) {
            Some(sid) => self
                .session
                .squad_mut(sid)
                .expect("find_by_cwd hit")
                .tabs
                .push(tab),
            None => {
                let sid = self.next_squad_id;
                self.next_squad_id += 1;
                self.session.add_squad(sid, squad_key, tab);
            }
        }
        // Keep any attached client's view consistent; a script-only session
        // has no clients, so this is then a cheap no-op.
        self.push_layout(true);
        Ok(pid)
    }

    /// A one-line refusal/notice to ONE client (BEL + transient message on
    /// its side). Errors write to the session log, never a client terminal
    /// (the compositor owns it).
    fn notice(&self, client_id: u64, text: impl Into<String>) {
        if let Some(c) = self.clients.iter().find(|c| c.id == client_id) {
            let _ = c
                .reliable_tx
                .try_send(ServerMsg::Notice { text: text.into() });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn attach(
        &mut self,
        id: u64,
        rows: u16,
        cols: u16,
        cwd: String,
        key: String,
        reliable_tx: mpsc::Sender<ServerMsg>,
        dirty: DirtyMap,
        notify: Arc<Notify>,
    ) {
        let view = match self.session.find_by_cwd(&key) {
            Some(sid) => {
                // Existing squad: the attach lands IN it (AC6-HP, worktree
                // rollup). The fresh client's VIEW anchors to the squad's
                // most-recently-active tab; nothing global moves (the Phase 2
                // last-wins active-squad flip died with per-client views).
                let squad = self.session.squad(sid).expect("find_by_cwd hit");
                let tid = squad
                    .tabs
                    .get(squad.active_tab)
                    .or_else(|| squad.tabs.first())
                    .expect("a squad always has a tab")
                    .id;
                (sid, tid)
            }
            None => {
                // Fresh squad: PTY spawn FIRST (Locked 7), then the model.
                // The first shell starts in the client's literal launch dir.
                match self.spawn_pane(rows, cols, &cwd) {
                    Ok(pid) => {
                        let sid = self.next_squad_id;
                        self.next_squad_id += 1;
                        let tid = self.session.mint_tab_id();
                        self.session.add_squad(
                            sid,
                            key,
                            Tab {
                                id: tid,
                                root: Node::Leaf(pid),
                                focus: pid,
                            },
                        );
                        (sid, tid)
                    }
                    Err(e) => {
                        // AC1-ERR terminal case at attach: nothing spawnable.
                        // Refuse THIS attach; the server keeps serving (an
                        // existing squad's clients are unaffected).
                        let _ = reliable_tx.try_send(ServerMsg::Bye {
                            reason: format!("cannot start a shell: {e}"),
                        });
                        return;
                    }
                }
            }
        };
        self.clients.push(Client {
            id,
            reliable_tx,
            dirty,
            notify,
            synced_modes: Modes::default(),
            view,
            visible: HashSet::new(),
            dims: (rows, cols),
        });
        self.push_layout(true);
    }

    /// The sender's current view, when it is still registered.
    fn client_view(&self, client_id: u64) -> Option<(u64, TabId)> {
        self.clients
            .iter()
            .find(|c| c.id == client_id)
            .map(|c| c.view)
    }

    /// The tab a view names, when it is live.
    fn viewed_tab(&self, view: (u64, TabId)) -> Option<&Tab> {
        self.session
            .squad(view.0)?
            .tabs
            .iter()
            .find(|t| t.id == view.1)
    }

    fn viewed_tab_mut(&mut self, view: (u64, TabId)) -> Option<&mut Tab> {
        self.session
            .squad_mut(view.0)?
            .tabs
            .iter_mut()
            .find(|t| t.id == view.1)
    }

    /// Re-anchor every client whose view no longer names a live (squad, tab).
    /// Runs inside the same core-loop mutation that killed the tab/squad
    /// (Invariants), BEFORE layouts push, so a push only ever sees live
    /// views. Preference order (a documented implementation choice, brief
    /// Discretion 6): the same squad's re-clamped most-recently-active tab
    /// (`remove_tab` already moved it to the nearest lower index); a dead
    /// squad falls back to the session's active-squad survivor, else the
    /// first squad.
    fn reanchor_views(&mut self) {
        let fallback: Option<(u64, TabId)> = {
            let s = self
                .session
                .active_squad
                .and_then(|id| self.session.squad(id))
                .or_else(|| self.session.squads.first());
            s.and_then(|s| {
                s.tabs
                    .get(s.active_tab)
                    .or_else(|| s.tabs.first())
                    .map(|t| (s.id, t.id))
            })
        };
        for i in 0..self.clients.len() {
            let view = self.clients[i].view;
            if self.viewed_tab(view).is_some() {
                continue;
            }
            let new_view = match self.session.squad(view.0) {
                // Same squad survives: its re-clamped MRU tab.
                Some(s) => s
                    .tabs
                    .get(s.active_tab)
                    .or_else(|| s.tabs.first())
                    .map(|t| (s.id, t.id)),
                None => fallback,
            };
            if let Some(v) = new_view {
                self.clients[i].view = v;
            }
            // No fallback = session empty; the caller is already shutting
            // down (Locked 12), the dangling view is never read again.
        }
    }

    /// The layout-change protocol (Locked 4), per client: resize PTYs/grids
    /// of every VIEWED tab to its rects, then for each client against ITS
    /// view: ModeSync (if its viewed tab's focused pane's modes differ from
    /// what that client's terminal last saw), flush stale frame slots, send
    /// its `Layout`, and (when `reemit`) queue full frames for its visible
    /// panes. Focus-only changes pass `reemit: false` - rects are unchanged,
    /// so queued frames stay valid. Unviewed tabs are untouched: grids keep
    /// feeding, geometry keeps its last size, nothing crosses the wire.
    fn push_layout(&mut self, reemit: bool) {
        // Geometry pass: each distinct viewed tab, once, at its view-scoped
        // smallest-client clamp (Locked 1/5). The applied area is cached so
        // the tab keeps it when its last viewer leaves.
        let viewed: HashSet<TabId> = self.clients.iter().map(|c| c.view.1).collect();
        let mut tab_rects: HashMap<TabId, (Vec<(u64, Rect)>, u64, (u16, u16))> = HashMap::new();
        for tid in viewed {
            let Some((sid, idx)) = self.session.find_tab(tid) else {
                // Unreachable while every tab-killing path re-anchors first;
                // if a future mutation forgets, the symptom is a blank
                // client - make it diagnosable from the session log.
                eprintln!("fno mux: dangling view on tab {tid}; re-anchor missed it");
                continue;
            };
            let area = self.tab_area(tid);
            self.tab_areas.insert(tid, area);
            let tab = &self.session.squad(sid).expect("find_tab hit").tabs[idx];
            let rects = tree::layout(
                &tab.root,
                Rect {
                    x: 0,
                    y: 0,
                    rows: area.0,
                    cols: area.1,
                },
            );
            let focus = tab.focus;
            // Rect-driven pane sizing: only geometry that actually changed
            // hits the PTY, so a resize storm's no-op tail is free (AC1-FR's
            // bounded-update half; the storm's head coalesces at the channel).
            for (pid, r) in &rects {
                if let Some(entry) = self.panes.get_mut(pid) {
                    if entry.vt.size() != (r.rows, r.cols) {
                        if let Err(e) = entry.pty.resize(r.rows, r.cols, 0, 0) {
                            // Grid and kernel winsize would disagree: log it.
                            eprintln!("fno mux: pty resize failed: {e}");
                        }
                        entry.vt.resize(r.rows, r.cols);
                    }
                }
            }
            tab_rects.insert(tid, (rects, focus, area));
        }

        // Per-client messages, precomputed so the send loop can borrow
        // clients mutably. A dangling view yields an empty layout, never a
        // panic (re-anchor upstream is the real guarantee).
        let per: Vec<(ServerMsg, Modes, Vec<(u64, Rect)>)> = self
            .clients
            .iter()
            .map(|c| {
                let (rects, focus, area) = tab_rects
                    .get(&c.view.1)
                    .cloned()
                    .unwrap_or_else(|| (Vec::new(), 0, c.dims));
                let msg = self.layout_msg_for(c.view, &rects, focus, area);
                let modes = self
                    .panes
                    .get(&focus)
                    .map(|e| e.vt.modes())
                    .unwrap_or_default();
                (msg, modes, rects)
            })
            .collect();

        let mut dead = Vec::new();
        for (c, (layout_msg, focused_modes, rects)) in self.clients.iter_mut().zip(per) {
            // ModeSync BEFORE the Layout that assumes it (brief ordering).
            // A failed send means the reliable channel is wedged: the client
            // is dead, exactly like a failed Layout - a silently dropped
            // ModeSync would desync its terminal's modes.
            if c.synced_modes != focused_modes {
                let bytes = vt::mode_diff(c.synced_modes, focused_modes);
                if !bytes.is_empty()
                    && c.reliable_tx
                        .try_send(ServerMsg::ModeSync { bytes })
                        .is_err()
                {
                    dead.push(c.id);
                    continue;
                }
                c.synced_modes = focused_modes;
            }
            // Flush-then-re-emit: every frame a client draws after this is
            // consistent with the Layout generation it just received.
            c.dirty.lock().unwrap().clear();
            if c.reliable_tx.try_send(layout_msg).is_err() {
                dead.push(c.id);
                continue;
            }
            c.visible = rects.iter().map(|(pid, _)| *pid).collect();
            if reemit {
                let mut d = c.dirty.lock().unwrap();
                for (pid, _) in &rects {
                    if let Some(entry) = self.panes.get(pid) {
                        d.insert(*pid, entry.vt.frame());
                    }
                }
                drop(d);
                c.notify.notify_one();
            }
        }
        self.clients.retain(|c| !dead.contains(&c.id));
        // A wedged-channel death is a membership event like Gone: without a
        // re-push, survivors stay clamped to the dead client's dims until
        // some later event. Terminates: each pass removes >= 1 client.
        if !dead.is_empty() {
            self.push_layout(true);
        }
    }

    /// One client's `Layout`: the shared squad/tab catalog, with the
    /// active-squad/active-tab highlights and the rects/focus taken from
    /// THIS client's view.
    fn layout_msg_for(
        &self,
        view: (u64, TabId),
        rects: &[(u64, Rect)],
        focus: u64,
        area: (u16, u16),
    ) -> ServerMsg {
        let cwds: Vec<String> = self
            .session
            .squads
            .iter()
            .map(|s| s.canonical_cwd.clone())
            .collect();
        let names = squad::display_names(&cwds);
        let squads = self
            .session
            .squads
            .iter()
            .zip(names)
            .map(|(s, name)| SquadMeta {
                id: s.id,
                name,
                canonical_cwd: s.canonical_cwd.clone(),
                tabs: s
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| TabMeta {
                        id: t.id,
                        name: (i + 1).to_string(),
                    })
                    .collect(),
                // The viewed squad highlights the VIEWER's tab; other squads
                // show their own most-recently-active tab.
                active_tab: if s.id == view.0 {
                    s.tabs
                        .iter()
                        .position(|t| t.id == view.1)
                        .unwrap_or(s.active_tab)
                } else {
                    s.active_tab
                },
            })
            .collect();
        ServerMsg::Layout {
            squads,
            active_squad: view.0,
            panes: rects.to_vec(),
            focus,
            area,
            agents: self.agent_rows(),
        }
    }

    /// Join the registry-derived agent set to this server's live state (4a-G2,
    /// the fact-badge lattice): a mux-hosted row (this session) renders under
    /// its pane's squad, and a missing/dead pane forces `exited` REGARDLESS of
    /// any live-TTL badge (fact beats report - AC2-EDGE, and the reason a dead
    /// row can never resurrect: the pane set is authoritative here, AC2-FR). A
    /// row hosted in ANOTHER session is skipped (that session's server renders
    /// it). Non-pane rows (bg/headless/daemon-worker) are watch-only, matched
    /// to a squad by canonical cwd (exact or child path), else the catch-all
    /// (`squad: None`).
    fn agent_rows(&self) -> Vec<AgentRow> {
        let mut out = Vec::with_capacity(self.agents.len());
        for a in &self.agents {
            let (squad, pane_id, exited) = match &a.mux {
                Some((sess, pane)) => {
                    if *sess != self.session_name {
                        continue;
                    }
                    let squad = self.session.find_pane(*pane).map(|(sid, _)| sid);
                    let pane_dead = !self.panes.contains_key(pane);
                    (squad, Some(*pane), a.exited || pane_dead)
                }
                None => {
                    let squad = self
                        .session
                        .squads
                        .iter()
                        .find(|s| {
                            a.cwd == s.canonical_cwd
                                || a.cwd
                                    .strip_prefix(&s.canonical_cwd)
                                    .is_some_and(|rest| rest.starts_with('/'))
                        })
                        .map(|s| s.id);
                    (squad, None, a.exited)
                }
            };
            out.push(AgentRow {
                squad,
                name: a.name.clone(),
                pane_id,
                // Exit beats badge, structurally: no path renders `working`
                // over a dead pane.
                badge: if exited { None } else { a.badge },
                reason: if exited { None } else { a.reason.clone() },
                exited,
            });
        }
        out
    }

    /// Fan one pane's fresh frame out - but only into the dirty slots of
    /// clients whose VIEW contains the pane (AC2-FR). Unviewed panes cost
    /// zero wire traffic; their grids were already fed upstream.
    fn broadcast_pane(&self, pid: u64) {
        if !self.clients.iter().any(|c| c.visible.contains(&pid)) {
            return;
        }
        let Some(entry) = self.panes.get(&pid) else {
            return;
        };
        let frame = entry.vt.frame();
        for c in &self.clients {
            if c.visible.contains(&pid) {
                c.dirty.lock().unwrap().insert(pid, frame.clone());
                c.notify.notify_one();
            }
        }
    }

    /// Route a client's pane-rect mouse event (brief Locked 2, US1/US2/US3).
    /// An app that negotiated SGR mouse reporting owns its mouse: the event is
    /// SGR-encoded onto its PTY and the mux consumes nothing (AC3-HP). Otherwise
    /// the mux interprets it - wheel scrolls the pane's history (US1), a left
    /// drag paints a server-side selection all viewers see (US2), and release
    /// auto-copies (Warp behavior). Selection is per-pane, independent of focus;
    /// click-to-focus is a documented candidate, not shipped in v1.
    fn mouse(&mut self, client_id: u64, pane: u64, event: MouseEvent) {
        let Some(modes) = self.panes.get(&pane).map(|e| e.vt.modes()) else {
            return;
        };
        match route_mouse(modes, event.kind) {
            MouseAction::Passthrough => {
                let bytes = sgr_mouse_bytes(&event);
                if let Some(entry) = self.panes.get(&pane) {
                    let _ = entry.pty.write_input(&bytes);
                }
            }
            MouseAction::Scroll(delta) => {
                if let Some(e) = self.panes.get_mut(&pane) {
                    e.vt.scroll(delta);
                }
                self.broadcast_pane(pane);
            }
            MouseAction::SelectStart => {
                if let Some(e) = self.panes.get_mut(&pane) {
                    e.vt.selection_start(event.row, event.col);
                }
                self.broadcast_pane(pane);
            }
            MouseAction::SelectUpdate => {
                if let Some(e) = self.panes.get_mut(&pane) {
                    e.vt.selection_update(event.row, event.col);
                }
                self.broadcast_pane(pane);
            }
            MouseAction::SelectRelease => {
                // Auto-copy on release with a real selection; the highlight stays
                // held (Warp). A plain click (empty selection) clears any prior
                // highlight instead - never a third behavior.
                match self.panes.get(&pane).and_then(|e| e.vt.selection_text()) {
                    Some(text) => self.send_copy(client_id, text),
                    None => {
                        if let Some(e) = self.panes.get_mut(&pane) {
                            e.vt.selection_clear();
                        }
                        self.broadcast_pane(pane);
                    }
                }
            }
            MouseAction::Ignore => {}
        }
    }

    /// Ship extracted selection text to one client's clipboard chain (Locked 5).
    /// Reliable: a dropped copy is silent data loss. A wedged channel means the
    /// client is already dead (same policy as [`Core::notice`]).
    fn send_copy(&self, client_id: u64, text: String) {
        if let Some(c) = self.clients.iter().find(|c| c.id == client_id) {
            let _ = c.reliable_tx.try_send(ServerMsg::Copy { text });
        }
    }

    /// Live mode changes in a focused pane (vim toggling mouse reporting
    /// mid-session) must reach the terminals of that pane's VIEWERS now, not
    /// at the next focus change. Cheap: a flag read per output burst, bytes
    /// only on a diff.
    fn sync_focused_modes(&mut self) {
        let targets: Vec<Option<Modes>> = self
            .clients
            .iter()
            .map(|c| {
                let tab = self.viewed_tab(c.view)?;
                self.panes.get(&tab.focus).map(|e| e.vt.modes())
            })
            .collect();
        let mut dead = Vec::new();
        for (c, modes) in self.clients.iter_mut().zip(targets) {
            let Some(modes) = modes else { continue };
            if c.synced_modes != modes {
                let bytes = vt::mode_diff(c.synced_modes, modes);
                if !bytes.is_empty()
                    && c.reliable_tx
                        .try_send(ServerMsg::ModeSync { bytes })
                        .is_err()
                {
                    // Wedged reliable channel = dead client (same policy as
                    // push_layout); never silently desync a live terminal.
                    dead.push(c.id);
                    continue;
                }
                c.synced_modes = modes;
            }
        }
        self.clients.retain(|c| !dead.contains(&c.id));
        if !dead.is_empty() {
            self.push_layout(true);
        }
    }

    /// Close one pane: kill+reap its PTY, remove it from the tree (collapse +
    /// focus re-anchor inside `tree::close`), cascade empty tab -> squad ->
    /// session (Locked 8). Idempotent: an unknown pane (double-close race,
    /// AC4-ERR) is a no-op.
    fn close_pane(&mut self, pid: u64) -> Flow {
        let Some((sid, ti)) = self.session.find_pane(pid) else {
            // Unknown to the tree; still reap a stray registry entry so a
            // half-created pane can never leak a child process.
            self.reap_pane(pid);
            return Flow::Continue;
        };
        self.reap_pane(pid);
        let tid = self
            .session
            .squad(sid)
            .expect("find_pane returned a live squad id")
            .tabs[ti]
            .id;
        let vp = self.tab_rect(tid);
        let squad = self
            .session
            .squad_mut(sid)
            .expect("find_pane returned a live squad id");
        let tab = &mut squad.tabs[ti];
        if !tree::close(tab, vp, pid) {
            self.push_layout(true);
            return Flow::Continue;
        }
        match self.session.remove_tab(sid, ti) {
            RemoveOutcome::SessionEmpty => Flow::Shutdown,
            _ => {
                // The tab (and possibly its squad) died: every client whose
                // view named it re-anchors in this same mutation, then the
                // push delivers ModeSync -> Layout -> frames in order
                // (AC2-ERR).
                self.tab_areas.remove(&tid);
                self.reanchor_views();
                self.push_layout(true);
                Flow::Continue
            }
        }
    }

    /// Point `client_id`'s view at `(squad, tab)` and record the tab as its
    /// squad's most-recently-active (the anchor fresh attaches and re-anchors
    /// fall back to). Mutates the SENDER only (Locked 3).
    fn set_view(&mut self, client_id: u64, sid: u64, tid: TabId) {
        // This is the one gateway that maintains "a view always names a live
        // (squad, tab)" - enforce the postcondition here instead of trusting
        // callers: an unvalidated pair leaves the view untouched.
        let Some(sq) = self.session.squad_mut(sid) else {
            return;
        };
        let Some(idx) = sq.tabs.iter().position(|t| t.id == tid) else {
            return;
        };
        sq.active_tab = idx;
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == client_id) {
            c.view = (sid, tid);
        }
    }

    fn command(&mut self, client_id: u64, cmd: Command) -> Flow {
        // Commands act on the SENDER's view (Locked 3/4). A command from a
        // just-deregistered client has nothing to act on: drop fail-closed.
        let Some(view) = self.client_view(client_id) else {
            return Flow::Continue;
        };
        // Tree mutations tile against the viewed tab's CLAMPED area.
        let vp = self.tab_rect(view.1);
        match cmd {
            Command::SplitH | Command::SplitV => {
                let axis = if matches!(cmd, Command::SplitH) {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let Some(tab) = self.viewed_tab(view) else {
                    return Flow::Continue;
                };
                // Spawn at the focused pane's current size; the layout pass
                // right after resizes both halves to their real rects. New
                // shells within a squad start in its canonical root.
                let (rows, cols) = self
                    .panes
                    .get(&tab.focus)
                    .map(|e| e.vt.size())
                    .unwrap_or((vp.rows, vp.cols));
                let squad_cwd = self
                    .session
                    .squad(view.0)
                    .map(|s| s.canonical_cwd.clone())
                    .unwrap_or_default();
                let pid = match self.spawn_pane(rows, cols, &squad_cwd) {
                    Ok(p) => p,
                    Err(e) => {
                        // AC1-ERR: nothing mutated yet - the tree is
                        // untouched by construction (spawn-first ordering).
                        self.notice(client_id, format!("split failed: {e}"));
                        return Flow::Continue;
                    }
                };
                let Some(tab) = self.viewed_tab_mut(view) else {
                    return Flow::Continue;
                };
                match tree::split(tab, vp, axis, pid) {
                    Ok(()) => self.push_layout(true),
                    Err(e) => {
                        // AC1-EDGE: refused split reaps the pre-spawned
                        // shell; the tree was never touched.
                        self.reap_pane(pid);
                        self.notice(client_id, e.to_string());
                    }
                }
                Flow::Continue
            }
            Command::ClosePane => {
                let Some(tab) = self.viewed_tab(view) else {
                    return Flow::Continue;
                };
                self.close_pane(tab.focus)
            }
            Command::FocusDir(dir) => {
                let Some(tab) = self.viewed_tab_mut(view) else {
                    return Flow::Continue;
                };
                match tree::navigate(&tab.root, vp, tab.focus, dir) {
                    Some(next) => {
                        // Focus is per-tab, shared by co-viewers (Locked 4).
                        tab.focus = next;
                        self.push_layout(false);
                    }
                    None => self.notice(client_id, "no pane in that direction"),
                }
                Flow::Continue
            }
            Command::ResizeDir(dir) => {
                let Some(tab) = self.viewed_tab_mut(view) else {
                    return Flow::Continue;
                };
                if tree::resize(tab, vp, dir, tree::RESIZE_STEP) {
                    self.push_layout(true);
                } else {
                    // BEL only when nothing changed.
                    self.notice(client_id, "cannot resize further");
                }
                Flow::Continue
            }
            Command::NewTab => {
                // The new tab's first (and so far only) viewer is the
                // sender: spawn at the sender's own content area (Locked 5's
                // NewTab event; the push below applies the same clamp).
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                let squad_cwd = self
                    .session
                    .squad(view.0)
                    .map(|s| s.canonical_cwd.clone())
                    .unwrap_or_default();
                let pid = match self.spawn_pane(rows, cols, &squad_cwd) {
                    Ok(p) => p,
                    Err(e) => {
                        // Stay on the current tab, error visible.
                        self.notice(client_id, format!("new tab failed: {e}"));
                        return Flow::Continue;
                    }
                };
                let tid = self.session.mint_tab_id();
                let Some(squad) = self.session.squad_mut(view.0) else {
                    return Flow::Continue;
                };
                squad.tabs.push(Tab {
                    id: tid,
                    root: Node::Leaf(pid),
                    focus: pid,
                });
                // NewTab switches only the SENDER (Locked 3); co-viewers
                // stay where they are and see the catalog grow.
                self.set_view(client_id, view.0, tid);
                self.push_layout(true);
                Flow::Continue
            }
            Command::SelectTab(tid) => {
                match self.session.find_tab(tid) {
                    Some((sid, _)) => {
                        self.set_view(client_id, sid, tid);
                        self.push_layout(true);
                    }
                    // AC2-EDGE: a dead TabId (catalog changed under the
                    // selector) is refused fail-closed; the sender's view is
                    // untouched.
                    None => self.notice(client_id, "no such tab"),
                }
                Flow::Continue
            }
            Command::NextTab | Command::PrevTab => {
                let Some(squad) = self.session.squad(view.0) else {
                    return Flow::Continue;
                };
                let n = squad.tabs.len();
                if n < 2 {
                    self.notice(client_id, "no other tab");
                    return Flow::Continue;
                }
                let cur = squad
                    .tabs
                    .iter()
                    .position(|t| t.id == view.1)
                    .unwrap_or(squad.active_tab);
                let next = if matches!(cmd, Command::NextTab) {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                let tid = squad.tabs[next].id;
                self.set_view(client_id, view.0, tid);
                self.push_layout(true);
                Flow::Continue
            }
            Command::CloseTab => {
                let Some((sid, ti)) = self.session.find_tab(view.1) else {
                    return Flow::Continue;
                };
                let pids =
                    tree::leaves(&self.session.squad(sid).expect("live squad").tabs[ti].root);
                for pid in pids {
                    self.reap_pane(pid);
                }
                match self.session.remove_tab(sid, ti) {
                    RemoveOutcome::SessionEmpty => Flow::Shutdown,
                    _ => {
                        // Everyone who viewed the dead tab (sender included)
                        // re-anchors in this same mutation (AC2-ERR).
                        self.tab_areas.remove(&view.1);
                        self.reanchor_views();
                        self.push_layout(true);
                        Flow::Continue
                    }
                }
            }
            Command::SelectSquad(id) => {
                match self.session.squad(id) {
                    Some(sq) => {
                        let tid = sq
                            .tabs
                            .get(sq.active_tab)
                            .or_else(|| sq.tabs.first())
                            .expect("a squad always has a tab")
                            .id;
                        self.set_view(client_id, id, tid);
                        self.push_layout(true);
                    }
                    // A stale id (squad died racing the selector) is refused
                    // fail-closed; the client re-anchors off the next Layout
                    // it already received.
                    None => self.notice(client_id, "no such squad"),
                }
                Flow::Continue
            }
        }
    }

    fn handle(&mut self, msg: CoreMsg) -> Flow {
        match msg {
            CoreMsg::Attach {
                id,
                rows,
                cols,
                cwd,
                squad_key,
                reliable_tx,
                dirty,
                notify,
            } => {
                self.attach(id, rows, cols, cwd, squad_key, reliable_tx, dirty, notify);
                Flow::Continue
            }
            CoreMsg::Input { id, bytes } => {
                // Input routes to the SENDER's viewed tab's focused pane
                // (Locked 4). Fail closed when there is no live view or
                // focused pane: dropped, never a panic - a re-anchor already
                // moved the view, or the exit signal is about to. A write
                // error means the child just exited mid-keystroke - same
                // policy.
                let focus = self
                    .client_view(id)
                    .and_then(|view| self.viewed_tab(view))
                    .map(|tab| tab.focus);
                if let Some(focus) = focus {
                    // Writer-claim interlock (4a-G3, AC3-UI): while the relay
                    // holds an agent pane's claim, human keystrokes bounce
                    // with a visible `busy: relay` notice (the client sounds
                    // BEL for every Notice). In-memory lookup + one kill(0)
                    // probe - a DEAD holder releases right here, so typing
                    // resumes without any sweep or restart (AC3-FR). General
                    // panes are never in `claims` (spawn-time opt-in).
                    if let Some(&holder) = self.claims.get(&focus) {
                        if pid_alive(holder) {
                            self.notice(id, "busy: relay");
                            return Flow::Continue;
                        }
                        self.claims.remove(&focus);
                    }
                    // A keystroke that will be delivered returns a scrolled pane
                    // to the live bottom, so input always lands on the visible
                    // line (AC1-ERR, Invariant). No-op when already live.
                    if self
                        .panes
                        .get(&focus)
                        .is_some_and(|e| e.vt.display_offset() != 0)
                    {
                        if let Some(e) = self.panes.get_mut(&focus) {
                            e.vt.scroll_to_bottom();
                        }
                        self.broadcast_pane(focus);
                    }
                    if let Some(entry) = self.panes.get(&focus) {
                        if let Err(crate::pty::PtyError::Write(e)) = entry.pty.write_input(&bytes) {
                            // Disconnected = child just exited (the exit
                            // signal follows; stay silent). Full = the
                            // child stopped reading (^S, SIGSTOP): the
                            // drop must not be invisible to the typist.
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                self.notice(id, "pane not accepting input; keys dropped");
                            }
                        }
                    }
                }
                Flow::Continue
            }
            CoreMsg::Resize { id, rows, cols } => {
                // One client's terminal changed size: update ITS dims; the
                // push recomputes every viewed tab's clamp (Locked 5's
                // Resize event).
                if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
                    c.dims = (rows, cols);
                }
                self.push_layout(true);
                Flow::Continue
            }
            CoreMsg::Command { id, cmd } => self.command(id, cmd),
            CoreMsg::Mouse { id, pane, event } => {
                self.mouse(id, pane, event);
                Flow::Continue
            }
            CoreMsg::Query(reply) => {
                let _ = reply.send(ServerMsg::Info {
                    session: self.session_name.clone(),
                    clients: self.clients.len() as u32,
                    squads: self.session.squads.len() as u32,
                    panes: self.panes.len() as u32,
                });
                Flow::Continue
            }
            CoreMsg::Kill => {
                // kill-server: the second (and last) sanctioned exit path
                // (Locked 12). Bye every client, kill every pane child
                // (AC4-FR: nothing outlives the session), then shut down -
                // the SocketGuard unlinks on the way out.
                self.bye_all("killed");
                for entry in self.panes.values() {
                    entry.pty.kill();
                }
                Flow::Shutdown
            }
            CoreMsg::Gone(id) => {
                // Gone is a geometry event (Locked 5, AC1-ERR): a vanished
                // constraining client releases its clamp, so the tab regrows
                // for the survivors in this same pass - Detach and an abrupt
                // socket death take the identical path.
                self.clients.retain(|c| c.id != id);
                self.push_layout(true);
                Flow::Continue
            }
            // -- v4 control verbs. Each answers on its oneshot; a dropped
            // receiver (client vanished mid-verb) makes the send a no-op.
            CoreMsg::PaneLs(reply) => {
                let _ = reply.send(ServerMsg::PaneList {
                    panes: self.pane_infos(),
                });
                Flow::Continue
            }
            CoreMsg::PaneRead {
                pane,
                lines,
                block,
                reply,
            } => {
                let msg = match self.panes.get(&pane) {
                    Some(entry) => match block {
                        // Block mode: `lines` is ignored; an unanswerable block
                        // is BLOCK_UNAVAILABLE, never empty/stale text.
                        Some(sel) => match entry.vt.read_block(sel) {
                            Ok(read) => ServerMsg::PaneText {
                                pane_id: pane,
                                text: read.text.clone(),
                                block: Some(read.meta()),
                            },
                            Err(()) => ServerMsg::Err {
                                code: err_code::BLOCK_UNAVAILABLE,
                                msg: format!("pane {pane}: no such block"),
                            },
                        },
                        // Plain read: `lines` reaches into history (v6, US5);
                        // no `--lines` keeps the visible-grid behavior (AC5-UI).
                        None => {
                            let text = match lines {
                                Some(n) => entry.vt.read_tail(n),
                                None => frame_text(&entry.vt.frame()),
                            };
                            ServerMsg::PaneText {
                                pane_id: pane,
                                text,
                                block: None,
                            }
                        }
                    },
                    None => dead_pane(pane),
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneRun {
                squad_key,
                cwd,
                argv,
                cols,
                rows,
                claim,
                reply,
            } => {
                let rows = rows.unwrap_or(vt::DEFAULT_ROWS);
                let cols = cols.unwrap_or(vt::DEFAULT_COLS);
                let msg = match self.run_pane(squad_key, cwd, argv, rows, cols, claim) {
                    Ok(pane_id) => ServerMsg::PaneSpawned { pane_id },
                    Err(e) => ServerMsg::Err {
                        code: err_code::SPAWN_FAILED,
                        msg: e,
                    },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneSend { pane, bytes, reply } => {
                let msg = match self.panes.get(&pane) {
                    None => dead_pane(pane),
                    Some(entry) => match entry.pty.write_input(&bytes) {
                        Ok(()) => ServerMsg::Ok,
                        // A dead/wedged pane fails closed (AC4-ERR): the child
                        // exited (BrokenPipe) or stopped reading (WouldBlock).
                        // Either way the send did not land - never a silent Ok.
                        Err(e) => ServerMsg::Err {
                            code: err_code::DEAD_PANE,
                            msg: format!("pane {pane} send failed: {e}"),
                        },
                    },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneWait {
                pane,
                quiet_ms,
                regex,
                timeout_ms,
                command_done,
                reply,
            } => {
                let Some(entry) = self.panes.get(&pane) else {
                    let _ = reply.send(dead_pane(pane));
                    return Flow::Continue;
                };
                // Seed `initial` from the pane's REAL current grid, not the
                // watch value: the watch text is refreshed only while a
                // watcher is subscribed, so output that landed before this
                // wait started lives only in the grid. Reading it and
                // subscribing are atomic on the single-threaded core loop, so
                // there is no missed-output gap; the wait itself then runs
                // entirely off-loop.
                let initial: Arc<str> = Arc::from(frame_text(&entry.vt.frame()));
                // Baseline the command-done watch against blocks already done,
                // atomically with the subscribe (same core-loop turn). A pane
                // that never emits `D` (no shell-init) simply times out - always
                // bounded, never infinite; the CLI notes the degradation. We
                // cannot inject a quiet fallback here: a pane that WILL emit a
                // marker after a delay looks markerless at subscribe time, so a
                // quiet settle would fire during that delay and pre-empt the D.
                let done_baseline = entry.vt.last_done().map(|(seq, _)| seq);
                let rx = self
                    .pane_watch
                    .get(&pane)
                    .expect("pane_watch is in lockstep with panes")
                    .subscribe();
                tokio::spawn(run_wait(
                    rx,
                    quiet_ms,
                    regex,
                    timeout_ms,
                    initial,
                    WaitDoneWatch {
                        enabled: command_done,
                        baseline: done_baseline,
                    },
                    reply,
                ));
                Flow::Continue
            }
            CoreMsg::PaneKill { pane, reply } => {
                if !self.panes.contains_key(&pane) {
                    let _ = reply.send(dead_pane(pane));
                    return Flow::Continue;
                }
                // Reply Ok BEFORE propagating a possible session-ending
                // Shutdown, so the client always learns the kill landed even
                // when it closed the last pane.
                let flow = self.close_pane(pane);
                let _ = reply.send(ServerMsg::Ok);
                flow
            }
            CoreMsg::PaneClaim {
                pane,
                holder_pid,
                reply,
            } => {
                let msg = if !self.panes.contains_key(&pane) {
                    dead_pane(pane)
                } else if !self.claim_eligible.contains(&pane) {
                    // AC3-EDGE: general panes never consult a claim; refusing
                    // the acquire keeps the opt-in boundary visible.
                    ServerMsg::Err {
                        code: err_code::BAD_REQUEST,
                        msg: format!(
                            "pane {pane} is not claim-eligible (only agent panes spawned with --claim carry the writer interlock)"
                        ),
                    }
                } else {
                    match self.claims.get(&pane) {
                        // A live other holder refuses; a dead or same-pid
                        // holder is replaced (re-acquire is idempotent).
                        Some(&held) if held != holder_pid && pid_alive(held) => ServerMsg::Err {
                            code: err_code::BAD_REQUEST,
                            msg: format!("pane {pane} writer claim held by pid {held}"),
                        },
                        _ => {
                            self.claims.insert(pane, holder_pid);
                            ServerMsg::Ok
                        }
                    }
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneRelease { pane, reply } => {
                // Idempotent: releasing an unheld (or already-exited) pane is
                // Ok - the burst may have raced the exit teardown, which
                // releases unconditionally.
                self.claims.remove(&pane);
                let _ = reply.send(ServerMsg::Ok);
                Flow::Continue
            }
            CoreMsg::AgentRows(rows) => {
                self.agents = rows;
                // Rects are unchanged; only the Layout's agent rows moved -
                // push without re-emitting frames (AC1-UI: visible within one
                // layout push; AC2-UI: the read happened off-loop).
                self.push_layout(false);
                Flow::Continue
            }
        }
    }

    /// Queue a `Bye` to every client (session end / shutdown).
    fn bye_all(&self, reason: &str) {
        for c in &self.clients {
            let _ = c.reliable_tx.try_send(ServerMsg::Bye {
                reason: reason.to_string(),
            });
        }
    }
}

/// The canonical `Err` for a pane id no live pane owns (read/send/wait/kill).
fn dead_pane(pane: u64) -> ServerMsg {
    ServerMsg::Err {
        code: err_code::DEAD_PANE,
        msg: format!("no such pane: {pane}"),
    }
}

/// The `--command-done` arm of a wait: resolve when the pane's last-completed
/// block advances past `baseline`.
#[derive(Clone, Copy)]
struct WaitDoneWatch {
    enabled: bool,
    baseline: Option<u64>,
}

/// The off-loop `PaneWait` watcher: observes a pane's output watch and answers
/// the control connection with the outcome. Nothing here runs on the core
/// loop; the deadline is server-enforced and a vanished client (`reply.closed`)
/// drops the watch at once.
async fn run_wait(
    mut rx: watch::Receiver<WaitTick>,
    quiet_ms: Option<u64>,
    pattern: Option<regex::Regex>,
    timeout_ms: u64,
    initial_text: Arc<str>,
    done_watch: WaitDoneWatch,
    mut reply: ControlReply,
) {
    // An already-present match settles immediately (the text at subscribe time
    // already carries every prior byte, so there is no missed-output gap).
    if let Some(re) = &pattern {
        if re.is_match(&initial_text) {
            let _ = reply.send(ServerMsg::WaitDone {
                outcome: WaitOutcome::Matched,
            });
            return;
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let quiet = quiet_ms.map(Duration::from_millis);
    let mut last_activity = tokio::time::Instant::now();
    let outcome = loop {
        // The quiet wakeup exists only when a quiet window was requested; it
        // is recomputed every iteration so each output burst resets it.
        let quiet_at = quiet.map(|q| last_activity + q);
        tokio::select! {
            biased;
            // Client vanished: abandon the watch (Failure Modes: a client
            // disconnect drops the watch).
            _ = reply.closed() => return,
            _ = tokio::time::sleep_until(deadline) => break WaitOutcome::Timeout,
            _ = async { tokio::time::sleep_until(quiet_at.unwrap()).await }, if quiet_at.is_some() => {
                break WaitOutcome::Quiet;
            }
            changed = rx.changed() => {
                // A dropped sender (pane reaped) reads as exited too.
                if changed.is_err() {
                    break WaitOutcome::PaneExited;
                }
                let tick = rx.borrow_and_update().clone();
                if tick.exited {
                    break WaitOutcome::PaneExited;
                }
                if let Some(re) = &pattern {
                    if re.is_match(&tick.text) {
                        break WaitOutcome::Matched;
                    }
                }
                // A command finished if the last-done block advanced past the
                // baseline captured at subscribe time.
                if done_watch.enabled {
                    if let Some((seq, exit)) = tick.last_done {
                        if done_watch.baseline.map_or(true, |b| seq > b) {
                            break WaitOutcome::CommandDone { exit };
                        }
                    }
                }
                last_activity = tokio::time::Instant::now();
            }
        }
    };
    let _ = reply.send(ServerMsg::WaitDone { outcome });
}

async fn serve(
    listener: std::os::unix::net::UnixListener,
    socket: &Path,
    session_name: String,
) -> i32 {
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("fno mux: listener setup failed: {e}");
        return 1;
    }
    let listener = match tokio::net::UnixListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fno mux: listener setup failed: {e}");
            return 1;
        }
    };

    // One shared pane-tagged output channel + one exit channel for all PTY
    // reader threads. Squads (and their first panes) are born from attaches;
    // nothing is spawned upfront.
    let (out_tx, mut out_rx) = mpsc::channel::<(u64, Vec<u8>)>(256);
    let (exit_tx, mut exit_rx) = mpsc::channel::<u64>(64);
    let (core_tx, mut core_rx) = mpsc::channel::<CoreMsg>(256);

    let mut core = Core {
        session: Session::default(),
        panes: HashMap::new(),
        pane_watch: HashMap::new(),
        clients: Vec::new(),
        next_pane_id: 1,
        next_squad_id: 1,
        tab_areas: HashMap::new(),
        session_name,
        shells: shell_candidates(std::env::var_os("SHELL").as_deref()),
        out_tx,
        exit_tx,
        agents: Vec::new(),
        claim_eligible: HashSet::new(),
        claims: HashMap::new(),
    };

    // The off-loop registry reader (4a-G2): a 1s interval task stats/reads
    // the fno-agents registry on the blocking pool, re-derives the agent row
    // set (TTL aging included), and sends it to the core only when it
    // changed. The render path never touches the file (AC2-UI; the origin
    // freeze class), and its staleness is bounded by this one interval.
    {
        let core_tx = core_tx.clone();
        let path = agents_view::registry_path();
        tokio::spawn(async move {
            let mut state = agents_view::ReaderState::default();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let stat_path = path.clone();
                let stamp = tokio::task::spawn_blocking(move || {
                    std::fs::metadata(&stat_path)
                        .ok()
                        .map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()))
                })
                .await
                .ok()
                .flatten();
                let read_path = path.clone();
                let changed = stamp != {
                    // Peek: ReaderState owns the cached stamp; read the
                    // file only when it moved (mtime+len gate).
                    state.cached_stamp()
                };
                let raw = if changed {
                    tokio::task::spawn_blocking(move || std::fs::read_to_string(&read_path).ok())
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Some(rows) = state.tick(stamp, move || raw, now) {
                    if core_tx.send(CoreMsg::AgentRows(rows)).await.is_err() {
                        return; // core loop gone; the server is shutting down
                    }
                }
            }
        });
    }

    // The squad-key cache, shared by the per-connection handshake tasks. The
    // blocking git resolution runs there (spawn_blocking), NEVER on the core
    // loop - a hung git may delay ONE attach by the 2s timeout, but every
    // pane and peer keeps streaming (the drive-freeze class).
    let resolver = Arc::new(Mutex::new(Resolver::default()));

    // Accept loop: handshake each connection off the core loop's back.
    let accept_core_tx = core_tx.clone();
    tokio::spawn(async move {
        let mut next_id: u64 = 1;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let id = next_id;
                    next_id += 1;
                    tokio::spawn(handle_client(
                        stream,
                        accept_core_tx.clone(),
                        resolver.clone(),
                        id,
                    ));
                }
                Err(e) => {
                    eprintln!("fno mux: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();

    eprintln!("fno mux: serving {}", socket.display());
    let flow = loop {
        tokio::select! {
            chunk = out_rx.recv() => {
                // out_tx lives in Core, so recv never yields None.
                let Some((pid, bytes)) = chunk else { break Flow::Shutdown };
                let mut touched = HashSet::new();
                if let Some(entry) = core.panes.get_mut(&pid) {
                    entry.vt.feed(&bytes);
                    touched.insert(pid);
                }
                // Coalesce whatever is already queued, per pane, into one
                // frame each (a flooding pane costs one snapshot per burst).
                while let Ok((p2, b2)) = out_rx.try_recv() {
                    if let Some(entry) = core.panes.get_mut(&p2) {
                        entry.vt.feed(&b2);
                        touched.insert(p2);
                    }
                }
                for pid in touched {
                    core.broadcast_pane(pid);
                    // Refresh any PaneWait watcher on this pane (no-op unless
                    // one is subscribed).
                    core.note_pane_output(pid);
                }
                core.sync_focused_modes();
            }
            exited = exit_rx.recv() => {
                let Some(pid) = exited else { break Flow::Shutdown };
                if core.close_pane(pid) == Flow::Shutdown {
                    break Flow::Shutdown;
                }
            }
            msg = core_rx.recv() => {
                // core_tx lives in the accept loop, so recv never yields None.
                let Some(msg) = msg else { break Flow::Shutdown };
                // Coalesce resize storms PER CLIENT: only each client's
                // final geometry hits its viewed tab's clamp (AC1-FR). Other
                // messages drained here run after, in arrival order.
                if let CoreMsg::Resize { id, rows, cols } = msg {
                    let mut last: HashMap<u64, (u16, u16)> = HashMap::new();
                    last.insert(id, (rows, cols));
                    let mut order = vec![id];
                    let mut pending = Vec::new();
                    while let Ok(m) = core_rx.try_recv() {
                        match m {
                            CoreMsg::Resize { id, rows, cols } => {
                                if last.insert(id, (rows, cols)).is_none() {
                                    order.push(id);
                                }
                            }
                            other => pending.push(other),
                        }
                    }
                    let mut flow = Flow::Continue;
                    for id in order {
                        let (rows, cols) = last[&id];
                        flow = core.handle(CoreMsg::Resize { id, rows, cols });
                        if flow == Flow::Shutdown { break; }
                    }
                    for m in pending {
                        if flow == Flow::Shutdown { break; }
                        flow = core.handle(m);
                    }
                    if flow == Flow::Shutdown { break Flow::Shutdown; }
                } else if core.handle(msg) == Flow::Shutdown {
                    break Flow::Shutdown;
                }
            }
            _ = async { sigterm.as_mut().unwrap().recv().await }, if sigterm.is_some() => break Flow::Shutdown,
            _ = async { sigint.as_mut().unwrap().recv().await }, if sigint.is_some() => break Flow::Shutdown,
        }
    };
    if flow == Flow::Shutdown {
        core.bye_all("session ended");
        // Give writer tasks a beat to flush the Byes; a lost Bye reads as
        // "session ended (server closed)" client-side, so this is best-effort.
        tokio::time::sleep(BYE_FLUSH).await;
    }
    0
}

/// Resolve a client cwd to its canonical squad key, OFF the core loop: the
/// git run blocks (bounded 2s) and the loop must never wait on it. Cache
/// check first; a miss runs on a blocking thread. Two racing misses on one
/// cwd both resolve and insert the same idempotent answer - cheaper than a
/// lock held across a subprocess. Shared by `Attach` and `PaneRun`.
async fn resolve_squad_key(resolver: &Arc<Mutex<Resolver>>, cwd: &str) -> String {
    // The guard drops before the await (a temporary living across it would
    // un-Send the future).
    let cached = resolver.lock().unwrap().cached(cwd);
    if let Some(hit) = cached {
        return hit;
    }
    let owned = cwd.to_string();
    let for_task = owned.clone();
    let key = tokio::task::spawn_blocking(move || squad::resolve_key(&for_task))
        .await
        .unwrap_or_else(|_| owned.clone());
    resolver.lock().unwrap().insert(owned, key.clone());
    key
}

/// A one-shot v4 control connection: version-check, route the verb to the core
/// loop with a oneshot reply, answer with exactly one message, close. A client
/// that vanishes mid-verb drops the reply receiver, which the off-loop
/// `PaneWait` watcher observes (`reply.closed()`) and abandons its watch.
async fn handle_control(
    mut stream: UnixStream,
    core_tx: mpsc::Sender<CoreMsg>,
    resolver: Arc<Mutex<Resolver>>,
    proto: u32,
    build: String,
    verb: ControlVerb,
) {
    // Control verbs are versioned (AC4-FR): refuse a skewed connection loudly,
    // naming both versions, unlike the frozen Query/KillServer pair.
    if let Err(reason) = check_attach_version(proto, &build) {
        let _ = write_msg(
            &mut stream,
            &ServerMsg::Err {
                code: err_code::VERSION_SKEW,
                msg: reason,
            },
        )
        .await;
        return;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let sent = match verb {
        ControlVerb::PaneLs => core_tx.send(CoreMsg::PaneLs(reply_tx)).await,
        ControlVerb::PaneRead { pane, lines, block } => {
            core_tx
                .send(CoreMsg::PaneRead {
                    pane,
                    lines,
                    block,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneRun {
            cwd,
            argv,
            cols,
            rows,
            claim,
        } => {
            // Resolve the squad key off the core loop, exactly like Attach.
            let squad_key = resolve_squad_key(&resolver, &cwd).await;
            core_tx
                .send(CoreMsg::PaneRun {
                    squad_key,
                    cwd,
                    argv,
                    cols,
                    rows,
                    claim,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneSend { pane, bytes } => {
            core_tx
                .send(CoreMsg::PaneSend {
                    pane,
                    bytes,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneWait {
            pane,
            quiet_ms,
            pattern,
            timeout_ms,
            command_done,
        } => {
            // Compile the pattern HERE, off the core loop (bounded CPU, but
            // the single-threaded loop must never do it). A bad pattern is a
            // BAD_REQUEST answered inline; the loop only ever gets a ready
            // `Option<Regex>`.
            let regex = match pattern.as_deref().map(regex::Regex::new).transpose() {
                Ok(r) => r,
                Err(e) => {
                    let _ = write_msg(
                        &mut stream,
                        &ServerMsg::Err {
                            code: err_code::BAD_REQUEST,
                            msg: format!("bad --pattern: {e}"),
                        },
                    )
                    .await;
                    return;
                }
            };
            core_tx
                .send(CoreMsg::PaneWait {
                    pane,
                    quiet_ms,
                    regex,
                    timeout_ms,
                    command_done,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneKill { pane } => {
            core_tx
                .send(CoreMsg::PaneKill {
                    pane,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneClaim { pane, holder_pid } => {
            core_tx
                .send(CoreMsg::PaneClaim {
                    pane,
                    holder_pid,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneRelease { pane } => {
            core_tx
                .send(CoreMsg::PaneRelease {
                    pane,
                    reply: reply_tx,
                })
                .await
        }
    };
    if sent.is_err() {
        return; // the server is shutting down
    }
    // Await the reply, but abandon it the moment the client disconnects: the
    // select drops `reply_rx`, and a pending `PaneWait` watcher sees the
    // closed receiver and drops its watch (Failure Modes: disconnect drops
    // the watch). A one-shot control client sends nothing after its verb, so
    // the peer-read only ever resolves on EOF.
    tokio::select! {
        reply = reply_rx => {
            if let Ok(msg) = reply {
                let _ = write_msg(&mut stream, &msg).await;
            }
        }
        _ = wait_for_peer_close(&mut stream) => {}
    }
}

/// Resolve when the control peer closes its half (or sends stray bytes). Used
/// only to notice a mid-`PaneWait` disconnect; a one-shot client is otherwise
/// silent until it reads the reply and closes.
async fn wait_for_peer_close(stream: &mut UnixStream) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 1];
    let _ = stream.read(&mut buf).await;
}

/// Handshake a fresh connection, then split it into the reader loop (this
/// task) and the writer task.
async fn handle_client(
    mut stream: UnixStream,
    core_tx: mpsc::Sender<CoreMsg>,
    resolver: Arc<Mutex<Resolver>>,
    id: u64,
) {
    let attach = tokio::time::timeout(ATTACH_TIMEOUT, read_msg::<_, ClientMsg>(&mut stream)).await;
    let (rows, cols, cwd) = match attach {
        Ok(Ok(ClientMsg::Attach {
            proto,
            build,
            rows,
            cols,
            cwd,
        })) => {
            if let Err(reason) = check_attach_version(proto, &build) {
                // Refuse loudly with both versions; the client relays it.
                let _ = write_msg(&mut stream, &ServerMsg::Bye { reason }).await;
                return;
            }
            (rows, cols, cwd)
        }
        // Pre-Attach management pair (wire shapes FROZEN, no version
        // handshake - proto.rs): Query answers one Info then closes;
        // KillServer triggers shutdown. Neither registers a client.
        Ok(Ok(ClientMsg::Query)) => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if core_tx.send(CoreMsg::Query(reply_tx)).await.is_ok() {
                if let Ok(info) = reply_rx.await {
                    let _ = write_msg(&mut stream, &info).await;
                }
            }
            return;
        }
        Ok(Ok(ClientMsg::KillServer)) => {
            let _ = core_tx.send(CoreMsg::Kill).await;
            return;
        }
        // A v4 one-shot control connection (`fno mux pane ...`): versioned
        // like Attach, answered with exactly one reply, then closed. Never
        // registers a client, never splits into reader/writer tasks.
        Ok(Ok(ClientMsg::Control { proto, build, verb })) => {
            handle_control(stream, core_tx, resolver, proto, build, verb).await;
            return;
        }
        // Liveness probes connect and vanish; malformed first messages and
        // timeouts close the same way: without touching any pane.
        _ => return,
    };

    let squad_key = resolve_squad_key(&resolver, &cwd).await;

    let (reliable_tx, reliable_rx) = mpsc::channel::<ServerMsg>(RELIABLE_CAP);
    let dirty: DirtyMap = Arc::default();
    let notify = Arc::new(Notify::new());
    if core_tx
        .send(CoreMsg::Attach {
            id,
            rows,
            cols,
            cwd,
            squad_key,
            reliable_tx,
            dirty: dirty.clone(),
            notify: notify.clone(),
        })
        .await
        .is_err()
    {
        return;
    }
    let (read_half, write_half) = stream.into_split();
    tokio::spawn(client_writer(
        write_half,
        reliable_rx,
        dirty,
        notify,
        core_tx.clone(),
        id,
    ));
    client_reader(read_half, core_tx, id).await;
}

/// Reliable inbound path: every message is awaited into the core channel.
/// Any read error (including an abruptly killed client) deregisters the
/// client and leaves every pane untouched (AC4-HP).
async fn client_reader(mut r: OwnedReadHalf, core_tx: mpsc::Sender<CoreMsg>, id: u64) {
    loop {
        match read_msg::<_, ClientMsg>(&mut r).await {
            Ok(ClientMsg::Input(bytes)) => {
                if core_tx.send(CoreMsg::Input { id, bytes }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Resize { rows, cols }) => {
                if core_tx
                    .send(CoreMsg::Resize { id, rows, cols })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::Command(cmd)) => {
                if core_tx.send(CoreMsg::Command { id, cmd }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Mouse { pane, event }) => {
                if core_tx
                    .send(CoreMsg::Mouse { id, pane, event })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::Detach) => {
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
            // A second Attach, a pre-Attach-only Query/KillServer, or a
            // one-shot Control on a live connection is a protocol violation:
            // log it (this stderr is the session log) and close rather than
            // acting on a confused stream.
            Ok(
                msg @ (ClientMsg::Attach { .. }
                | ClientMsg::Query
                | ClientMsg::KillServer
                | ClientMsg::Control { .. }),
            ) => {
                let name = match msg {
                    ClientMsg::Attach { .. } => "Attach",
                    ClientMsg::Query => "Query",
                    ClientMsg::Control { .. } => "Control",
                    _ => "KillServer",
                };
                eprintln!("fno mux: client {id} sent {name} on a live connection; dropping it");
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
            Err(e) => {
                // Includes the abrupt-close case (killed client): routine, but
                // one log line makes a misbehaving client diagnosable.
                if !matches!(e, crate::proto::ProtoError::Closed) {
                    eprintln!("fno mux: client {id} read failed: {e}");
                }
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
        }
    }
}

/// The per-client writer: reliable messages FIRST (biased select - a Layout
/// is never stuck behind a frame burst), then the droppable dirty map. A
/// write failure drops THIS client only (AC4-ERR generalized).
async fn client_writer(
    mut w: OwnedWriteHalf,
    mut reliable_rx: mpsc::Receiver<ServerMsg>,
    dirty: DirtyMap,
    notify: Arc<Notify>,
    core_tx: mpsc::Sender<CoreMsg>,
    id: u64,
) {
    loop {
        tokio::select! {
            biased;
            msg = reliable_rx.recv() => {
                let Some(msg) = msg else { break }; // deregistered by the core
                let is_bye = matches!(msg, ServerMsg::Bye { .. });
                if write_msg(&mut w, &msg).await.is_err() {
                    let _ = core_tx.send(CoreMsg::Gone(id)).await;
                    break;
                }
                if is_bye {
                    break;
                }
            }
            _ = notify.notified() => {
                // Drain the whole map; every frame is self-contained, and a
                // frame inserted mid-drain re-notifies, so nothing is lost.
                loop {
                    // Reliable messages queued mid-flood jump ahead of the
                    // frame stream (codex P2): continuous re-insertion could
                    // otherwise pin the writer inside this arm, and the
                    // biased select only prioritizes at the select point -
                    // not while an arm is running.
                    loop {
                        match reliable_rx.try_recv() {
                            Ok(msg) => {
                                let is_bye = matches!(msg, ServerMsg::Bye { .. });
                                if write_msg(&mut w, &msg).await.is_err() {
                                    let _ = core_tx.send(CoreMsg::Gone(id)).await;
                                    return;
                                }
                                if is_bye {
                                    return;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let next = {
                        let mut d = dirty.lock().unwrap();
                        let key = d.keys().next().copied();
                        key.map(|k| (k, d.remove(&k).expect("key just seen")))
                    };
                    let Some((pane_id, frame)) = next else { break };
                    if write_msg(&mut w, &ServerMsg::Frame { pane_id, frame })
                        .await
                        .is_err()
                    {
                        let _ = core_tx.send(CoreMsg::Gone(id)).await;
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_control_dead_pane_err_carries_the_code_and_id() {
        match dead_pane(99) {
            ServerMsg::Err { code, msg } => {
                assert_eq!(code, err_code::DEAD_PANE);
                assert!(msg.contains("99"), "{msg}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn server_control_wait_tick_default_is_empty_and_live() {
        let t = WaitTick::default();
        assert!(!t.exited);
        assert!(t.text.is_empty());
    }

    fn mouse_mode() -> Modes {
        Modes {
            mouse_click: true,
            sgr_mouse: true,
            ..Modes::default()
        }
    }

    #[test]
    fn route_mouse_passes_through_when_app_owns_mouse() {
        // AC3-HP: a pane in SGR mouse mode consumes nothing mux-side.
        for kind in [
            MouseKind::WheelUp,
            MouseKind::Press(MouseButton::Left),
            MouseKind::Drag(MouseButton::Left),
            MouseKind::Release(MouseButton::Left),
        ] {
            assert_eq!(route_mouse(mouse_mode(), kind), MouseAction::Passthrough);
        }
    }

    #[test]
    fn route_mouse_interprets_when_pane_has_no_mouse_mode() {
        // US1/US2: a plain shell pane scrolls and selects mux-side.
        let plain = Modes::default();
        assert_eq!(
            route_mouse(plain, MouseKind::WheelUp),
            MouseAction::Scroll(MOUSE_WHEEL_LINES)
        );
        assert_eq!(
            route_mouse(plain, MouseKind::WheelDown),
            MouseAction::Scroll(-MOUSE_WHEEL_LINES)
        );
        assert_eq!(
            route_mouse(plain, MouseKind::Press(MouseButton::Left)),
            MouseAction::SelectStart
        );
        assert_eq!(
            route_mouse(plain, MouseKind::Drag(MouseButton::Left)),
            MouseAction::SelectUpdate
        );
        assert_eq!(
            route_mouse(plain, MouseKind::Release(MouseButton::Left)),
            MouseAction::SelectRelease
        );
        assert_eq!(
            route_mouse(plain, MouseKind::Press(MouseButton::Right)),
            MouseAction::Ignore
        );
    }

    #[test]
    fn route_mouse_non_sgr_mouse_app_falls_through_to_interpretation() {
        // A mouse-reporting app that never negotiated SGR is not sent garbage;
        // the mux interprets instead (Domain: SGR-only passthrough).
        let legacy = Modes {
            mouse_click: true,
            sgr_mouse: false,
            ..Modes::default()
        };
        assert_eq!(
            route_mouse(legacy, MouseKind::WheelUp),
            MouseAction::Scroll(MOUSE_WHEEL_LINES)
        );
    }

    #[test]
    fn sgr_mouse_bytes_encodes_button_coords_and_terminator() {
        // Left press at pane-local (row 4, col 9) -> SGR button 0, 1-based coords.
        let press = sgr_mouse_bytes(&MouseEvent {
            row: 4,
            col: 9,
            kind: MouseKind::Press(MouseButton::Left),
        });
        assert_eq!(press, b"\x1b[<0;10;5M");
        // Release terminates with lowercase m.
        let release = sgr_mouse_bytes(&MouseEvent {
            row: 4,
            col: 9,
            kind: MouseKind::Release(MouseButton::Left),
        });
        assert_eq!(release, b"\x1b[<0;10;5m");
        // Drag adds the motion bit (32).
        let drag = sgr_mouse_bytes(&MouseEvent {
            row: 0,
            col: 0,
            kind: MouseKind::Drag(MouseButton::Left),
        });
        assert_eq!(drag, b"\x1b[<32;1;1M");
        // Wheel up is button 64.
        let wheel = sgr_mouse_bytes(&MouseEvent {
            row: 2,
            col: 3,
            kind: MouseKind::WheelUp,
        });
        assert_eq!(wheel, b"\x1b[<64;4;3M");
    }
}
