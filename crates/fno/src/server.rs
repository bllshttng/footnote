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
use crate::backlog_view;
use crate::proto::{
    bind_or_probe, check_attach_version, err_code, read_msg, write_msg, AgentBadge, AgentRow,
    BacklogCard, BindOutcome, BlockDir, BlockSel, ClientMsg, Command, ControlVerb, Frame,
    MouseButton, MouseEvent, MouseKind, PaneInfo, ServerMsg, SquadMeta, TabMeta, WaitOutcome,
};
use crate::pty::{shell_candidates, PtyShell};
use crate::squad::{self, RemoveOutcome, Resolver, Session};
use crate::tree::{self, Axis, Node, Rect, Tab, TabId};
use crate::vt::BlockJumpOutcome;
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

/// Route by the pane's mode (brief Locked 2). A pane that negotiated SGR mouse
/// reporting gets passthrough, but only for the event kinds its mode actually
/// asked for: a click-only app (`?1000`) must not receive drag reports it never
/// requested, so a drag over such a pane is ignored (not mux-interpreted -
/// mux-interpreting a mouse app's pane would fight its own click handling). Only
/// SGR is honored - a mouse app that never negotiated SGR falls through to
/// interpretation rather than receiving garbage (Domain: legacy X10 truncates at
/// column 223). Known v1 limit: the client captures `?1002` (button + drag)
/// only, so a `?1003` all-motion app never sees hover motion.
fn route_mouse(modes: Modes, kind: MouseKind) -> MouseAction {
    let reports_mouse = modes.mouse_click || modes.mouse_drag || modes.mouse_motion;
    if reports_mouse && modes.sgr_mouse {
        let wants = match kind {
            // Wheel, press, and release are reported by every mouse mode.
            MouseKind::WheelUp
            | MouseKind::WheelDown
            | MouseKind::Press(_)
            | MouseKind::Release(_) => true,
            // Motion-while-held is only wanted by ?1002 (drag) / ?1003 (motion).
            MouseKind::Drag(_) => modes.mouse_drag || modes.mouse_motion,
        };
        return if wants {
            MouseAction::Passthrough
        } else {
            MouseAction::Ignore
        };
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

/// One block-navigation operation the core applies to a pane's OSC 133 store
/// (v8, x-38c4). Jump/select carry a walk direction; rerun re-sends the
/// selected block's command line under the idle guard.
#[derive(Debug, Clone, Copy)]
enum BlockNavOp {
    Jump(BlockDir),
    Select(BlockDir),
    Rerun,
}

/// One in-scrollback search operation the core applies to a pane (v12, x-e780).
/// Open carries the query (owned - the wire string); step carries the walk
/// direction (reusing [`BlockDir`]); clear drops the search. Each mutates the
/// shared pane and, on open/step, replies a `SearchResult` to the initiator.
#[derive(Debug, Clone)]
enum SearchOp {
    Open(String),
    Step(BlockDir),
    Clear,
}

/// The rerun idle guard (x-38c4), pure over the registry rows so the routing
/// (the safety-critical bit) is unit-testable without a Core. A pane with no
/// agent row is a plain shell - always safe. An agent pane must prove idle: a
/// `Done`/exited badge allows; a `Working`/`Blocked` badge refuses (busy); an
/// unknown badge (liveness-only, no fresh hook report / manifest verdict)
/// refuses fail-closed - injecting a command mid-turn corrupts an agent's
/// composer, and false-ready is the forbidden direction.
///
/// `agents` is the WHOLE cross-session registry, so the row match is scoped to
/// `session` on the FULL `(session, pane)` ref (the same filter `agent_rows`
/// applies): pane ids are minted per-server and collide across sessions, so a
/// pane-only match could read a foreign session's idle badge and let a rerun
/// into THIS session's busy agent (the exact forbidden write).
fn rerun_allowed(agents: &[RegistryAgent], session: &str, pane: u64) -> Result<(), &'static str> {
    match agents.iter().find(|a| {
        a.mux
            .as_ref()
            .is_some_and(|(s, p)| s == session && *p == pane)
    }) {
        None => Ok(()),
        Some(a) if a.exited => Ok(()),
        Some(a) => match a.badge {
            Some(AgentBadge::Done) => Ok(()),
            Some(AgentBadge::Working) | Some(AgentBadge::Blocked) => {
                Err("pane busy - rerun blocked")
            }
            None => Err("pane state unknown - rerun blocked"),
        },
    }
}

/// The last `n` non-empty lines of `text`, joined by `\n` - the mux-server twin
/// of the daemon's `Region::BottomNonEmptyLines` extraction (x-c929). The crates
/// share no code, so this is a focused copy (like `rfc3339_like_to_secs`); it
/// must stay byte-identical to the daemon's so an answer's region fingerprint
/// hashes the same on both sides.
fn bottom_non_empty_lines(text: &str, n: usize) -> String {
    let nonblank: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = nonblank.len().saturating_sub(n);
    nonblank[start..].join("\n")
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
    /// (v8) Walk a pane's OSC 133 blocks: jump the shared scroll or move the
    /// block-scoped selection. `id` is the requesting client (for a "no blocks"
    /// notice); the scroll/selection is shared, so the broadcast reaches every
    /// co-viewer.
    BlockNav {
        id: u64,
        pane: u64,
        op: BlockNavOp,
    },
    /// (v12, x-e780) In-scrollback search: open/step/clear a free-text find over
    /// the pane's server-side history. The scroll + highlight are shared (every
    /// co-viewer sees the jump); `id` is the initiator, who alone gets the
    /// `SearchResult` counter and any no-op/pane-not-found notice.
    Search {
        id: u64,
        pane: u64,
        op: SearchOp,
    },
    /// (v9, x-c929) Answer a blocked prompt: re-verify the region fingerprint
    /// against the live grid, then inject the daemon-pinned `keystroke`. `id`
    /// is the requesting client (for the stale/busy/closed notice).
    PaneAnswer {
        id: u64,
        pane: u64,
        fingerprint: [u8; 32],
        region_lines: u16,
        keystroke: Vec<u8>,
    },
    /// (v11, x-6f77) "Grab work" (leader+g): dispatch the next ready node into a
    /// new pane. `id` is the requesting client (for the outcome notice). The
    /// spawn runs OFF the core loop in a detached task (it shells `fno dispatch
    /// one`); the pane appears via the existing registry reader, and only the
    /// no-work / lanes-full / failure outcomes come back as `DispatchResult`.
    DispatchNext {
        id: u64,
    },
    /// The off-loop dispatch task's outcome, routed back so the notice is sent
    /// from the core loop (which owns `clients`). `notice` empty = say nothing
    /// (the launched pane speaks for itself via the layout push).
    DispatchResult {
        id: u64,
        notice: String,
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
    /// (x-6f77) A fresh board-ordered work-queue card set from the off-loop graph
    /// reader. Sent only when the set changed; the core stores it and re-pushes
    /// layouts so the sideline backlog lane tracks claims/closes.
    BacklogCards(Vec<BacklogCard>),
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
    /// An observer client (attached with rows==0 && cols==0, e.g. the web
    /// bridge): excluded from the smallest-client clamp so it never shrinks a
    /// PTY, and its `visible` set is EVERY live pane so the browser can pick
    /// any pane without an upstream message (x-6a14 read-only attach). Its
    /// `Resize` is ignored and it never spawns a squad.
    passive: bool,
}

struct PaneEntry {
    pty: PtyShell,
    vt: vt::Pane,
    /// The pane's `FNO_NODE` provenance (x-84a8), parsed from the `env(1)`
    /// wrapper prefix in the pane-run argv at spawn. `None` for a shell pane or
    /// an ad-hoc `pane run` with no `FNO_NODE=` token. Surfaced to the client
    /// status row via `Layout::focus_node`.
    node: Option<String>,
}

/// Extract the `FNO_NODE` value from a pane-run `argv`. The `_mesh_env_wrapper`
/// (mux_spawn.py) prefixes the command with `env FNO_NODE=<id> ...`, so the id
/// is already in the argv the server receives - no new IPC from the pane. An
/// ad-hoc pane (no such token) yields `None`.
///
/// Anchored to the `env(1)` wrapper to avoid false positives: only a leading
/// `env` followed by its `NAME=VALUE` assignment run is scanned, stopping at
/// the actual command. So a real command that merely mentions `FNO_NODE=` in
/// its own args (e.g. `grep FNO_NODE=x file`) is never mistaken for provenance.
fn node_from_argv(argv: &[String]) -> Option<String> {
    if argv.first().map(String::as_str) != Some("env") {
        return None;
    }
    argv.iter()
        .skip(1)
        .take_while(|a| a.contains('='))
        .find_map(|a| a.strip_prefix("FNO_NODE="))
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
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
    /// A clone of the core channel so an off-loop task (the leader+g dispatch
    /// shell-out) can route its outcome back as a `CoreMsg::DispatchResult`
    /// (x-6f77), the same off-loop-work-feeds-the-loop shape as the registry
    /// reader.
    self_tx: mpsc::Sender<CoreMsg>,
    /// Latest registry-derived agent rows (4a-G2), stored raw; the pane-exit
    /// fact and squad assignment are joined at layout time, where the live
    /// pane set and the squad catalog live.
    agents: Vec<RegistryAgent>,
    /// Latest board-ordered work-queue cards (x-6f77), from the off-loop graph
    /// reader; packed into every `Layout` for the sideline backlog lane.
    backlog: Vec<BacklogCard>,
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

/// Resolve the `fno` binary: `$FNO_BIN`, else the running executable itself (the
/// mux server IS the `fno` binary - it forwards non-native verbs like `dispatch`
/// to Python), else bare `fno` on PATH.
fn fno_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_BIN") {
        return PathBuf::from(v);
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fno"))
}

/// Shell `fno dispatch one --session <s> --json`, bounded + fail-open (the
/// digest_overlay idiom), and turn its verdict into the client notice. An empty
/// return says nothing (the launched pane speaks for itself); every error path
/// yields a visible notice rather than a silent no-op (x-6f77).
async fn run_dispatch_one(session: &str) -> String {
    // Selection + spawn crosses a subprocess and a mux socket round-trip, so the
    // budget is seconds, not the digest's 800ms; a hung dispatch still fails
    // open to a notice rather than wedging.
    const DISPATCH_TIMEOUT: Duration = Duration::from_secs(20);
    let fut = tokio::process::Command::new(fno_bin())
        .args(["dispatch", "one", "--mux-session", session, "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(DISPATCH_TIMEOUT, fut).await {
        Err(_) => "grab work: timed out".to_string(),
        Ok(Err(_)) => "grab work: dispatch unavailable".to_string(),
        // The porcelain ALWAYS prints its `--json` verdict to stdout, even on a
        // `failed` exit (code 1), so parse stdout whenever it is non-empty - the
        // JSON is the contract, not the exit code. `dispatch_notice` surfaces the
        // `detail` of a failed verdict; the exit status only distinguishes
        // "couldn't produce a verdict at all" (empty stdout).
        Ok(Ok(o)) => {
            let out = String::from_utf8_lossy(&o.stdout);
            if out.trim().is_empty() {
                "grab work: dispatch failed".to_string()
            } else {
                dispatch_notice(&out)
            }
        }
    }
}

/// Map a `fno dispatch one --json` verdict to the one-line client notice.
/// Unparseable / unknown output fails open to a generic failure notice (never
/// silent on an error).
fn dispatch_notice(stdout: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => return "grab work: dispatch failed".to_string(),
    };
    let node = v.get("node").and_then(|n| n.as_str()).unwrap_or("");
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap_or("");
    let label = if slug.is_empty() { node } else { slug };
    match v.get("outcome").and_then(|o| o.as_str()) {
        Some("launched") if label.is_empty() => String::new(),
        Some("launched") => format!("dispatched {label}"),
        Some("no-work") => "no ready work".to_string(),
        Some("lanes-full") => "lanes full".to_string(),
        // The node is already being dispatched/worked (same-node race loser or an
        // in-flight node) - a benign no-op, not a failure.
        Some("already-dispatching") if label.is_empty() => "already dispatching".to_string(),
        Some("already-dispatching") => format!("already dispatching {label}"),
        Some("failed") => match v.get("detail").and_then(|d| d.as_str()) {
            Some(d) if !d.is_empty() => format!("grab work failed: {d}"),
            _ => "grab work: dispatch failed".to_string(),
        },
        _ => "grab work: dispatch failed".to_string(),
    }
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
            // An observer (passive) client never enters the clamp reduce, so
            // a phone-sized viewer can never shrink a real client's PTY
            // (x-6a14 Locked Decision 4 / AC1-EDGE).
            .filter(|c| c.view.1 == tid && !c.passive)
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
        // A shell pane carries no node provenance (no wrapper argv).
        self.register_pane(id, pty, rows, cols, None);
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
        let node = node_from_argv(argv);
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
        self.register_pane(id, pty, rows, cols, node);
        Ok(id)
    }

    /// Record a freshly-spawned pane: bump the id, insert its VT grid, and
    /// arm its output watch (dropped receiver, so the watch costs nothing
    /// until a `PaneWait` subscribes).
    fn register_pane(
        &mut self,
        id: u64,
        pty: PtyShell,
        rows: u16,
        cols: u16,
        node: Option<String>,
    ) {
        self.next_pane_id += 1;
        self.panes.insert(
            id,
            PaneEntry {
                pty,
                vt: vt::Pane::new(rows, cols),
                node,
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

    /// "Grab work" (leader+g, x-6f77): dispatch the next ready backlog node into
    /// a new pane. Selection + claim + spawn is the Python porcelain's job (`fno
    /// dispatch one`), shelled OFF the core loop in a detached task so a slow
    /// backlog read never stalls a pane. The launched pane appears through the
    /// existing registry reader; the outcome (dispatched / no-work / lanes-full
    /// / failure) routes back as `DispatchResult` for a one-line notice.
    fn dispatch_next(&self, id: u64) {
        let session = self.session_name.clone();
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_dispatch_one(&session).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
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
        // An observer (web bridge) attaches (0,0). It must never create a
        // squad or a PTY (Locked Decision 5: read-only); it only watches. It
        // anchors to any existing squad's MRU tab for a sane `active_squad`
        // highlight - its frames come from `visible` = all panes, not `view`.
        let passive = rows == 0 && cols == 0;
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
            None if passive => {
                // Observer, no cwd match: anchor to the first squad's MRU tab,
                // or a (0,0) sentinel when the session has no squads yet (an
                // empty session - the browser shows its "no panes" placeholder;
                // TabId 0 is never minted so it is a safe dangling view).
                match self.session.squads.first() {
                    Some(sq) => {
                        let tid = sq
                            .tabs
                            .get(sq.active_tab)
                            .or_else(|| sq.tabs.first())
                            .expect("a squad always has a tab")
                            .id;
                        (sq.id, tid)
                    }
                    None => (0, 0),
                }
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
            passive,
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

    /// Whether `client_id` attached as an observer (`Attach { rows: 0, cols: 0 }`).
    /// A passive client is read-only at the server: any PTY/tree-mutating message
    /// from it is dropped (x-6a14 defense-in-depth - the read-only guarantee holds
    /// at the server, not only in the write-half-less web.rs bridge).
    fn is_passive(&self, client_id: u64) -> bool {
        self.clients.iter().any(|c| c.id == client_id && c.passive)
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

        // An observer client sees EVERY pane (x-6a14): its `visible` set is
        // all live panes so the browser can draw any pane the server broadcasts
        // without an upstream `View`. Precomputed here so the send loop can
        // hold `&mut self.clients` while reading it.
        let all_pane_ids: Vec<u64> = self.panes.keys().copied().collect();
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
            // An observer subscribes to all panes; a driving client to just
            // its viewed tab's rects.
            let frame_ids: Vec<u64> = if c.passive {
                all_pane_ids.clone()
            } else {
                rects.iter().map(|(pid, _)| *pid).collect()
            };
            c.visible = frame_ids.iter().copied().collect();
            if reemit {
                let mut d = c.dirty.lock().unwrap();
                for pid in &frame_ids {
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
            // The focused pane's provenance for the status row (x-66e8). Re-sent
            // whenever `focus` changes, so the cell tracks focus for free.
            focus_node: self.panes.get(&focus).and_then(|e| e.node.clone()),
            // The work-queue lane (x-6f77); already board-ordered by the reader.
            backlog: self.backlog.clone(),
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
                // An exited pane is unanswerable; drop any stale payload with
                // the badge (x-c929).
                answerable: if exited { None } else { a.answerable.clone() },
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
    /// Reliable: a dropped copy is silent data loss, and the copy is the only
    /// feedback that release-to-copy worked. A wedged reliable channel is a dead
    /// client (same policy as [`Core::push_layout`] / [`Core::sync_focused_modes`]):
    /// tear it down rather than lose the copy silently. A live client drains its
    /// reliable channel fast and never hits this.
    fn send_copy(&mut self, client_id: u64, text: String) {
        let Some(c) = self.clients.iter().find(|c| c.id == client_id) else {
            return;
        };
        if c.reliable_tx.try_send(ServerMsg::Copy { text }).is_err() {
            eprintln!("fno mux: client {client_id} reliable channel wedged on Copy; dropping it");
            self.clients.retain(|c| c.id != client_id);
            self.push_layout(true);
        }
    }

    /// Apply one block-navigation op to `pane` (v8, x-38c4). Jump moves the
    /// shared scroll and select moves the shared block selection - both broadcast
    /// so every co-viewer tracks (tmux precedent, brief). Rerun re-sends the
    /// selected block's command line, guarded idle. A pane with no blocks / no
    /// command / a busy pane gets a one-line notice to the requester, never a
    /// silent no-op.
    fn block_nav(&mut self, client_id: u64, pane: u64, op: BlockNavOp) {
        // One pane lookup per branch; a missing pane (client focus raced a pane
        // close) is a visible "pane not found", never a silent drop.
        match op {
            BlockNavOp::Jump(dir) => {
                match self.panes.get_mut(&pane).map(|e| e.vt.block_jump(dir)) {
                    Some(BlockJumpOutcome::Moved { .. }) | Some(BlockJumpOutcome::AtLive) => {
                        self.broadcast_pane(pane)
                    }
                    Some(BlockJumpOutcome::NoBlocks) => self.notice(client_id, "no command blocks"),
                    None => self.notice(client_id, "pane not found"),
                }
            }
            BlockNavOp::Select(dir) => {
                match self.panes.get_mut(&pane).map(|e| e.vt.block_select(dir)) {
                    Some(Some(_)) => self.broadcast_pane(pane),
                    Some(None) => self.notice(client_id, "no command blocks"),
                    None => self.notice(client_id, "pane not found"),
                }
            }
            BlockNavOp::Rerun => {
                if !self.panes.contains_key(&pane) {
                    self.notice(client_id, "pane not found");
                    return;
                }
                // Idle guard FIRST (false-ready is the forbidden direction):
                // refuse an agent pane that is not provably idle before the PTY.
                if let Err(reason) = self.pane_rerun_allowed(pane) {
                    self.notice(client_id, reason);
                    return;
                }
                // Rerun is human input, so honor the writer-claim interlock the
                // same as CoreMsg::Input: a live relay holder bounces with the
                // `busy: relay` notice (never inject into its in-flight write); a
                // dead holder releases here (AC3-FR), so rerun resumes.
                if let Some(&holder) = self.claims.get(&pane) {
                    if pid_alive(holder) {
                        self.notice(client_id, "busy: relay");
                        return;
                    }
                    self.claims.remove(&pane);
                }
                let cmd = self.panes.get(&pane).and_then(|e| e.vt.rerun_command());
                match cmd {
                    Some(mut line) => {
                        line.push('\r');
                        if let Some(entry) = self.panes.get(&pane) {
                            let _ = entry.pty.write_input(line.as_bytes());
                        }
                    }
                    None => self.notice(client_id, "block has no command to rerun"),
                }
            }
        }
    }

    /// Apply one in-scrollback search op to `pane` (v12, x-e780). Open/step mutate
    /// the shared scroll + highlight (broadcast so every co-viewer tracks, tmux
    /// precedent) and reply a `SearchResult` counter to the initiator ONLY; clear
    /// drops the highlight for all. A missing pane, or a step/clear with no active
    /// search, gets a one-line notice to the requester, never a silent no-op or a
    /// panic. Only match counts + coordinates ever leave the server.
    fn search_nav(&mut self, client_id: u64, pane: u64, op: SearchOp) {
        match op {
            SearchOp::Open(query) => {
                match self.panes.get_mut(&pane).map(|e| e.vt.search_open(&query)) {
                    Some((total, current)) => {
                        self.broadcast_pane(pane);
                        self.send_search_result(client_id, pane, total, current);
                    }
                    None => self.notice(client_id, "pane not found"),
                }
            }
            SearchOp::Step(dir) => match self.panes.get_mut(&pane).map(|e| e.vt.search_step(dir)) {
                Some(Some((total, current))) => {
                    self.broadcast_pane(pane);
                    self.send_search_result(client_id, pane, total, current);
                }
                Some(None) => self.notice(client_id, "no active search"),
                None => self.notice(client_id, "pane not found"),
            },
            SearchOp::Clear => match self.panes.get_mut(&pane) {
                Some(e) if e.vt.has_search() => {
                    e.vt.search_clear();
                    self.broadcast_pane(pane);
                }
                // Honor the documented no-op: clearing with nothing active is a
                // notice, not a stray broadcast (matches the Step arm).
                Some(_) => self.notice(client_id, "no active search"),
                None => self.notice(client_id, "pane not found"),
            },
        }
    }

    /// Reply the initiator-only `SearchResult` counter (v12). Reliable, like a
    /// `Copy`: the `[i/n]` chrome is the only signal the search landed, and a
    /// wedged reliable channel is a dead client (same teardown policy as
    /// [`Core::send_copy`]).
    fn send_search_result(&mut self, client_id: u64, pane_id: u64, total: u32, current: u32) {
        let Some(c) = self.clients.iter().find(|c| c.id == client_id) else {
            return;
        };
        if c.reliable_tx
            .try_send(ServerMsg::SearchResult {
                pane_id,
                total,
                current,
            })
            .is_err()
        {
            eprintln!(
                "fno mux: client {client_id} reliable channel wedged on SearchResult; dropping it"
            );
            self.clients.retain(|c| c.id != client_id);
            self.push_layout(true);
        }
    }

    /// Whether a rerun may write to `pane` (x-38c4 idle guard); see
    /// [`rerun_allowed`].
    fn pane_rerun_allowed(&self, pane: u64) -> Result<(), &'static str> {
        rerun_allowed(&self.agents, &self.session_name, pane)
    }

    /// Answer a blocked prompt without focusing the pane (x-c929). The freshness
    /// contract: re-read the pane's live bottom-N region, re-hash, and inject the
    /// daemon-pinned `keystroke` ONLY when the hash matches the `fingerprint` the
    /// operator read - so a picked answer can never land on a pane that advanced
    /// since the scrape (fail closed to focus). A foreign live writer-claim
    /// bounces (never inject under a relay's in-flight write). The re-read + send
    /// is atomic on the serial core loop: no other input interleaves for this
    /// pane between the hash check and the send (no in-server TOCTOU).
    ///
    /// Deliberately NOT the `rerun_allowed` idle-guard: that guard refuses a
    /// `blocked` pane, which is exactly the pane we answer. The fingerprint match
    /// IS the proof the pane is still at the prompt the human saw. The bytes sent
    /// are only ever the daemon-pinned `keystroke` (Locked 2), never fabricated.
    fn pane_answer(
        &mut self,
        client_id: u64,
        pane: u64,
        fingerprint: [u8; 32],
        region_lines: u16,
        keystroke: &[u8],
    ) {
        let Some(entry) = self.panes.get(&pane) else {
            self.notice(client_id, "pane closed - answer not sent");
            return;
        };
        // Re-read the SAME region the daemon fingerprinted: full grid text ->
        // bottom_non_empty_lines(region_lines). The daemon's `mux pane read
        // --json` returns this same frame_text, so the hashes agree iff the grid
        // is unchanged. Empty keystroke or an unhostable region can never produce
        // a match against a real prompt, so both fail closed here.
        let live = frame_text(&entry.vt.frame());
        let region = bottom_non_empty_lines(&live, region_lines as usize);
        if keystroke.is_empty() || *blake3::hash(region.as_bytes()).as_bytes() != fingerprint {
            self.notice(client_id, "prompt changed - focus to answer");
            return;
        }
        // Writer-claim interlock (same as rerun/Input): a live relay holder
        // bounces; a dead holder releases here so the answer resumes (AC3-FR).
        // Single map lookup via Entry; the notice is deferred past the borrow.
        let mut driven_by_relay = false;
        if let std::collections::hash_map::Entry::Occupied(e) = self.claims.entry(pane) {
            if pid_alive(*e.get()) {
                driven_by_relay = true;
            } else {
                e.remove();
            }
        }
        if driven_by_relay {
            self.notice(client_id, "driven by relay - focus to answer");
            return;
        }
        if let Some(entry) = self.panes.get(&pane) {
            let _ = entry.pty.write_input(keystroke);
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
            Command::CopySelection => {
                // Keyboard copy (leader+y): the focused pane's selection, else the
                // newest completed block (precedence + refusals in copy_source).
                // Nothing to copy is a plain notice; reuses the mouse-release channel.
                let text = self
                    .viewed_tab(view)
                    .and_then(|tab| self.panes.get(&tab.focus))
                    .and_then(|e| {
                        // block read is deferred: skipped entirely when a
                        // selection wins (it can clone up to a 256 KiB block).
                        copy_source(e.vt.selection_text(), || e.vt.read_block(BlockSel::Last))
                    });
                match text {
                    Some(text) => self.send_copy(client_id, text),
                    None => self.notice(client_id, "nothing selected"),
                }
                Flow::Continue
            }
        }
    }

    fn handle(&mut self, msg: CoreMsg) -> Flow {
        // Read-only enforcement at the server (x-6a14): drop any PTY/tree-mutating
        // message from an observer (passive) client, whatever sends it. The web
        // bridge holds no write half so it never sends these; this makes the
        // guarantee hold for ANY (0,0) attacher, not by the bridge's discipline
        // alone. Resize is already neutralized (its dims are ignored for a passive
        // client); Detach/Gone/Query/etc. are not mutations and pass through.
        let mutating_sender = match &msg {
            CoreMsg::Input { id, .. }
            | CoreMsg::Command { id, .. }
            | CoreMsg::Mouse { id, .. }
            | CoreMsg::BlockNav { id, .. }
            // Search moves the shared scroll + highlight for every co-viewer
            // (x-e780), the same shared-state mutation as BlockNav; a read-only
            // observer must never jump everyone's viewport.
            | CoreMsg::Search { id, .. }
            // PaneAnswer injects a keystroke into a pane PTY (x-c929); a
            // read-only observer must never reach it (same class as Input).
            | CoreMsg::PaneAnswer { id, .. }
            // DispatchNext spawns a real worker pane (x-6f77); a passive
            // web-bridge observer must never start work (x-6a14 Invariant).
            // DispatchResult is NOT gated here - it originates from the trusted
            // off-loop task, not a client.
            | CoreMsg::DispatchNext { id, .. } => Some(*id),
            _ => None,
        };
        if let Some(id) = mutating_sender {
            if self.is_passive(id) {
                return Flow::Continue;
            }
        }
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
                    // line (AC1-ERR, Invariant). No-op when already live. One
                    // lookup: broadcast after the mutable borrow ends.
                    let mut scrolled = false;
                    if let Some(e) = self.panes.get_mut(&focus) {
                        if e.vt.display_offset() != 0 {
                            e.vt.scroll_to_bottom();
                            scrolled = true;
                        }
                    }
                    if scrolled {
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
                    // An observer never drives geometry: ignore its (never-sent)
                    // Resize so it cannot enter the clamp (x-6a14 Invariant).
                    if !c.passive {
                        c.dims = (rows, cols);
                    }
                }
                self.push_layout(true);
                Flow::Continue
            }
            CoreMsg::Command { id, cmd } => self.command(id, cmd),
            CoreMsg::Mouse { id, pane, event } => {
                self.mouse(id, pane, event);
                Flow::Continue
            }
            CoreMsg::BlockNav { id, pane, op } => {
                self.block_nav(id, pane, op);
                Flow::Continue
            }
            CoreMsg::Search { id, pane, op } => {
                self.search_nav(id, pane, op);
                Flow::Continue
            }
            CoreMsg::PaneAnswer {
                id,
                pane,
                fingerprint,
                region_lines,
                keystroke,
            } => {
                self.pane_answer(id, pane, fingerprint, region_lines, &keystroke);
                Flow::Continue
            }
            CoreMsg::DispatchNext { id } => {
                self.dispatch_next(id);
                Flow::Continue
            }
            CoreMsg::DispatchResult { id, notice } => {
                if !notice.is_empty() {
                    self.notice(id, notice);
                }
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
            CoreMsg::BacklogCards(cards) => {
                // Same as AgentRows: only sideline data moved, so push the
                // Layout without a frame re-emit (x-6f77).
                self.backlog = cards;
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

/// The leader+y copy source precedence (epic Locked 6 / cv-4ac072b6): the active
/// `selection`, else the newest completed OSC 133 `block`. An open (still
/// streaming), truncated/evicted, or markerless-implicit block never copies -
/// `None` here makes the caller show the "nothing selected" notice rather than
/// land partial or wrong text. Both inputs are already validated by vt.
fn copy_source<F>(selection: Option<String>, block: F) -> Option<String>
where
    F: FnOnce() -> Result<vt::BlockRead, ()>,
{
    selection.or_else(|| match block() {
        Ok(b) if b.complete && !b.truncated && !b.implicit => Some(b.text),
        _ => None,
    })
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
        self_tx: core_tx.clone(),
        agents: Vec::new(),
        backlog: Vec::new(),
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

    // The off-loop work-queue reader (x-6f77): the same 1s mtime-gated shape as
    // the registry reader above, over ~/.fno/graph.json. graph.json's mtime
    // bumps on every backlog mutation (claim/close), so a card flips to
    // in-flight on the next tick with no separate subscribe stream. The 4M read
    // is skipped whenever the stamp is unchanged.
    {
        let core_tx = core_tx.clone();
        let path = backlog_view::graph_path();
        tokio::spawn(async move {
            let mut state = backlog_view::ReaderState::default();
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
                let changed = stamp != state.cached_stamp();
                let raw = if changed {
                    tokio::task::spawn_blocking(move || std::fs::read_to_string(&read_path).ok())
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                if let Some(cards) = state.tick(stamp, move || raw) {
                    if core_tx.send(CoreMsg::BacklogCards(cards)).await.is_err() {
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
            Ok(ClientMsg::BlockJump { pane, dir }) => {
                if core_tx
                    .send(CoreMsg::BlockNav {
                        id,
                        pane,
                        op: BlockNavOp::Jump(dir),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::BlockSelect { pane, dir }) => {
                if core_tx
                    .send(CoreMsg::BlockNav {
                        id,
                        pane,
                        op: BlockNavOp::Select(dir),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::BlockRerun { pane }) => {
                if core_tx
                    .send(CoreMsg::BlockNav {
                        id,
                        pane,
                        op: BlockNavOp::Rerun,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::SearchOpen { pane, query }) => {
                if core_tx
                    .send(CoreMsg::Search {
                        id,
                        pane,
                        op: SearchOp::Open(query),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::SearchStep { pane, dir }) => {
                if core_tx
                    .send(CoreMsg::Search {
                        id,
                        pane,
                        op: SearchOp::Step(dir),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::SearchClear { pane }) => {
                if core_tx
                    .send(CoreMsg::Search {
                        id,
                        pane,
                        op: SearchOp::Clear,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::PaneAnswer {
                pane,
                fingerprint,
                region_lines,
                keystroke,
            }) => {
                if core_tx
                    .send(CoreMsg::PaneAnswer {
                        id,
                        pane,
                        fingerprint,
                        region_lines,
                        keystroke,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::DispatchNext) => {
                if core_tx.send(CoreMsg::DispatchNext { id }).await.is_err() {
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
    fn node_from_argv_reads_the_wrapper_token() {
        // env(1) wrapper prefix: `env FNO_AGENT_SELF=... FNO_NODE=x-66e8 ... claude`.
        let argv: Vec<String> = [
            "env",
            "FNO_AGENT_SELF=peer",
            "FNO_NODE=x-66e8",
            "FNO_SLUG=some-slug",
            "claude",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(node_from_argv(&argv), Some("x-66e8".to_string()));
    }

    #[test]
    fn node_from_argv_is_none_for_ad_hoc_pane() {
        let ad_hoc =
            |a: &[&str]| node_from_argv(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // A plain `pane run htop` (no wrapper) has no provenance.
        assert_eq!(ad_hoc(&["htop"]), None);
        // An empty-valued token is treated as absent (no empty-string exports).
        assert_eq!(ad_hoc(&["env", "FNO_NODE=", "sh"]), None);
        // A command that merely MENTIONS FNO_NODE= in its own args is not
        // provenance: scanning stops at the command (first non-`NAME=` token).
        assert_eq!(
            ad_hoc(&["env", "FOO=1", "grep", "FNO_NODE=x", "file"]),
            None
        );
        // No `env` wrapper at all -> never scanned, even with a bare token.
        assert_eq!(ad_hoc(&["grep", "FNO_NODE=x", "file"]), None);
    }

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
    fn route_mouse_passes_through_kinds_the_app_requested() {
        // AC3-HP: a click-mode app (?1000) gets wheel/press/release passthrough
        // and consumes nothing mux-side...
        for kind in [
            MouseKind::WheelUp,
            MouseKind::Press(MouseButton::Left),
            MouseKind::Release(MouseButton::Left),
        ] {
            assert_eq!(route_mouse(mouse_mode(), kind), MouseAction::Passthrough);
        }
        // ...but a drag it never requested is ignored, not forwarded and not
        // mux-interpreted (fighting the app's own handling).
        assert_eq!(
            route_mouse(mouse_mode(), MouseKind::Drag(MouseButton::Left)),
            MouseAction::Ignore
        );
        // A drag-mode app (?1002) does get the drag.
        let drag_mode = Modes {
            mouse_drag: true,
            sgr_mouse: true,
            ..Modes::default()
        };
        assert_eq!(
            route_mouse(drag_mode, MouseKind::Drag(MouseButton::Left)),
            MouseAction::Passthrough
        );
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

    // -- Rerun idle guard (x-38c4) ---------------------------------------------

    fn agent_in(sess: &str, pane: u64, badge: Option<AgentBadge>, exited: bool) -> RegistryAgent {
        RegistryAgent {
            name: "w".into(),
            cwd: "/w".into(),
            exited,
            badge,
            reason: None,
            mux: Some((sess.into(), pane)),
            answerable: None,
        }
    }

    fn agent(pane: u64, badge: Option<AgentBadge>, exited: bool) -> RegistryAgent {
        agent_in("main", pane, badge, exited)
    }

    #[test]
    fn rerun_allowed_on_a_plain_shell_pane() {
        // AC-HP: a pane with no agent row is a shell - rerun is always safe.
        assert_eq!(rerun_allowed(&[], "main", 7), Ok(()));
        // Another agent on a different pane does not gate this one.
        assert_eq!(
            rerun_allowed(&[agent(9, Some(AgentBadge::Working), false)], "main", 7),
            Ok(())
        );
    }

    #[test]
    fn rerun_refused_for_a_busy_or_unknown_agent_pane() {
        // AC-ERR: false-ready is the forbidden direction - a working/blocked
        // agent pane refuses, and an unknown (liveness-only) badge fails closed.
        assert!(rerun_allowed(&[agent(7, Some(AgentBadge::Working), false)], "main", 7).is_err());
        assert!(rerun_allowed(&[agent(7, Some(AgentBadge::Blocked), false)], "main", 7).is_err());
        assert!(rerun_allowed(&[agent(7, None, false)], "main", 7).is_err());
    }

    #[test]
    fn rerun_allowed_for_an_idle_or_exited_agent_pane() {
        // A done agent (idle) or an exited one (the pane is a shell again) allows.
        assert_eq!(
            rerun_allowed(&[agent(7, Some(AgentBadge::Done), false)], "main", 7),
            Ok(())
        );
        assert_eq!(
            rerun_allowed(&[agent(7, Some(AgentBadge::Working), true)], "main", 7),
            Ok(())
        );
    }

    #[test]
    fn rerun_guard_is_scoped_to_the_current_session() {
        // Pane ids collide across sessions: a FOREIGN session's idle (Done) agent
        // on the same pane number must NOT clear the guard for THIS session's busy
        // agent - that would be the forbidden write into a working composer.
        let rows = [
            agent_in("other", 5, Some(AgentBadge::Done), false),
            agent_in("main", 5, Some(AgentBadge::Working), false),
        ];
        assert!(
            rerun_allowed(&rows, "main", 5).is_err(),
            "our busy agent must gate regardless of a foreign idle row on pane 5"
        );
        // And a foreign busy row must not spuriously gate our plain-shell pane.
        let foreign_only = [agent_in("other", 5, Some(AgentBadge::Working), false)];
        assert_eq!(rerun_allowed(&foreign_only, "main", 5), Ok(()));
    }

    // -- Observer attach (x-6a14 web read-only bridge) --------------------------

    fn empty_core() -> Core {
        let (out_tx, _out_rx) = mpsc::channel::<(u64, Vec<u8>)>(8);
        let (exit_tx, _exit_rx) = mpsc::channel::<u64>(8);
        let (self_tx, _self_rx) = mpsc::channel::<CoreMsg>(8);
        Core {
            session: Session::default(),
            panes: HashMap::new(),
            pane_watch: HashMap::new(),
            clients: Vec::new(),
            next_pane_id: 1,
            next_squad_id: 1,
            tab_areas: HashMap::new(),
            session_name: "test".into(),
            shells: Vec::new(),
            out_tx,
            exit_tx,
            self_tx,
            agents: Vec::new(),
            backlog: Vec::new(),
            claim_eligible: HashSet::new(),
            claims: HashMap::new(),
        }
    }

    fn client(id: u64, view_tab: TabId, dims: (u16, u16), passive: bool) -> Client {
        Client {
            id,
            reliable_tx: mpsc::channel(1).0,
            dirty: Arc::default(),
            notify: Arc::new(Notify::new()),
            synced_modes: Modes::default(),
            view: (1, view_tab),
            visible: HashSet::new(),
            dims,
            passive,
        }
    }

    #[test]
    fn observer_attach_excluded_from_clamp() {
        // AC1-EDGE: a passive (web) viewer must never enter the smallest-client
        // reduce, so it cannot shrink the driver's PTY - even with tiny dims.
        let mut core = empty_core();
        core.clients.push(client(1, 5, (24, 80), false)); // driving client
        core.clients.push(client(2, 5, (10, 30), true)); // phone observer
        assert_eq!(
            core.tab_area(5),
            (24, 80),
            "a passive viewer must not lower the driver's clamp"
        );
    }

    #[test]
    fn observer_attach_sole_viewer_keeps_last_or_default() {
        // AC1-EDGE: when the observer is the ONLY viewer, the tab keeps its
        // last-applied size (or VT defaults if never sized), never the phone's.
        let mut core = empty_core();
        core.clients.push(client(1, 5, (10, 30), true));
        assert_eq!(
            core.tab_area(5),
            (vt::DEFAULT_ROWS, vt::DEFAULT_COLS),
            "sole observer -> VT defaults, never its own dims"
        );
        core.tab_areas.insert(5, (40, 120));
        assert_eq!(
            core.tab_area(5),
            (40, 120),
            "sole observer -> last-applied size, never reflows to the phone"
        );
    }

    #[test]
    fn observer_attach_never_spawns_a_pane() {
        // Locked Decision 5: an observer (0,0) attach to a session it does not
        // match must register read-only without ever creating a squad/PTY.
        let mut core = empty_core();
        let (tx, _rx) = mpsc::channel::<ServerMsg>(8);
        core.attach(
            1,
            0,
            0,
            "/nowhere".into(),
            "/nowhere".into(),
            tx,
            Arc::default(),
            Arc::new(Notify::new()),
        );
        assert_eq!(
            core.panes.len(),
            0,
            "observer attach must never spawn a PTY"
        );
        assert_eq!(core.clients.len(), 1);
        assert!(core.clients[0].passive, "the (0,0) client is passive");
    }

    #[test]
    fn is_passive_flags_only_observer_clients() {
        let mut core = empty_core();
        core.clients.push(client(1, 5, (24, 80), false));
        core.clients.push(client(2, 5, (0, 0), true));
        assert!(!core.is_passive(1));
        assert!(core.is_passive(2));
        assert!(
            !core.is_passive(999),
            "an unknown id is not passive (its message is processed normally)"
        );
    }

    #[test]
    fn handle_drops_mutating_messages_from_a_passive_client() {
        let mut core = empty_core();
        core.clients.push(client(2, 5, (0, 0), true));
        // Read-only at the server: an observer's PTY/tree-mutating messages are
        // dropped by the guard before their handler body ever runs (x-6a14).
        assert!(matches!(
            core.handle(CoreMsg::Input {
                id: 2,
                bytes: vec![b'x']
            }),
            Flow::Continue
        ));
        assert!(matches!(
            core.handle(CoreMsg::BlockNav {
                id: 2,
                pane: 1,
                op: BlockNavOp::Rerun
            }),
            Flow::Continue
        ));
    }

    // -- Answer freshness (x-c929) ---------------------------------------------

    #[test]
    fn bottom_non_empty_lines_scopes_and_joins_like_the_daemon() {
        // The server twin must reproduce the daemon's Region::extract: blank
        // lines filtered, last N joined by '\n', line content untrimmed.
        let grid = "scrollback\n\nDo you want to proceed?\n  ❯ 1. Yes\n  2. No\n";
        assert_eq!(
            bottom_non_empty_lines(grid, 8),
            "scrollback\nDo you want to proceed?\n  ❯ 1. Yes\n  2. No"
        );
        // N smaller than the non-blank count scopes to the tail.
        assert_eq!(bottom_non_empty_lines(grid, 2), "  ❯ 1. Yes\n  2. No");
    }

    // The freshness contract: a fingerprint over the daemon-side region verifies
    // against the server's re-hash of the same grid (else every answer would
    // fail closed as stale - a false negative that breaks the feature), and a
    // grid that advanced hashes differently (the true-positive stale that keeps
    // an answer off a moved-on pane).
    #[test]
    fn answer_fingerprint_matches_unchanged_grid_and_rejects_advanced() {
        let grid = "scrollback\n\nDo you want to proceed?\n  ❯ 1. Yes\n  2. No\n";
        let daemon_fp = *blake3::hash(bottom_non_empty_lines(grid, 8).as_bytes()).as_bytes();
        // Server re-reads the identical grid -> same fingerprint (answer lands).
        let server_fp = *blake3::hash(bottom_non_empty_lines(grid, 8).as_bytes()).as_bytes();
        assert_eq!(daemon_fp, server_fp, "unchanged grid must verify");
        // The pane advanced (a new line appended) -> different fingerprint.
        let advanced = format!("{grid}Running the tool now...\n");
        let advanced_fp = *blake3::hash(bottom_non_empty_lines(&advanced, 8).as_bytes()).as_bytes();
        assert_ne!(daemon_fp, advanced_fp, "advanced grid must read stale");
    }

    #[test]
    fn dispatch_notice_maps_each_verdict() {
        // Launched shows the friendly slug; the pane itself is the real feedback.
        assert_eq!(
            dispatch_notice(r#"{"outcome":"launched","node":"x-1","slug":"feat"}"#),
            "dispatched feat"
        );
        // No slug -> fall back to the node id.
        assert_eq!(
            dispatch_notice(r#"{"outcome":"launched","node":"x-1","slug":""}"#),
            "dispatched x-1"
        );
        assert_eq!(dispatch_notice(r#"{"outcome":"no-work"}"#), "no ready work");
        assert_eq!(dispatch_notice(r#"{"outcome":"lanes-full"}"#), "lanes full");
        assert_eq!(
            dispatch_notice(r#"{"outcome":"failed","detail":"boom"}"#),
            "grab work failed: boom"
        );
        // Garbage / unknown outcome fails open to a visible notice, never silent.
        assert_eq!(dispatch_notice("not json"), "grab work: dispatch failed");
        assert_eq!(
            dispatch_notice(r#"{"outcome":"???"}"#),
            "grab work: dispatch failed"
        );
    }

    fn block(complete: bool, truncated: bool, implicit: bool, text: &str) -> vt::BlockRead {
        vt::BlockRead {
            seq: Some(0),
            exit: Some(0),
            complete,
            truncated,
            implicit,
            text: text.to_string(),
        }
    }

    #[test]
    fn copy_source_precedence_selection_then_block_then_none() {
        // AC-happy: an active selection wins even when a completed block exists.
        assert_eq!(
            copy_source(Some("sel".into()), || Ok(block(true, false, false, "blk"))),
            Some("sel".into())
        );
        // AC-happy: no selection -> the newest completed block copies.
        assert_eq!(
            copy_source(None, || Ok(block(true, false, false, "blk"))),
            Some("blk".into())
        );
        // AC-error: no selection and no block (BLOCK_UNAVAILABLE) -> notice.
        assert_eq!(copy_source(None, || Err(())), None);
    }

    #[test]
    fn copy_source_refuses_open_truncated_and_implicit_blocks() {
        // The open (still-running) block never copies.
        assert_eq!(
            copy_source(None, || Ok(block(false, false, false, "partial"))),
            None
        );
        // AC-edge: a truncated (head-evicted) block refuses rather than copy wrong text.
        assert_eq!(
            copy_source(None, || Ok(block(true, true, false, "trunc"))),
            None
        );
        // A markerless pane's implicit whole-output block keeps the old notice.
        assert_eq!(
            copy_source(None, || Ok(block(true, false, true, "whole"))),
            None
        );
    }
}
