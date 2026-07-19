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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use crate::agents_view::{self, RegistryAgent};
use crate::backlog_view;
use crate::proto::{
    bind_or_probe, check_attach_version, err_code, read_msg, write_msg, AgentBadge, AgentRow,
    BacklogCard, BindOutcome, BlockDir, BlockSel, CardState, ClientMsg, Command, ControlVerb,
    Frame, MouseButton, MouseEvent, MouseKind, PaneInfo, PaneMeta, PanePlacement, PaneTarget,
    ServerMsg, SquadMeta, TabMeta, WaitOutcome, MAX_SQUAD_NAME, MAX_TAB_NAME,
};
use crate::pty::{shell_candidates, PtyShell};
use crate::squad::{self, MoveTabOutcome, RemoveOutcome, Resolver, Session};
use crate::tree::{self, Axis, Dir, Node, Rect, Tab, TabId};
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
/// column 223). Known limit: the client now enables `?1003` too, but consumes
/// bare-motion (`MouseKind::Move`) LOCALLY for hover (focus-follows-mouse +
/// sideline highlight, x-a496) and never forwards it, so a pane app's own
/// `?1003` all-motion request still never sees hover motion - hover is a mux
/// affordance, not passthrough.
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
            // Bare motion is client-local hover (never forwarded); the arm is
            // dead but honest - only a ?1003 app would want it.
            MouseKind::Move => modes.mouse_motion,
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

/// Bound a folded wheel burst's net offset move to `cap` lines (one viewport)
/// so a high-event-rate trackpad flick lands a screen at a time instead of
/// teleporting hundreds of lines. `before`/`after` are the offsets around an
/// in-order fold, so the per-tick history clamp already happened - this only
/// caps the aggregate. A single mouse-wheel notch is one tick far under the
/// cap and passes through unchanged; the cap self-selects the trackpad burst.
fn bounded_scroll_target(before: i32, after: i32, cap: i32) -> i32 {
    before + (after - before).clamp(-cap, cap)
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
        // Bare motion: no-button code (3) plus the motion bit (32). Client-local
        // hover never forwards Move, so this arm is dead but wire-correct.
        MouseKind::Move => (3 + 32, false),
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
        /// (x-c914) The requesting client's session-local active account, so
        /// leader+g routes the spawn to it just like a targeted card click.
        account: Option<String>,
    },
    /// The off-loop dispatch task's outcome, routed back so the notice is sent
    /// from the core loop (which owns `clients`). `notice` empty = say nothing
    /// (the launched pane speaks for itself via the layout push).
    DispatchResult {
        id: u64,
        notice: String,
    },
    /// (v29, x-c376) The off-loop peek task's transcript, routed back so the
    /// `PeekBody` is sent from the core loop (which owns `clients`) to the
    /// requesting client only. `seq` echoes the request; error/timeout text
    /// travels in `lines`. Originates from a trusted server task, not a client,
    /// so it is NOT in the passive-observer mutation gate.
    PeekResult {
        id: u64,
        seq: u64,
        name: String,
        lines: Vec<String>,
    },
    /// (x-7561) A refreshed external-lifecycle record set from an off-loop
    /// external action (`claude stop|rm`) or the startup reconcile, routed back
    /// so the render snapshot update + layout push run on the core loop. `to`
    /// targets one client (an action outcome) or every client (`None`, the
    /// reconcile broadcast); `notices` are the bounded per-record messages.
    ExternalLifecycleSync {
        to: Option<u64>,
        records: Vec<crate::squad_store::ExternalLifecycle>,
        notices: Vec<String>,
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
        placement: PanePlacement,
        reply: ControlReply,
    },
    PaneSend {
        pane: u64,
        bytes: Vec<u8>,
        guarded: bool,
        /// Fresh registry snapshot for a guarded send, read off-loop in
        /// `handle_control`. `None` means either the read failed (guarded ->
        /// fail closed) or the send is unguarded (unused). `Some(rows)` is the
        /// idle authority the guard checks, `rows` empty => no agents => proceed.
        agents: Option<Vec<RegistryAgent>>,
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
    /// re-pushes layouts (rects unchanged, so no frame re-emit). `branches`
    /// (x-cd67 US4) is the reader's off-loop cwd -> git-branch resolution for
    /// the row cwds, joined into each row's `subline` at layout time; a cwd
    /// with no resolvable branch is simply absent (the subline degrades to the
    /// cwd tail).
    AgentRows {
        rows: Vec<RegistryAgent>,
        branches: HashMap<String, String>,
    },
    /// (x-6f77) A fresh board-ordered work-queue card set from the off-loop graph
    /// reader, claim-overlaid (x-54fa). Sent only when the set changed; the core
    /// stores it and re-pushes layouts so the sideline backlog lane tracks
    /// claims/closes. `holders` is the sweep's node-id -> claim-holder map,
    /// consumed at publish time for the `where_hint` of unroutable cards.
    BacklogCards {
        cards: Vec<BacklogCard>,
        holders: HashMap<String, String>,
        /// (x-9c5f) node id -> pr_number, from the same graph read as `cards`.
        prs: HashMap<String, u64>,
    },
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
    /// The spawn cwd, captured once so the tab-label derivation (x-c150) never
    /// touches the filesystem on the render path. Empty when the spawn fell
    /// back to the server cwd.
    cwd: String,
    /// Basename of the spawned command ("claude", "htop"), parsed from the
    /// pane-run argv like [`node_from_argv`]. `None` for a shell pane.
    cmd: Option<String>,
    /// (x-c914) The pane's birth claude account (`FNO_ACCOUNT`), parsed once at
    /// spawn. `None` = the default account. Drives the sideline account glyph
    /// for a mux-spawned pane; a durable pane fact (survives reattach), never
    /// the registry schema (Locked Decision 5).
    account: Option<String>,
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
    env_token_from_argv(argv, "FNO_NODE=")
}

/// (x-c914) The pane's `FNO_ACCOUNT` birth account, parsed from the same
/// `env(1)` wrapper prefix as `FNO_NODE` (`_mesh_env_wrapper` stamps it when a
/// spawn was routed with `--account`). `None` for a default-account or ad-hoc
/// pane. This is the mux-spawned-pane source for the sideline account glyph
/// (managed accounts share `~/.claude`, so the roster can't distinguish them -
/// the pane's own birth env can).
fn account_from_argv(argv: &[String]) -> Option<String> {
    env_token_from_argv(argv, "FNO_ACCOUNT=")
}

/// The argv index where the `env(1)` `NAME=VALUE` assignment run begins:
/// past `env` itself and its option run. `_mesh_env_wrapper` emits an auth-var
/// scrub (`-u VAR`) BEFORE the assignments on an `--account` spawn (x-c914), so
/// a naive "first token after env" scan would stop on `-u` and miss every
/// assignment (dropping both `FNO_NODE` and `FNO_ACCOUNT`). Skip `-u VAR` (and
/// `--unset VAR`) pairs, other `-flags`, and a `--` terminator. `None` when
/// argv doesn't start with `env`.
fn env_assignments_start(argv: &[String]) -> Option<usize> {
    if argv.first().map(String::as_str) != Some("env") {
        return None;
    }
    let mut i = 1;
    while let Some(tok) = argv.get(i).map(String::as_str) {
        if tok == "--" {
            i += 1;
            break;
        }
        if tok.starts_with('-') {
            // `-u`/`--unset` consumes the next token (the var name to unset).
            i += if tok == "-u" || tok == "--unset" {
                2
            } else {
                1
            };
        } else {
            break; // the assignment run (or the command) starts here
        }
    }
    Some(i)
}

/// Shared scan for a `NAME=` token in the leading `env(1)` assignment run of a
/// pane-run argv (anchored to `env` so a command that merely mentions the token
/// in its own args is never mistaken for provenance).
fn env_token_from_argv(argv: &[String], prefix: &str) -> Option<String> {
    let start = env_assignments_start(argv)?;
    argv[start..]
        .iter()
        .take_while(|a| a.contains('='))
        .find_map(|a| a.strip_prefix(prefix))
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// The spawned command's basename for the tab-label chain (x-c150): the first
/// argv token past an optional leading `env` + its `NAME=VALUE` run (the same
/// scan shape as [`node_from_argv`]). `None` when the scan finds no command -
/// spawn never fails on labeling.
fn cmd_from_argv(argv: &[String]) -> Option<String> {
    let cmd = match env_assignments_start(argv) {
        // Past the assignment run (skip `NAME=VALUE`s) is the command; the
        // option run was already skipped, so `-u` never masquerades as the cmd.
        Some(start) => argv[start..].iter().find(|a| !a.contains('='))?,
        None => argv.first()?,
    };
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    (!base.is_empty()).then(|| base.to_string())
}

#[cfg(test)]
thread_local! {
    /// Test override for the attach program (see [`attach_argv`]): points unit
    /// tests at a benign binary so the attach spawn+swap path runs without claude.
    static ATTACH_PROGRAM: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// The base argv attaching bg session `id`: `claude attach <id>`. `id` is always
/// a positional arg (never a shell string), so an 8-hex id can only name a
/// session. Tests override the program via `set_attach_program` (x-9f75).
fn attach_base(id: &str) -> Vec<String> {
    #[cfg(test)]
    if let Some(mut argv) = ATTACH_PROGRAM.with(|p| p.borrow().clone()) {
        argv.push(id.to_string());
        return argv;
    }
    vec!["claude".to_string(), "attach".to_string(), id.to_string()]
}

/// The argv attaching bg session `id`, routed to the right claude daemon. For an
/// isolated-account row (`config_dir` set), wrap with `env CLAUDE_CONFIG_DIR=<dir>`
/// so the attach hits that account's daemon instead of the ambient `~/.claude`
/// (codex P1: a bare `claude attach` under the default dir fails, or worse
/// targets a colliding default-account session); `FNO_ACCOUNT` rides along so the
/// re-attached pane keeps its account glyph. A default-account row passes `None`
/// and is byte-identical to the pre-feature attach.
fn attach_argv(
    id: &str,
    account: Option<&str>,
    config_dir: Option<&std::path::Path>,
) -> Vec<String> {
    let base = attach_base(id);
    let Some(dir) = config_dir else {
        return base;
    };
    let mut wrapped = vec![
        "env".to_string(),
        format!("CLAUDE_CONFIG_DIR={}", dir.display()),
    ];
    if let Some(a) = account {
        wrapped.push(format!("FNO_ACCOUNT={a}"));
    }
    wrapped.extend(base);
    wrapped
}

#[cfg(test)]
fn set_attach_program(argv: &[&str]) {
    ATTACH_PROGRAM.with(|p| *p.borrow_mut() = Some(argv.iter().map(|s| s.to_string()).collect()));
}

/// (x-c914) `short_id -> (account, config_dir)` for every isolated-account roster
/// worker, so restore can route a persisted isolated member's `claude attach` at
/// the right daemon (codex P1): at restore time `self.agents` is empty and the
/// stored member carries no account, so `attach_account_ctx` cannot resolve it -
/// this reverse lookup reads the isolated rosters directly. One-shot, read-only,
/// fail-open to empty.
fn isolated_attach_ctx() -> HashMap<String, (String, std::path::PathBuf)> {
    let mut map = HashMap::new();
    for (account, roster_path) in agents_view::isolated_roster_paths() {
        let Some(dir) = agents_view::account_config_dir(&account) else {
            continue;
        };
        if let Ok(raw) = std::fs::read_to_string(&roster_path) {
            for w in agents_view::parse_roster(&raw).into_iter().flatten() {
                map.insert(w.short_id, (account.clone(), dir.clone()));
            }
        }
    }
    map
}

/// A tab's display label (x-c150), from spawn-time facts only - no I/O, no
/// subprocess on the layout path (squad.rs's origin-freeze discipline).
/// Chain (Locked 1): explicit rename > `FNO_NODE` provenance > spawn-cwd
/// basename when it differs from the squad's > command basename > the bare
/// 1-based index (exactly the pre-x-c150 label, so a plain shell tab renders
/// unchanged). `pane` is the focused pane's `(node, cwd, cmd)`; `None` (a
/// reaped pane racing tree cleanup) falls through to the index - the
/// derivation never panics on a missing pane.
fn tab_label(
    rename: Option<&str>,
    pane: Option<(Option<&str>, &str, Option<&str>)>,
    squad_cwd: &str,
    i: usize,
) -> String {
    if let Some(name) = rename {
        return name.to_string();
    }
    if let Some((node, cwd, cmd)) = pane {
        // Every derived candidate is sanitized like a rename (codex peer
        // review): FNO_NODE values, dir names, and argv all admit control
        // bytes, and these strings land in chrome cells. A candidate that
        // sanitizes to empty (e.g. whitespace-only) falls through to the
        // next source instead of rendering a blank label.
        if let Some(node) = node {
            let clean = sanitize_tab_name(node);
            if !clean.is_empty() {
                return clean;
            }
        }
        fn base(p: &str) -> &str {
            p.trim_end_matches('/').rsplit('/').next().unwrap_or("")
        }
        let cwd_base = base(cwd);
        if !cwd_base.is_empty() && cwd_base != base(squad_cwd) {
            let clean = sanitize_tab_name(cwd_base);
            if !clean.is_empty() {
                return clean;
            }
        }
        if let Some(cmd) = cmd {
            let clean = sanitize_tab_name(cmd);
            if !clean.is_empty() {
                return clean;
            }
        }
    }
    (i + 1).to_string()
}

/// A pane's display label for the session navigator (v22, x-653d). Unlike
/// [`tab_label`] (which prefers a dir name so a tab reads as its worktree), a
/// pane's discriminator WITHIN a tab is what it is running, so `cmd` leads:
/// `cmd` -> `node` -> cwd basename -> `shell`. Sanitized like a wire name (these
/// land in chrome cells). Never an ordinal - a plain pane is `shell`, not a
/// number the operator cannot map back.
fn pane_label(node: Option<&str>, cwd: &str, cmd: Option<&str>) -> String {
    for cand in [cmd, node] {
        if let Some(c) = cand {
            let clean = sanitize_tab_name(c);
            if !clean.is_empty() {
                return clean;
            }
        }
    }
    let base = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let clean = sanitize_tab_name(base);
    if clean.is_empty() {
        "shell".to_string()
    } else {
        clean
    }
}

/// Sanitize a wire-supplied name: strip control characters (they would corrupt
/// chrome cells), trim, cap at `cap` chars. The cap lives HERE and not only in
/// the overlay: `Command` is a wire surface, and the TUI is not the only
/// client. Empty-after-sanitize means "clear".
fn sanitize_name(raw: &str, cap: usize) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().chars().take(cap).collect()
}

/// Tab-name sanitize (x-c150), capped at [`MAX_TAB_NAME`].
fn sanitize_tab_name(raw: &str) -> String {
    sanitize_name(raw, MAX_TAB_NAME)
}

/// Whether an event ended the session.
#[derive(PartialEq)]
enum Flow {
    Continue,
    Shutdown,
}

/// Unlink the socket AND its wire-version sidecar (x-1a85) on every exit path
/// out of `run` (a SIGKILL leaves them behind by design; the stale-socket path
/// in `bind_or_probe` covers that, and a lingering `.ver` is inert - `ls` only
/// reads it for a LIVE server, and a dead one probes `Stale`).
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(crate::proto::version_sidecar_path(&self.0));
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

    // Stamp this server's wire version next to its socket (x-1a85) so `fno mux
    // ls` can flag a stale-wire server after a binary upgrade. Best-effort: a
    // write failure only means `ls` reads no version and treats the server as
    // stale (conservative - a spurious restart, never a missed skew), so it must
    // never abort the server.
    if let Err(e) = std::fs::write(
        crate::proto::version_sidecar_path(&socket),
        crate::proto::PROTO_VERSION.to_string(),
    ) {
        eprintln!("fno mux: warn: could not write version sidecar: {e}");
    }

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
    /// (x-cd67 US4) Latest cwd -> git-branch map from the off-loop reader,
    /// joined into each agent row's `subline` at layout time. A cwd absent from
    /// the map has no resolvable branch (non-git dir, unreadable HEAD); the
    /// subline then degrades to the cwd tail. Display-only, so staleness across
    /// a git checkout is cosmetic.
    branch_by_cwd: HashMap<String, String>,
    /// Latest board-ordered work-queue cards (x-6f77), from the off-loop graph
    /// reader; packed into every `Layout` for the sideline backlog lane.
    backlog: Vec<BacklogCard>,
    /// Claim holder per in-flight node id (x-54fa), from the reader's sweep;
    /// joined at publish time into card routes / `where_hint`.
    backlog_holders: HashMap<String, String>,
    /// (x-9c5f) node id -> pr_number, from the off-loop graph reader; joined at
    /// layout time (holder name -> node -> pr) into `AgentRow.pr` for the peek
    /// header's `PR #N` label.
    backlog_pr: HashMap<String, u64>,
    /// Panes spawned claim-ELIGIBLE (`pane run --claim`, agent panes). A
    /// general pane never appears here and never consults a claim (Locked 5).
    claim_eligible: HashSet<u64>,
    /// Held writer claims: pane -> holder pid. Enforced on `Input` as an
    /// in-memory lookup + a `kill(pid, 0)` liveness probe (one syscall, never
    /// a subprocess - the origin freeze class); a dead holder releases lazily
    /// on the next contested keystroke, so typing resumes without a server
    /// restart (AC3-FR) and no sweep timer exists to tune.
    claims: HashMap<u64, u32>,
    /// Per-pane last `human_touch(inject)` emit time (W4 touch telemetry):
    /// at most one emit per pane per [`TOUCH_COALESCE_WINDOW`], so a typing
    /// burst is one steering action, not a per-keystroke fork storm. Purged
    /// with the pane in [`Core::reap_pane`].
    touch_last_emit: HashMap<u64, Instant>,
    /// (x-9454) Per-pane wheel-passthrough rate gate: bounds how many wheel
    /// ticks per window reach a mouse-owning pane's PTY, so a trackpad flood
    /// stops scrolling when the finger stops instead of draining stale ticks.
    /// Purged with the pane in [`Core::reap_pane`], the `touch_last_emit`
    /// pattern.
    wheel_gate: HashMap<u64, WheelGateState>,
    /// Failed `human_touch` emits (AC4-ERR): counted, never raised to the
    /// steering path. An inflated autonomy rate is the dangerous silent
    /// failure, so the count exists even before the scoreboard reads it.
    touch_emit_failures: Arc<AtomicU64>,
    /// Attached-client count for the periodic readers (x-4e30). Published
    /// from choke points (tail of `handle` + the main-loop tail), never
    /// per mutation site: `clients` mutates in six places and per-site
    /// stores drift on the next refactor. A `watch`, not an atomic,
    /// because the readers park in `tick().await` and need the
    /// `changed()` edge as the 0->1 wakeup.
    client_count: watch::Sender<usize>,
    /// (x-4328) Pane ids the operator has focused while badged `Done`.
    /// Inserted as a one-shot side effect of an actual focus action
    /// (`Command::FocusPane`, via [`Core::mark_seen_if_done`]) when that
    /// pane is currently `Done`; evicted level-triggered every layout pass
    /// the instant a pane's badge leaves `Done` (a re-run re-arms unseen,
    /// and never self-reinserts merely by remaining the focused pane -
    /// AC1-EDGE/AC2-EDGE). Reattach-durable for free - `Core` survives a
    /// client detach/reattach - but not server-restart (a cold-scrape
    /// non-goal, Locked Decision 7). Orphan ids from reaped panes are inert
    /// (never re-matched); no GC.
    seen: HashSet<u64>,
    /// (x-0090) Live attach panes: `attach_id -> pane`. Lifetime = pane
    /// lifetime, never persisted (server death kills panes; the bg agent
    /// re-surfaces watch-only next session). Lets `AttachAgent` reconcile
    /// row-to-tab identity: a second attach for a mapped id focuses the
    /// existing pane instead of minting a duplicate tab, and `agent_rows()`
    /// presents the mapped watch-only row pane-hosted. Swept eagerly on pane
    /// teardown; `agent_rows()` also checks `panes` liveness lazily so a stale
    /// entry can never present a dead pane.
    attached: HashMap<String, u64>,
    /// (x-8f11) Durable membership of each PERSISTED named squad: squad id ->
    /// its recruited members (attach-ids + tombstone bits). Populated only by
    /// `NewSquad`, `RecruitAgents`, and restore; presence here is what marks a
    /// squad persistent (an attach-born origin squad is absent and never
    /// written). Written through to `~/.fno/squads.json` on every membership
    /// mutation. Keyed by session-scoped id, so a removed squad's entry is
    /// inert (ids never reused; no GC - ponytail: a dead-sid leak is one small
    /// map entry per closed workspace per session, bounded by session length).
    squad_members: HashMap<u64, Vec<crate::squad_store::StoredMember>>,
    /// (x-7561) Machine-global external-row lifecycle tombstones the sideline
    /// renders (stopped -> exited `x`-removable; failed/unknown/stopping/removing
    /// -> `!exited` with an in-flight reason). Loaded at restore, refreshed after
    /// every external action and the startup reconcile. The durable truth is
    /// `squads.json`'s `external_lifecycle`; this is the render snapshot.
    external_lifecycle: Vec<crate::squad_store::ExternalLifecycle>,
    /// (x-8f11) Latch for the one-shot "persistence degraded" notice (AC3-ERR):
    /// a store-write failure notices every client exactly once, then stays
    /// silent so a full disk never spams a bell per keystroke.
    persist_degraded_notified: bool,
    /// (x-8f11) First-attach restore fires once per server lifetime; this gates
    /// it so a second client attach does not re-materialize the persisted
    /// squads.
    restored: bool,
}

/// At most one `human_touch(inject)` per pane per window: the first keystroke
/// of a burst means "operator started steering this pane".
/// ponytail: fixed 5s window; tune only if real bursts split.
const TOUCH_COALESCE_WINDOW: Duration = Duration::from_secs(5);

/// Whether an inject emit should fire now for `pane` (recording `now`), or be
/// coalesced into the burst whose start time is already stored.
fn touch_coalesce(last: &mut HashMap<u64, Instant>, pane: u64, now: Instant) -> bool {
    match last.entry(pane) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            // saturating: a `now` behind the stored instant (clock quirks
            // under virtualization) coalesces instead of panicking.
            if now.saturating_duration_since(*e.get()) < TOUCH_COALESCE_WINDOW {
                false
            } else {
                e.insert(now);
                true
            }
        }
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(now);
            true
        }
    }
}

/// Wheel-passthrough rate gate (x-9454): forward at most [`WHEEL_GATE_BUDGET`]
/// wheel ticks per [`WHEEL_GATE_WINDOW`] to a mouse-owning pane's PTY. A
/// physical notch stream (a few ticks/s) never gates; only a trackpad flood
/// (hundreds/s) clips. 12 ticks / 100ms is a ~120 ticks/s ceiling.
/// ponytail: too low and fast-redrawing apps (vim) feel sluggish - raise the
/// budget if a deliberate notch scroll ever drops.
const WHEEL_GATE_WINDOW: Duration = Duration::from_millis(100);
const WHEEL_GATE_BUDGET: u32 = 12;

/// Per-pane wheel-gate state: the current window's start, ticks forwarded in
/// it, and the direction of the last forwarded tick (a reversal is fresh
/// intent and resets the window - brief Locked 3).
#[derive(Debug)]
struct WheelGateState {
    window_start: Instant,
    count: u32,
    dir: MouseKind,
}

/// Whether a wheel `dir` tick for `pane` should be forwarded now, recording
/// `now`. Pure over the state map (injected `Instant`, `touch_coalesce`
/// pattern) so it is PTY-free unit-testable. Rules: a direction reversal or an
/// expired window (at or after the boundary instant - no permanent mute,
/// AC1-FR) resets to a fresh budget and allows; under budget allows; else
/// drops. Drops only - forwarded ticks keep arrival order (brief Locked 5).
fn wheel_gate(
    gate: &mut HashMap<u64, WheelGateState>,
    pane: u64,
    dir: MouseKind,
    now: Instant,
) -> bool {
    match gate.entry(pane) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let st = e.get_mut();
            // saturating: a `now` behind window_start (virtualized clock skew)
            // treats the tick as inside the window instead of panicking.
            if st.dir != dir || now.saturating_duration_since(st.window_start) >= WHEEL_GATE_WINDOW
            {
                *st = WheelGateState {
                    window_start: now,
                    count: 1,
                    dir,
                };
                true
            } else if st.count < WHEEL_GATE_BUDGET {
                st.count += 1;
                true
            } else {
                false
            }
        }
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(WheelGateState {
                window_start: now,
                count: 1,
                dir,
            });
            true
        }
    }
}

/// The set of attach-ids live NOW, from the raw registry + roster contents
/// (x-8f11). Pure so restore's liveness read is unit-testable without files or
/// env, like `agents_view::derive_rows`: a non-exited registry row's
/// `attach_id` and every roster worker's `short_id` are live; an exited row is
/// not.
fn live_ids_from(reg_raw: Option<&str>, roster_raw: Option<&str>, now: u64) -> HashSet<String> {
    let mut live = HashSet::new();
    if let Some(raw) = reg_raw {
        for a in agents_view::derive_rows(raw, now).into_iter().flatten() {
            if !a.exited {
                if let Some(id) = a.attach_id {
                    live.insert(id);
                }
            }
        }
    }
    if let Some(raw) = roster_raw {
        for w in agents_view::parse_roster(raw).into_iter().flatten() {
            live.insert(w.short_id);
        }
    }
    live
}

/// Loose `<prefix>-<hex4..8>` node-id shape check for the cwd-basename
/// fallback, so a plain shell squad (basename "footnote") is never
/// mis-attributed as a graph node.
fn node_id_shaped(s: &str) -> bool {
    match s.split_once('-') {
        Some((prefix, hex)) => {
            !prefix.is_empty()
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && (4..=8).contains(&hex.len())
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        }
        None => false,
    }
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
pub(crate) fn fno_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_BIN") {
        return PathBuf::from(v);
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fno"))
}

/// Shell `fno dispatch one --session <s> --json`, bounded + fail-open (the
/// digest_overlay idiom), and turn its verdict into the client notice. An empty
/// return says nothing (the launched pane speaks for itself); every error path
/// yields a visible notice rather than a silent no-op (x-6f77).
async fn run_dispatch_one(session: &str, node: Option<&str>, account: Option<&str>) -> String {
    // Selection + spawn crosses a subprocess and a mux socket round-trip, so the
    // budget is seconds, not the digest's 800ms; a hung dispatch still fails
    // open to a notice rather than wedging.
    const DISPATCH_TIMEOUT: Duration = Duration::from_secs(20);
    // A targeted node (a clicked work-queue card, x-a496) pins `--node`; without
    // it the porcelain picks the board's next ready node (leader+g). The claim
    // race, lane cap, and verdict shape are identical either way.
    let mut args = vec!["dispatch", "one", "--mux-session", session, "--json"];
    if let Some(n) = node {
        args.push("--node");
        args.push(n);
    }
    // (x-c914) The client's session-local active account, resolved to the
    // spawn's `--account` overlay CLI-side (x-d012 owns the resolver + the
    // stale-account refusal); the mux only forwards the id.
    if let Some(a) = account {
        args.push("--account");
        args.push(a);
    }
    let fut = tokio::process::Command::new(fno_bin())
        .args(&args)
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

/// Shell `fno-agents <verb> <name>` for a sideline lifecycle gesture (x-76ea),
/// bounded + fail-open (the `run_dispatch_one` idiom): a short outcome notice,
/// never a wedge. The registry poll owns the row's truth, so a lost/failed
/// notice degrades to "the row updates a beat later or stays put", not a silent
/// mutation. `verb` is always a fixed literal; the argv is never a shell string.
async fn run_agent_action(verb: &str, name: &str) -> String {
    const AGENT_ACTION_TIMEOUT: Duration = Duration::from_secs(20);
    let fut = tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())
        .args([verb, name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    let past = if verb == "stop" { "stopped" } else { "removed" };
    match tokio::time::timeout(AGENT_ACTION_TIMEOUT, fut).await {
        Err(_) => format!("{verb} {name}: timed out"),
        Ok(Err(_)) => format!("{verb} {name}: unavailable"),
        Ok(Ok(status)) if status.success() => format!("{past} {name}"),
        Ok(Ok(_)) => format!("{verb} {name}: failed"),
    }
}

/// (x-9c5f) Sanitize peek-overlay free-text mail: strip control chars, trim,
/// refuse blank-after-sanitize and over-`MAX_MAIL_TEXT` (never truncate - a
/// silently cut instruction to a worker is worse than a visible refusal, Locked
/// Decision 7). The count is chars, matching the client's printable-ASCII cap.
fn sanitize_mail_text(text: &str) -> Result<String, String> {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return Err("message is empty".to_string());
    }
    if clean.chars().count() > crate::proto::MAX_MAIL_TEXT {
        return Err("message too long".to_string());
    }
    Ok(clean.to_string())
}

/// (x-9c5f) Whether `s` is a lowercase 8-4-4-4-12 hex uuid (the respawn shape
/// gate, the AttachAgent jobId precedent): a malformed value never reaches
/// `spawn --resume`'s argv.
fn valid_session_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == groups.len()
        && parts.iter().zip(groups).all(|(p, n)| {
            p.len() == n
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// (x-9c5f) First non-empty line of `s` with control chars stripped, else
/// `fallback`. Subprocess stdout/stderr becomes an operator-visible notice, so
/// raw ANSI/C0 must never reach the status line (Domain Pitfall: route stderr
/// through the same strip the peek body uses).
fn first_line_or(s: &str, fallback: &str) -> String {
    s.lines()
        .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// (x-9c5f) Shell `fno mail send <name> <text>` off-loop, bounded + capturing:
/// the CLI's one-line stdout verdict (`msg-<id> delivered|queued`) becomes the
/// notice verbatim; a nonzero exit surfaces the first stderr line. Never silent
/// (Locked Decision 6). Uses the `fno` porcelain; argv array only.
async fn run_mail_send(name: &str, text: &str) -> String {
    const MAIL_TIMEOUT: Duration = Duration::from_secs(20);
    // `--` ends option parsing so operator text starting with `-` (e.g. a reply
    // of `--help`) is delivered as the message, not consumed as a CLI flag.
    let fut = tokio::process::Command::new(fno_bin())
        .args(["mail", "send", "--", name, text])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(MAIL_TIMEOUT, fut).await {
        Err(_) => format!("mail {name}: timed out"),
        Ok(Err(_)) => format!("mail {name}: unavailable"),
        Ok(Ok(o)) if o.status.success() => first_line_or(
            &String::from_utf8_lossy(&o.stdout),
            &format!("mailed {name}"),
        ),
        Ok(Ok(o)) => first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            &format!("mail {name}: failed"),
        ),
    }
}

/// (x-9c5f) Shell `fno agents spawn <name> --resume <uuid> --substrate bg`
/// off-loop for the peek `r` respawn: a longer 60s bound (a bg spawn creates a
/// thread; 20s is too tight). Success -> `respawned <name>`; failure -> the
/// first stderr line. The 1s registry poll owns the row flipping live; this
/// notice is advisory. Uses the `fno` porcelain, NOT `fno-agents`: the Rust
/// runtime intercepts `agents spawn` and routes it (unlike stop/rm, which use
/// the `fno-agents` binary deliberately).
///
/// `cwd` + `account` come from the registry row, NOT the `--resume` uuid:
/// `fno agents spawn` defaults `--cwd` to the CANONICAL checkout (x-85fe), so a
/// worker revived from a feature worktree would land in main without `--cwd`;
/// an isolated-account session's uuid lives in that account's config dir, so it
/// needs `--account` to be found. Both are omitted when empty/absent (the
/// pre-existing default, correct for a canonical/default-account worker).
async fn run_respawn(name: &str, uuid: &str, cwd: &str, account: Option<&str>) -> String {
    const RESPAWN_TIMEOUT: Duration = Duration::from_secs(60);
    // Pin `--provider claude`: respawn is definitionally a claude revival (the
    // uuid is carried only for claude rows), but `fno agents spawn` otherwise
    // infers the provider from the invoking harness, so a mux server running
    // under a non-claude context would infer the wrong provider and fail the
    // claude-only `--resume` guard.
    let mut args: Vec<&str> = vec![
        "agents",
        "spawn",
        name,
        "--provider",
        "claude",
        "--resume",
        uuid,
        "--substrate",
        "bg",
    ];
    if !cwd.is_empty() {
        args.push("--cwd");
        args.push(cwd);
    }
    if let Some(acct) = account.filter(|a| !a.is_empty()) {
        args.push("--account");
        args.push(acct);
    }
    let fut = tokio::process::Command::new(fno_bin())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(RESPAWN_TIMEOUT, fut).await {
        Err(_) => format!("respawn {name}: timed out"),
        Ok(Err(_)) => format!("respawn {name}: unavailable"),
        Ok(Ok(o)) if o.status.success() => format!("respawned {name}"),
        Ok(Ok(o)) => first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            &format!("respawn {name}: failed"),
        ),
    }
}

/// Shell `fno agents peek <name> -n 20` for the sideline peek overlay (x-c376),
/// bounded + fail-open: the captured lines (stdout, else stderr, else a
/// synthesized one-liner) become the overlay body verbatim. `fno agents peek`
/// reads the peer's on-disk transcript, so it works on a suspended/watch-only
/// worker without spawning anything; it is read-only (never writes what the peer
/// reads). Every failure path yields a visible body line, never an empty result:
/// the overlay renders whatever comes back and never closes on a fetch error
/// (AC1-ERR, AC2-FR). `name` was resolved from the client's own `Layout`; the
/// argv is never a shell string, so the value can only be `peek`'s positional.
async fn run_agent_peek(name: &str) -> Vec<String> {
    // A transcript read crosses a subprocess (disk tail); a hung read (locked
    // file, dead NFS) is killed at the timeout and surfaces a timeout line
    // rather than wedging the overlay on "loading…" forever.
    const PEEK_TIMEOUT: Duration = Duration::from_secs(5);
    const PEEK_LINES: &str = "20";
    let fut = tokio::process::Command::new(fno_bin())
        .args(["agents", "peek", name, "-n", PEEK_LINES])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let body = match tokio::time::timeout(PEEK_TIMEOUT, fut).await {
        Err(_) => format!("peek timed out ({}s)", PEEK_TIMEOUT.as_secs()),
        Ok(Err(_)) => "peek unavailable (fno not on server PATH?)".to_string(),
        Ok(Ok(o)) => {
            // Exit 13 (unknown peer) / exit 1 (no reader) print their reason to
            // stderr; a clean read prints the transcript (or "no activity yet")
            // to stdout. Prefer stdout when non-empty, else stderr, so an error
            // reason is never dropped for a blank body.
            let out = String::from_utf8_lossy(&o.stdout);
            if out.trim().is_empty() {
                let err = String::from_utf8_lossy(&o.stderr);
                if err.trim().is_empty() {
                    "no activity yet".to_string()
                } else {
                    err.into_owned()
                }
            } else {
                out.into_owned()
            }
        }
    };
    body.lines().map(str::to_string).collect()
}

/// Shell `fno-agents reap --json` once for the bulk-reap gesture (x-7561),
/// bounded + fail-open like [`run_agent_action`]: on success parse the `reaped`
/// array length into a visible `reaped N` count (zero is a successful `reaped
/// 0`), else a bounded failure notice. The argv is a fixed literal.
async fn run_reap() -> String {
    const REAP_TIMEOUT: Duration = Duration::from_secs(20);
    let fut = tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())
        .args(["reap", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(REAP_TIMEOUT, fut).await {
        Err(_) => "reap: timed out".to_string(),
        Ok(Err(_)) => "reap: unavailable".to_string(),
        Ok(Ok(out)) if out.status.success() => reap_notice(&String::from_utf8_lossy(&out.stdout)),
        Ok(Ok(_)) => "reap: failed".to_string(),
    }
}

/// Shell `claude <verb> <attach_id>` for an external lifecycle action (x-7561):
/// `stop` preserves the conversation, `rm` deletes the session + worktree
/// (Domain Pitfall 2 - they are not interchangeable). Bounded + argv-safe (the
/// id is 8-hex validated at load, never a shell string). Returns `(ok, reason)`:
/// the caller's `complete_external` maps `ok` to stopped/removed vs failed.
async fn run_claude_lifecycle(
    verb: &'static str,
    attach_id: &str,
    config_dir: Option<std::path::PathBuf>,
) -> (bool, Option<String>) {
    const CLAUDE_TIMEOUT: Duration = Duration::from_secs(20);
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([verb, attach_id]);
    // (x-c914) Route the lifecycle action at the row's own daemon: an isolated
    // account lives in its own CLAUDE_CONFIG_DIR, so a bare `claude stop|rm`
    // under the default dir would miss it or hit a colliding id (codex P1).
    if let Some(dir) = config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let fut = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(CLAUDE_TIMEOUT, fut).await {
        Err(_) => (false, Some(format!("{verb} timed out"))),
        Ok(Err(_)) => (false, Some("claude unavailable".to_string())),
        Ok(Ok(status)) if status.success() => (true, None),
        Ok(Ok(_)) => (false, Some(format!("{verb} failed"))),
    }
}

/// Shell `claude agents --json --all` ONCE for the startup reconcile (x-7561),
/// bounded + fail-open: parse the tracked-id liveness map, or `None` on missing
/// binary / non-zero exit / timeout / schema drift so the caller holds tracked
/// rows as `unknown` rather than deleting an id it could not observe (AC1-FR).
async fn run_claude_agents_all(
    tracked: &std::collections::HashSet<String>,
) -> Option<HashMap<String, crate::agents_view::ObservedExternal>> {
    const AGENTS_TIMEOUT: Duration = Duration::from_secs(10);
    let fut = tokio::process::Command::new("claude")
        .args(["agents", "--json", "--all"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(AGENTS_TIMEOUT, fut).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    crate::agents_view::parse_claude_agents(&String::from_utf8_lossy(&output.stdout), tracked)
}

/// Map `fno-agents reap --json` stdout to the `reaped N` notice. The verb
/// exited zero, so the reap ran; unparseable output still reports a success
/// with an unknown count rather than a false failure (the row-vanish is the
/// authoritative truth, this notice is advisory).
fn reap_notice(stdout: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(v) => match v.get("reaped").and_then(|r| r.as_array()) {
            Some(arr) => format!("reaped {}", arr.len()),
            None => "reaped 0".to_string(),
        },
        Err(_) => "reap: done".to_string(),
    }
}

/// x-0296 CI diagnostics: timestamped breadcrumbs for the e2e server log
/// (`<session>.log`, dumped by the test harness on a wait_screen timeout).
/// FNO_E2E-gated so a production server writes none of it; the gate is
/// latched once so the hot call sites (push_layout) never re-read the env.
fn e2e_log(msg: std::fmt::Arguments<'_>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("FNO_E2E").is_some()) {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        eprintln!("fno mux e2e[{ms} pid {}]: {msg}", std::process::id());
    }
}

/// Shell `fno-agents claim sweep --json`, bounded + fail-open (the digest
/// idiom, x-4e2d): returns the live-claim map (node id -> holder) for the
/// work-queue overlay (x-54fa), or `None` on missing binary / non-zero exit /
/// timeout / unparseable output — the caller keeps its last-good sweep, so a
/// single flaky tick never downgrades an in-flight card.
async fn run_claim_sweep() -> Option<HashMap<String, String>> {
    const SWEEP_TIMEOUT: Duration = Duration::from_millis(800);
    let fut = tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())
        .args(["claim", "sweep", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // On timeout the future is dropped; kill_on_drop reaps the child so a
        // hung sweep can't accumulate an orphan per tick.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SWEEP_TIMEOUT, fut).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    backlog_view::live_claims_from_sweep(&String::from_utf8_lossy(&output.stdout))
}

/// Whether `node` (an id or slug) names a READY card in the server's backlog
/// snapshot (x-a496, codex peer review). A targeted card dispatch must refuse a
/// blocked / in-flight / unknown node - the same nodes `leader+g` would never
/// select - so a click cannot start work with unmet deps even if the client's
/// (staler) Layout still showed it ready. Pure so the gate is unit-testable
/// without touching the subprocess spawn.
fn card_ready_to_dispatch(backlog: &[BacklogCard], node: &str) -> bool {
    backlog
        .iter()
        .any(|c| (c.id == node || c.slug == node) && c.state == CardState::Ready)
}

/// Whether `name` carries `node` as an exact token (x-54fa, plan Locked 6):
/// the id appears with no alphanumeric neighbor on either side, so
/// `tgt-x-54fa` and `x-54fa` match but `x-54f` inside `x-54fa` (or `x-54fa`
/// inside `x-54fab`) never does. `-` cannot be the boundary test (it is part
/// of the id shape itself), so boundaries are non-alphanumeric.
fn name_has_node_token(name: &str, node: &str) -> bool {
    if node.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    let mut from = 0;
    // Advance past a rejected match by the WIDTH of node's first char, not a
    // hardcoded 1: ids are ASCII in practice, but `id_prefix` is user config,
    // and a multi-byte first char would put `start + 1` inside a char and
    // panic the slice (gemini review of PR #211).
    let first_char_len = node.chars().next().map_or(1, char::len_utf8);
    while let Some(i) = name[from..].find(node) {
        let start = from + i;
        let end = start + node.len();
        let pre_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let post_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if pre_ok && post_ok {
            return true;
        }
        from = start + first_char_len;
    }
    false
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
        self.register_pane(id, pty, rows, cols, None, cwd.to_string(), None, None);
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
        let cmd = cmd_from_argv(argv);
        let account = account_from_argv(argv);
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
        self.register_pane(id, pty, rows, cols, node, cwd.to_string(), cmd, account);
        Ok(id)
    }

    /// Record a freshly-spawned pane: bump the id, insert its VT grid, and
    /// arm its output watch (dropped receiver, so the watch costs nothing
    /// until a `PaneWait` subscribes).
    #[allow(clippy::too_many_arguments)]
    fn register_pane(
        &mut self,
        id: u64,
        pty: PtyShell,
        rows: u16,
        cols: u16,
        node: Option<String>,
        cwd: String,
        cmd: Option<String>,
        account: Option<String>,
    ) {
        self.next_pane_id += 1;
        e2e_log(format_args!("pane {id} registered ({rows}x{cols})"));
        self.panes.insert(
            id,
            PaneEntry {
                pty,
                vt: vt::Pane::new(rows, cols),
                node,
                cwd,
                cmd,
                account,
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
        self.touch_last_emit.remove(&pid);
        self.wheel_gate.remove(&pid);
        // (x-0090) Drop any attach mapping onto the dead pane so a re-attach
        // spawns fresh rather than focusing a corpse (the lazy `panes` check in
        // `agent_rows()` is the belt to this eager suspenders - Discretion 3).
        self.attached.retain(|_, p| *p != pid);
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
                        (sid, sq.tabs[ti].id, sq.canonical_cwd().to_string())
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

    fn resolve_placement_target(
        &self,
        target: &PaneTarget,
        current: Option<u64>,
    ) -> Result<Option<u64>, String> {
        match target {
            PaneTarget::CurrentRoute => Ok(current),
            PaneTarget::SquadName(name) => {
                let n = name.trim();
                if n.is_empty() {
                    return Err("squad name cannot be blank".into());
                }
                let cwds: Vec<String> = self
                    .session
                    .squads
                    .iter()
                    .map(|s| s.canonical_cwd().to_string())
                    .collect();
                let derived = squad::display_names(&cwds);
                let mut hits = self
                    .session
                    .squads
                    .iter()
                    .zip(derived)
                    .filter(|(s, derived)| s.name.as_deref().unwrap_or(derived) == n)
                    .map(|(s, _)| s);
                match (hits.next(), hits.next()) {
                    (Some(s), None) => Ok(Some(s.id)),
                    (Some(_), Some(_)) => Err(format!("ambiguous squad name: {n}")),
                    (None, _) => Err(format!("no such squad: {n}")),
                }
            }
            PaneTarget::SquadId(id) => self
                .session
                .squad(*id)
                .map(|s| Some(s.id))
                .ok_or_else(|| format!("no such squad id: {id}")),
        }
    }

    /// Place a spawned pane -> `(squad, tab, split_fell_back)`. `split_fell_back` is `true` when a requested
    /// split was refused at min-size and the pane landed as a new tab instead (x-9f75 AC3-FR; caller notices
    /// "tab full"). Only a vanished-squad race errs; a crowded tab never dead-ends.
    fn place_spawned_pane(
        &mut self,
        dest: Option<u64>,
        squad_key: &str,
        pid: u64,
        split: Option<Dir>,
    ) -> Result<(u64, TabId, bool), String> {
        let sid = match dest {
            Some(sid) => sid,
            None => {
                let tid = self.session.mint_tab_id();
                let tab = Tab {
                    name: None,
                    id: tid,
                    root: Node::Leaf(pid),
                    focus: pid,
                };
                let sid = self.next_squad_id;
                self.next_squad_id += 1;
                self.session
                    .add_squad(sid, vec![squad_key.to_string()], None, tab);
                return Ok((sid, tid, false));
            }
        };
        let Some(si) = self.session.squads.iter().position(|s| s.id == sid) else {
            self.reap_pane(pid);
            return Err("selected squad vanished".into());
        };
        let new_tab = |this: &mut Self, si: usize| {
            let tid = this.session.mint_tab_id();
            this.session.squads[si].tabs.push(Tab {
                name: None,
                id: tid,
                root: Node::Leaf(pid),
                focus: pid,
            });
            tid
        };
        if split.is_none() || self.session.squads[si].tabs.is_empty() {
            return Ok((sid, new_tab(self, si), false));
        }
        let dir = split.expect("split present");
        let squad = &self.session.squads[si];
        let ti = squad.active_tab.min(squad.tabs.len() - 1);
        let tid = squad.tabs[ti].id;
        let vp = self.tab_rect(tid);
        let split_ok = {
            let tab = &mut self.session.squads[si].tabs[ti];
            tree::split_directional(tab, vp, dir, pid).is_ok()
        };
        if split_ok {
            Ok((sid, tid, false))
        } else {
            // Split refused (tab min-size): fall back to a new tab rather than reaping and dead-ending.
            // A fresh tab is a full-viewport leaf, so it always fits.
            Ok((sid, new_tab(self, si), true))
        }
    }

    fn run_pane(
        &mut self,
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        claim: bool,
        placement: PanePlacement,
    ) -> Result<u64, String> {
        // Create-if-absent lives ONLY here on the script path (Locked 7, x-9f75): a `pane run --squad
        // <name>` for a not-yet-existing squad mints one so lanes group by project; AttachAgent / UI targets
        // stay fail-closed. Only an UNKNOWN name is creatable (blank / unknown id still error). Resolved
        // pre-spawn so a bad target refuses with no pane.
        let (dest, create_name): (Option<u64>, Option<String>) = match &placement.target {
            PaneTarget::SquadName(name) => {
                let n = name.trim();
                if n.is_empty() {
                    return Err("squad name cannot be blank".into());
                }
                match self.resolve_placement_target(&placement.target, None) {
                    Ok(d) => (d, None),
                    // Coupled to resolve_placement_target's error text: a name matching NO squad is
                    // creatable; an ambiguous name (2+ matches) still errors - never silently pick one.
                    Err(e) if e.starts_with("no such squad") => (None, Some(n.to_string())),
                    Err(e) => return Err(e),
                }
            }
            _ => {
                let current = self.session.find_by_cwd(&squad_key);
                (
                    self.resolve_placement_target(&placement.target, current)?,
                    None,
                )
            }
        };
        let pid = self.spawn_pane_cmd(&argv, rows, cols, &cwd)?;
        if claim {
            // Writer-claim ELIGIBILITY, set only at agent spawn (Locked 5).
            // The claim itself is acquired per-burst via PaneClaim.
            self.claim_eligible.insert(pid);
        }
        if let Some(name) = create_name {
            // Origins = the spawn's repo root, so same-project lanes converge here. persist_squad
            // write-through is non-blocking (x-8f11): a failed write degrades restore, not the live session.
            let sid = self.next_squad_id;
            self.next_squad_id += 1;
            let tid = self.session.mint_tab_id();
            self.session.add_squad(
                sid,
                vec![squad_key.clone()],
                Some(name),
                Tab {
                    name: None,
                    id: tid,
                    root: Node::Leaf(pid),
                    focus: pid,
                },
            );
            self.squad_members.insert(sid, Vec::new());
            self.persist_squad(sid);
        } else {
            self.place_spawned_pane(dest, &squad_key, pid, placement.split)?;
        }
        // Keep any attached client's view consistent; a script-only session
        // has no clients, so this is then a cheap no-op.
        self.push_layout(true);
        Ok(pid)
    }

    /// A one-line refusal/notice to ONE client (BEL + transient message on
    /// its side). Errors write to the session log, never a client terminal
    /// (the compositor owns it).
    /// The sideline-attach catalog gate: is `id` a live watch-only row
    /// (paneless, not exited) whose jobId matches? Both a registry bg row and
    /// a roster-synthesized foreign row (x-0a2e) share this shape, so foreign
    /// rows attach through the existing path with no new spawn logic (AC2-HP).
    fn attachable_agent(&self, id: &str) -> bool {
        self.agents
            .iter()
            .any(|a| a.mux.is_none() && !a.exited && a.attach_id.as_deref() == Some(id))
    }

    // ---- Persisted named squads (x-8f11) --------------------------------

    /// Whether `name` is already taken by a LIVE named squad or a PERSISTED
    /// one - the fail-closed uniqueness gate for `NewSquad` and recruit-create
    /// (Locked Decision 4). Case-sensitive, trimmed by the caller.
    fn named_squad_taken(&self, name: &str) -> bool {
        let live = self
            .session
            .squads
            .iter()
            .any(|s| s.name.as_deref() == Some(name));
        live || crate::squad_store::load()
            .squads
            .iter()
            .any(|s| s.name == name)
    }

    /// Write-through one persisted squad (upsert by name). Reads name/origins
    /// from the live session squad and members from `squad_members`; a squad
    /// that is unnamed or untracked is a silent no-op.
    fn persist_squad(&mut self, sid: u64) {
        let Some(sq) = self.session.squad(sid) else {
            return;
        };
        let Some(name) = sq.name.clone() else {
            return;
        };
        let origins = sq.origins.clone();
        let members = self.squad_members.get(&sid).cloned().unwrap_or_default();
        if let Err(e) = crate::squad_store::upsert(&name, &origins, &members) {
            self.persist_degraded(&e);
        }
    }

    /// Write-through a raw upsert from captured fields (used when the in-session
    /// squad is already gone - a churned member's last pane).
    fn persist_stored(
        &mut self,
        name: &str,
        origins: &[String],
        members: &[crate::squad_store::StoredMember],
    ) {
        if let Err(e) = crate::squad_store::upsert(name, origins, members) {
            self.persist_degraded(&e);
        }
    }

    /// Write-through a delete of the workspace named `name`.
    fn persist_remove_name(&mut self, name: &str) {
        if let Err(e) = crate::squad_store::remove(name) {
            self.persist_degraded(&e);
        }
    }

    /// Notice every client exactly once that persistence is degraded (AC3-ERR),
    /// then latch silent. The live session is never affected by a failed write.
    fn persist_degraded(&mut self, e: &std::io::Error) {
        eprintln!("fno mux: squad persistence degraded: {e}");
        if self.persist_degraded_notified {
            return;
        }
        self.persist_degraded_notified = true;
        let text = format!("workspace persistence degraded: {e}");
        for c in &self.clients {
            let _ = c
                .reliable_tx
                .try_send(ServerMsg::Notice { text: text.clone() });
        }
    }

    /// The persisted-member context of a pane, captured BEFORE it is reaped
    /// (the reap clears `attached` and the tree). `(squad id, name, origins,
    /// attach_id)`, or `None` when the pane is not a member of a persisted
    /// named squad.
    #[allow(clippy::type_complexity)]
    fn member_ctx(&self, pid: u64) -> Option<(u64, String, Vec<String>, String)> {
        let attach_id = self
            .attached
            .iter()
            .find(|(_, &p)| p == pid)
            .map(|(k, _)| k.clone())?;
        let (sid, _) = self.session.find_pane(pid)?;
        if !self.squad_members.contains_key(&sid) {
            return None;
        }
        let sq = self.session.squad(sid)?;
        let name = sq.name.clone()?;
        Some((sid, name, sq.origins.clone(), attach_id))
    }

    /// Broadcast a one-line notice to every attached client (restore + degraded
    /// paths that are not scoped to one sender).
    fn notice_all(&self, text: impl Into<String>) {
        let text = text.into();
        for c in &self.clients {
            let _ = c
                .reliable_tx
                .try_send(ServerMsg::Notice { text: text.clone() });
        }
    }

    /// (x-c914) The birth account + isolated `config_dir` for a to-be-attached
    /// row, looked up by `attach_id` in the current catalog. A default-account
    /// row (or an unknown id) yields `(None, None)`, so the attach runs under
    /// the ambient `~/.claude` exactly as before; an isolated-account row yields
    /// its config_dir so `attach_argv` routes to the right daemon (codex P1).
    fn attach_account_ctx(&self, attach_id: &str) -> (Option<String>, Option<std::path::PathBuf>) {
        let account = self
            .agents
            .iter()
            .find(|a| a.attach_id.as_deref() == Some(attach_id))
            .and_then(|a| a.account.clone());
        let dir = account.as_deref().and_then(agents_view::account_config_dir);
        (account, dir)
    }

    /// The attach-ids that are LIVE right now, read synchronously from the
    /// registry + roster files. Restore runs at the first attach, before the
    /// off-loop 1s reader has populated `self.agents`, so a stale in-memory
    /// catalog would tombstone every member (AC1-HP). One-shot read per server
    /// lifetime, off the steady loop.
    fn live_attach_ids_now(&self) -> HashSet<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let reg = std::fs::read_to_string(agents_view::registry_path()).ok();
        let roster = std::fs::read_to_string(agents_view::roster_path()).ok();
        let mut live = live_ids_from(reg.as_deref(), roster.as_deref(), now);
        // (x-c914) An isolated-account worker in a persisted squad is live in
        // ITS roster, not the default one; fold each so restore does not
        // tombstone a live alt-account member.
        for (_account, path) in agents_view::isolated_roster_paths() {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                for w in agents_view::parse_roster(&raw).into_iter().flatten() {
                    live.insert(w.short_id);
                }
            }
        }
        live
    }

    /// Materialize the persisted named squads at the first real attach (US2).
    /// `rows`/`cols` are the attaching client's dims; `home_sid` is its own cwd
    /// squad, restored as the active anchor afterward so the restored squads sit
    /// in the sideline without stealing the view. Per-squad failure isolation:
    /// a squad that cannot even open a shell is skipped with a notice, never a
    /// crash (AC2-FR: a degraded restore leaves a fully usable session).
    fn restore_squads(&mut self, rows: u16, cols: u16, home_sid: u64) {
        let loaded = crate::squad_store::load();
        if let Some(n) = loaded.notice {
            self.notice_all(n);
        }
        // (x-7561) Stash the external-lifecycle tombstones so the sideline can
        // render them BEFORE the squads-empty early-out - a store with no named
        // squads can still hold stopped/failed external rows to act on. The
        // startup reconcile against `claude agents --all` runs off-loop from the
        // attach path and refreshes this via `ExternalLifecycleSync`.
        self.external_lifecycle = loaded.external_lifecycle;
        if loaded.squads.is_empty() {
            return;
        }
        let live = self.live_attach_ids_now();
        // (x-c914) Reverse lookup so a persisted isolated-account member restores
        // against its own daemon, not the default ~/.claude.
        let iso_ctx = isolated_attach_ctx();
        let home_cwd = std::env::var_os("HOME")
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
        for ps in loaded.squads {
            let cwd0 = ps
                .origins
                .first()
                .cloned()
                .unwrap_or_else(|| home_cwd.clone());
            let mut members: Vec<crate::squad_store::StoredMember> = Vec::new();
            // (tab, optional (attach_id, pid)) for each pane we build.
            let mut tabs: Vec<(Tab, Option<(String, u64)>)> = Vec::new();
            for m in &ps.members {
                if m.tombstone {
                    members.push(m.clone()); // already dead - stays a tombstone
                    continue;
                }
                if !live.contains(&m.attach_id) {
                    // Dead now: tombstone it (AC1-EDGE dimmed row, persisted).
                    members.push(crate::squad_store::StoredMember {
                        attach_id: m.attach_id.clone(),
                        tombstone: true,
                    });
                    continue;
                }
                // Live: re-attach it into a fresh pane, routed to its daemon.
                let (acct, cd) = match iso_ctx.get(&m.attach_id) {
                    Some((a, d)) => (Some(a.as_str()), Some(d.as_path())),
                    None => (None, None),
                };
                let argv = attach_argv(&m.attach_id, acct, cd);
                match self.spawn_pane_cmd(&argv, rows, cols, &cwd0) {
                    Ok(pid) => {
                        let tid = self.session.mint_tab_id();
                        tabs.push((
                            Tab {
                                name: None,
                                id: tid,
                                root: Node::Leaf(pid),
                                focus: pid,
                            },
                            Some((m.attach_id.clone(), pid)),
                        ));
                        members.push(crate::squad_store::StoredMember {
                            attach_id: m.attach_id.clone(),
                            tombstone: false,
                        });
                    }
                    Err(e) => {
                        // AC2-FR: keep the member (not tombstone - it is live),
                        // skip its pane, notice; restore continues.
                        self.notice_all(format!("restore: could not attach {}: {e}", m.attach_id));
                        members.push(crate::squad_store::StoredMember {
                            attach_id: m.attach_id.clone(),
                            tombstone: false,
                        });
                    }
                }
            }
            // >=1-tab invariant (AC1-EDGE zero-live, or every attach spawn
            // failed): open one shell at origins[0] (else $HOME).
            if tabs.is_empty() {
                match self.spawn_pane(rows, cols, &cwd0) {
                    Ok(pid) => {
                        let tid = self.session.mint_tab_id();
                        tabs.push((
                            Tab {
                                name: None,
                                id: tid,
                                root: Node::Leaf(pid),
                                focus: pid,
                            },
                            None,
                        ));
                    }
                    Err(e) => {
                        // Cannot even open a shell: skip this squad entirely, the
                        // rest of the restore proceeds (per-squad isolation).
                        self.notice_all(format!("restore: skipped workspace {}: {e}", ps.name));
                        continue;
                    }
                }
            }
            // Register the squad with its first tab, push the rest, record the
            // attach mappings so agent_rows reconciles the panes and member_ctx
            // resolves them.
            let sid = self.next_squad_id;
            self.next_squad_id += 1;
            let mut it = tabs.into_iter();
            let (first_tab, first_map) = it.next().expect("tabs is non-empty above");
            self.session
                .add_squad(sid, ps.origins.clone(), Some(ps.name.clone()), first_tab);
            if let Some((id, pid)) = first_map {
                self.attached.insert(id, pid);
            }
            for (tab, map) in it {
                self.session
                    .squad_mut(sid)
                    .expect("just added")
                    .tabs
                    .push(tab);
                if let Some((id, pid)) = map {
                    self.attached.insert(id, pid);
                }
            }
            self.squad_members.insert(sid, members);
            // Persist the reconciled membership (members dead at restore are now
            // tombstoned in the store).
            self.persist_squad(sid);
        }
        // The restored squads must not steal the attaching client's view: its
        // per-client `view` is untouched, but add_squad flipped the global MRU
        // anchor - restore it so the sideline active marker stays on home.
        if self.session.squad(home_sid).is_some() {
            self.session.active_squad = Some(home_sid);
        }
        self.push_layout(true);
    }

    /// Reconcile the store after a member pane left, given its pre-reap context.
    /// `churn` (worker died on its own) tombstones the member and keeps the
    /// workspace persisted even if its last pane just died (AC4-EDGE + the
    /// zero-live restore, AC1-EDGE); `!churn` (user closed the pane) de-recruits
    /// the member, and if that was the workspace's last pane the whole entry is
    /// dropped (AC3-EDGE - it must not return at restart).
    fn reconcile_member_close(
        &mut self,
        ctx: Option<(u64, String, Vec<String>, String)>,
        churn: bool,
    ) {
        let Some((sid, name, origins, attach_id)) = ctx else {
            return;
        };
        // member_ctx only returns Some when squad_members holds sid, so get_mut
        // is guaranteed present - never insert an empty vec via entry() (gemini
        // review).
        let Some(members) = self.squad_members.get_mut(&sid) else {
            return;
        };
        if churn {
            if let Some(mm) = members.iter_mut().find(|m| m.attach_id == attach_id) {
                mm.tombstone = true;
            }
            let members = members.clone();
            self.persist_stored(&name, &origins, &members);
        } else {
            members.retain(|m| m.attach_id != attach_id);
            let survives = self.session.squad(sid).is_some();
            if survives {
                self.persist_squad(sid);
            } else {
                self.squad_members.remove(&sid);
                self.persist_remove_name(&name);
            }
        }
    }

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
    fn dispatch_next(&self, id: u64, node: Option<String>, account: Option<String>) {
        let session = self.session_name.clone();
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_dispatch_one(&session, node.as_deref(), account.as_deref()).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// Shell `fno-agents <verb> <name>` OFF the core loop (x-76ea), mirroring
    /// `dispatch_next`: the one-line outcome routes back as a `DispatchResult`
    /// notice, but the AUTHORITATIVE row change is the registry poll's exited
    /// flip / row vanish, not this notice. `verb` is a fixed literal
    /// (`"stop"`/`"rm"`), never operator text; `name` was catalog-validated by
    /// the caller.
    fn agent_action(&self, id: u64, verb: &'static str, name: String) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_agent_action(verb, &name).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// (x-9c5f) Shell `fno mail send <name> <text>` OFF the core loop, mirroring
    /// `agent_action`: the CLI's one-line verdict routes back as a
    /// `DispatchResult` notice. `name` was catalog-validated and `text` sanitized
    /// by the caller.
    fn mail_agent(&self, id: u64, name: String, text: String) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_mail_send(&name, &text).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// (x-9c5f) Shell `fno agents spawn <name> --resume <uuid> --substrate bg`
    /// OFF the core loop: the advisory outcome routes back as a `DispatchResult`
    /// notice; the 1s registry poll owns the row flipping live. `uuid` was
    /// shape-validated by the caller.
    fn respawn_agent(
        &self,
        id: u64,
        name: String,
        uuid: String,
        cwd: String,
        account: Option<String>,
    ) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_respawn(&name, &uuid, &cwd, account.as_deref()).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// Shell `fno agents peek <name>` OFF the core loop (x-c376), the
    /// `dispatch_next` pattern: the transcript routes back as a `PeekResult` the
    /// core loop turns into a `PeekBody` for the requesting client only. `seq`
    /// rides through unchanged so the client can drop a stale reply. Read-only -
    /// the peek subprocess never writes anything the peer reads. `name` was
    /// resolved from the client's own `Layout`; no server-side catalog validation
    /// is needed (an unknown name simply comes back as `peek`'s exit-13 body).
    fn peek_agent(&self, id: u64, name: String, seq: u64) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let lines = run_agent_peek(&name).await;
            let _ = core_tx
                .send(CoreMsg::PeekResult {
                    id,
                    seq,
                    name,
                    lines,
                })
                .await;
        });
    }

    /// Bulk-reap OFF the core loop (x-7561): shell `fno-agents reap --json` once,
    /// parse the reaped count, route it back as a `reaped N` notice. Same
    /// off-loop + advisory-notice contract as `agent_action`; the registry poll
    /// owns the row-vanish, not this notice.
    fn reap_action(&self, id: u64) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_reap().await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// Resolve a `StopExternal` target by attach id (x-7561): return the
    /// `(name, cwd)` snapshot for the CAS, from a LIVE external roster row (the
    /// normal live stop) OR a persisted retry-eligible tombstone (a
    /// failed/unknown/stopping record whose `x` retries the stop). Fail-closed
    /// when the id names neither - the AC1-ERR stale-target refusal, so a row
    /// that raced out between confirm and command launches no subprocess.
    fn resolve_external_stop_target(&self, attach_id: &str) -> Result<(String, String), String> {
        if let Some(a) = self
            .agents
            .iter()
            .find(|a| a.external && a.attach_id.as_deref() == Some(attach_id))
        {
            return Ok((a.name.clone(), a.cwd.clone()));
        }
        use crate::squad_store::ExternalState as S;
        if let Some(r) = self.external_lifecycle.iter().find(|r| {
            r.attach_id == attach_id && matches!(r.state, S::Failed | S::Unknown | S::Stopping)
        }) {
            return Ok((r.name.clone(), r.cwd.clone()));
        }
        Err(format!("{attach_id} is no longer a live external row"))
    }

    /// Re-read the durable `external_lifecycle` into the render snapshot and
    /// re-push the sideline (x-7561), so an in-flight `stopping…`/`removing…`
    /// state is visible the instant the CAS commits (AC1-UI), before the
    /// off-loop subprocess even starts.
    fn refresh_external_lifecycle(&mut self) {
        self.external_lifecycle = crate::squad_store::load().external_lifecycle;
        self.push_layout(true);
    }

    /// Run an external lifecycle subprocess (`claude stop|rm <attach_id>`) OFF
    /// the core loop (x-7561), then durably record the completion under the
    /// captured `generation` (a stale generation is ignored by
    /// `complete_external`) and route the refreshed record set + outcome notice
    /// back for the render update. `verb` is a fixed literal; `attach_id` was
    /// 8-hex validated at load, so it can never be a shell injection.
    fn external_action(
        &self,
        client_id: u64,
        verb: &'static str,
        attach_id: String,
        generation: u64,
        action: crate::squad_store::ExternalState,
    ) {
        let core_tx = self.self_tx.clone();
        let (_acct, config_dir) = self.attach_account_ctx(&attach_id);
        tokio::spawn(async move {
            let (ok, reason) = run_claude_lifecycle(verb, &attach_id, config_dir).await;
            let _ = crate::squad_store::complete_external(
                &attach_id,
                generation,
                action,
                ok,
                reason.clone(),
            );
            let records = crate::squad_store::load().external_lifecycle;
            let past = if verb == "stop" { "stopped" } else { "removed" };
            let notice = if ok {
                format!("{past} {attach_id}")
            } else {
                reason.unwrap_or_else(|| format!("{verb} {attach_id}: failed"))
            };
            let _ = core_tx
                .send(CoreMsg::ExternalLifecycleSync {
                    to: Some(client_id),
                    records,
                    notices: vec![notice],
                })
                .await;
        });
    }

    /// Reconcile the persisted external tombstones against `claude agents --json
    /// --all` ONCE at startup (x-7561, AC1-FR/AC3-FR), OFF the core loop. Filters
    /// the daemon's full history to tracked ids only, applies the pure reconcile
    /// table, commits the result, and routes the refreshed set + notices back to
    /// every client. A no-tracked-id store spawns nothing.
    fn reconcile_external_lifecycle(&self) {
        // Snapshot attach_id -> generation BEFORE the off-lock query, so the
        // atomic locked apply can leave any record a concurrent operator action
        // advanced past its baseline untouched (lost-update guard, code review).
        let baseline: std::collections::HashMap<String, u64> = self
            .external_lifecycle
            .iter()
            .map(|r| (r.attach_id.clone(), r.generation))
            .collect();
        if baseline.is_empty() {
            return;
        }
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let tracked: std::collections::HashSet<String> = baseline.keys().cloned().collect();
            let observed = run_claude_agents_all(&tracked).await;
            // Read-compute-write is atomic under the store lock: reconcile only
            // the baseline-generation-matching records; a concurrent stop/rm's
            // record (advanced generation) is left for its own completion.
            let notices = crate::squad_store::reconcile_lifecycle(&baseline, |reconcilable| {
                crate::agents_view::reconcile_external(reconcilable, observed.as_ref())
            })
            .unwrap_or_default();
            let records = crate::squad_store::load().external_lifecycle;
            let _ = core_tx
                .send(CoreMsg::ExternalLifecycleSync {
                    to: None,
                    records,
                    notices,
                })
                .await;
        });
    }

    /// Resolve a sideline lifecycle target (x-76ea `StopAgent`/`RemoveAgent`) by
    /// name against the current catalog, returning the exited flag of the single
    /// resolved registry row. `name` is NOT a unique key (codex review): the
    /// catalog dedups by `attach_id`, so an external roster row and a registry
    /// row can carry the same name. Fail-closed on every ambiguity - absent, any
    /// external row sharing the name (never act on a registry agent an external
    /// shadows), or a >1 non-external collision - so a keypress can only ever act
    /// on exactly one unambiguous registry agent, never a guessed match.
    /// (x-9c5f) Widened from `Result<bool>` to the resolved row reference so
    /// callers can read `.exited` AND `.claude_session_uuid` (the respawn arm
    /// needs both); the fail-closed semantics (absent / external / ambiguous all
    /// refused) are unchanged.
    fn resolve_lifecycle_target(&self, name: &str) -> Result<&RegistryAgent, String> {
        let matches: Vec<&RegistryAgent> = self.agents.iter().filter(|a| a.name == name).collect();
        if matches.is_empty() {
            return Err(format!("no such agent: {name}"));
        }
        if matches.iter().any(|a| a.external) {
            return Err(format!(
                "{name} is external - manage it from its own session"
            ));
        }
        match matches.as_slice() {
            [one] => Ok(one),
            _ => Err(format!("{name} is ambiguous - use the CLI")),
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
                            vec![key],
                            None,
                            Tab {
                                name: None,
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
        // Cold-attach snapshot rides the RELIABLE channel (x-0296). The
        // dirty-map seed push_layout just wrote is droppable by design, and
        // a passive reattach with quiet panes produces no PTY output, so a
        // lost seed never recovers (`broadcast_pane` only fires on output).
        // Re-send every visible pane's frame on the reliable queue - ordered
        // after the Layout queued above - and drop the now-redundant
        // droppable seeds. Steady-state delivery (DirtyMap + broadcast_pane)
        // is unchanged; the wire message set is unchanged (same `Frame`
        // variant, so no proto bump).
        if let Some(c) = self.clients.iter().find(|c| c.id == id) {
            let mut sent_n = 0usize;
            let mut pids: Vec<u64> = c.visible.iter().copied().collect();
            pids.sort_unstable();
            let visible_n = pids.len();
            let mut d = c.dirty.lock().unwrap();
            for pid in pids {
                if let Some(entry) = self.panes.get(&pid) {
                    let sent = c
                        .reliable_tx
                        .try_send(ServerMsg::Frame {
                            pane_id: pid,
                            frame: entry.vt.frame(),
                        })
                        .is_ok();
                    // Only drop the droppable seed push_layout wrote once the
                    // reliable frame actually landed. A failed send means a
                    // wedged client (unreachable at birth: fresh 256-cap
                    // channel, dead clients already reaped by push_layout) -
                    // but if it ever happens, keep the seed so the already-
                    // notified droppable path stays the fallback rather than
                    // leaving the pane with no delivery at all.
                    if sent {
                        sent_n += 1;
                        d.remove(&pid);
                    }
                }
            }
            drop(d);
            e2e_log(format_args!(
                "attach client {id}: {visible_n} visible panes, {sent_n} reliable frames"
            ));
        }
        // (x-8f11) Eager restore of persisted named squads, once per server
        // lifetime, on the first REAL (non-passive) attach - a passive observer
        // has no dims to spawn panes with, so it defers restore to the first
        // terminal. The restored squads sit in the sideline; this client's view
        // stays on its own cwd squad.
        if !self.restored && !passive {
            self.restored = true;
            self.restore_squads(rows, cols, view.0);
            // (x-7561) One bounded `claude agents --all` reconcile of the loaded
            // external tombstones, off the core loop; a no-tombstone store is a
            // no-op. Runs after restore so `self.external_lifecycle` is populated.
            self.reconcile_external_lifecycle();
        }
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
    /// what that client's terminal last saw), send its `Layout`, and (when
    /// `reemit`) flush stale frame slots and queue full frames for its
    /// visible panes. Focus-only changes pass `reemit: false` - rects are
    /// unchanged, so queued frames stay valid (and are kept: a pending
    /// quiet-pane frame has no other copy). Unviewed tabs are untouched: grids keep
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

        // (x-4328) Evict half of the seen set: level-triggered every pass,
        // no prev-tick diffing - any pane whose CURRENT badge isn't `Done` is
        // dropped, so a re-run re-arms unseen for free. The insert half is
        // NOT level-triggered on "is this still the focused pane": AC2-EDGE
        // requires that parking on a pane while it is `Working` never marks
        // a LATER `Done` seen, so insert instead fires as a one-shot side
        // effect of the actual focus action (`Command::FocusPane`; hover-
        // focus settles to a client-side `FocusPane`, so it rides the same
        // hook for free). `AttachAgent` always spawns a brand-new pane_id,
        // which can never already be `Done`, so it has no seen-marking hook.
        // Read `self.agents` directly rather than `self.agent_rows()`: the
        // latter allocates a `Vec<AgentRow>` and clones every row's strings
        // for the full registry on every pass, which is wasteful for a check
        // that only needs a pane's own badge (gemini review).
        for a in &self.agents {
            if let Some((sess, pane)) = &a.mux {
                if sess == &self.session_name {
                    let pid = *pane;
                    let exited = a.exited || !self.panes.contains_key(&pid);
                    let badge = if exited { None } else { a.badge };
                    if badge != Some(AgentBadge::Done) {
                        self.seen.remove(&pid);
                    }
                }
            }
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
            if c.reliable_tx.try_send(layout_msg).is_err() {
                dead.push(c.id);
                continue;
            }
            e2e_log(format_args!(
                "layout -> client {}: {} rects, reemit={reemit}",
                c.id,
                rects.len()
            ));
            // An observer subscribes to all panes; a driving client to just
            // its viewed tab's rects.
            let frame_ids: Vec<u64> = if c.passive {
                all_pane_ids.clone()
            } else {
                rects.iter().map(|(pid, _)| *pid).collect()
            };
            c.visible = frame_ids.iter().copied().collect();
            if reemit {
                // Flush-then-re-emit: geometry changed, so queued frames are
                // stale - drop them and re-seed every visible pane in one
                // locked pass, so every frame the client draws after this is
                // consistent with the Layout generation it just received.
                //
                // A focus-only push (reemit=false) must NOT flush: rects are
                // unchanged, so queued frames stay valid (the contract in
                // this function's doc) - and a quiet pane's pending output
                // frame has no other copy. Clearing it between the output's
                // dirty-insert and the writer's drain blanked the pane until
                // its NEXT output (broadcast_pane only fires on output),
                // which for an idle shell is never: the x-0296 CI flake.
                let mut d = c.dirty.lock().unwrap();
                d.clear();
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
            e2e_log(format_args!(
                "push_layout dropped wedged clients {dead:?} (reliable send failed)"
            ));
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
            .map(|s| s.canonical_cwd().to_string())
            .collect();
        let derived = squad::display_names(&cwds);
        let squads = self
            .session
            .squads
            .iter()
            .zip(derived)
            .map(|(s, derived)| SquadMeta {
                id: s.id,
                // An explicit workspace name wins; an attach-born squad falls
                // back to the origin-basename label (disambiguated).
                name: s.name.clone().unwrap_or(derived),
                canonical_cwd: s.canonical_cwd().to_string(),
                tabs: s
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| TabMeta {
                        id: t.id,
                        name: tab_label(
                            t.name.as_deref(),
                            self.panes
                                .get(&t.focus)
                                .map(|e| (e.node.as_deref(), e.cwd.as_str(), e.cmd.as_deref())),
                            s.canonical_cwd(),
                            i,
                        ),
                        // (v22, x-653d) Every leaf pane of the tab, labelled from
                        // its own entry, so the navigator can goto a pane in any
                        // tab/squad - not just the active view the client tiles.
                        panes: tree::leaves(&t.root)
                            .iter()
                            .map(|pid| {
                                let e = self.panes.get(pid);
                                PaneMeta {
                                    id: *pid,
                                    label: pane_label(
                                        e.and_then(|e| e.node.as_deref()),
                                        e.map(|e| e.cwd.as_str()).unwrap_or(""),
                                        e.and_then(|e| e.cmd.as_deref()),
                                    ),
                                }
                            })
                            .collect(),
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
                // Blast radius for the RemoveSquad confirm (x-96e8): live leaves
                // summed over every tab of the squad.
                panes: s.tabs.iter().map(|t| tree::leaves(&t.root).len()).sum(),
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
            // The work-queue lane (x-6f77); already board-ordered by the
            // reader, routes joined on at publish time (x-54fa).
            backlog: self.routed_backlog(),
        }
    }

    /// The backlog cards with their v18 routes joined on at publish time
    /// (x-54fa Phase B). An in-flight card gains, in priority order: the pane
    /// in THIS session whose `FNO_NODE` provenance equals the node id; else
    /// the attach jobId of a live paneless registry row working the node;
    /// else a one-line `where_hint` naming the session or claim holder. Join
    /// keys are exact only. Ready/Blocked cards pass through untouched.
    fn routed_backlog(&self) -> Vec<BacklogCard> {
        let mut cards = self.backlog.clone();
        for c in &mut cards {
            if c.state != CardState::InFlight {
                continue;
            }
            if let Some(pid) = self.node_pane(&c.id) {
                c.pane_id = Some(pid);
            } else if let Some((attach, name)) = self.node_registry_row(&c.id) {
                match attach {
                    Some(id) => c.attach_id = Some(id),
                    None => c.where_hint = Some(format!("in flight - session {name}")),
                }
            } else if let Some(holder) = self.backlog_holders.get(&c.id) {
                c.where_hint = Some(format!("in flight - worked by {holder}"));
            }
        }
        cards
    }

    /// The route command for an in-flight card named by id or slug (the same
    /// matching `card_ready_to_dispatch` uses), or `None` when the card is
    /// unknown, not in flight, or unroutable - the stale-client `DispatchNode`
    /// re-check (x-54fa AC2-ERR).
    fn inflight_route(&self, node: &str) -> Option<Command> {
        let card = self
            .backlog
            .iter()
            .find(|c| (c.id == node || c.slug == node) && c.state == CardState::InFlight)?;
        if let Some(pid) = self.node_pane(&card.id) {
            return Some(Command::FocusPane(pid));
        }
        self.node_registry_row(&card.id)
            .and_then(|(attach, _)| attach)
            .map(Command::attach_agent)
    }

    /// The situated notice for an in-flight card `inflight_route` could not
    /// route (codex peer review): the same copy the v18 click path shows, so a
    /// stale-client `DispatchNode` never regresses to a bare refusal on a card
    /// the server knows is being worked. `None` when `node` names no in-flight
    /// card (the caller falls through to the not-ready refusal).
    fn inflight_hint(&self, node: &str) -> Option<String> {
        let card = self
            .backlog
            .iter()
            .find(|c| (c.id == node || c.slug == node) && c.state == CardState::InFlight)?;
        Some(match self.node_registry_row(&card.id) {
            Some((_, name)) => format!("in flight - session {name}"),
            None => match self.backlog_holders.get(&card.id) {
                Some(holder) => format!("in flight - worked by {holder}"),
                None => "card in flight - no session visible here".to_string(),
            },
        })
    }

    /// The lowest-id live pane in this session whose `FNO_NODE` provenance
    /// equals `node`. Provenance equality only - no cwd fallback here; a
    /// shell pane that merely sits in the node's worktree is not the worker.
    fn node_pane(&self, node: &str) -> Option<u64> {
        self.panes
            .iter()
            .filter(|(_, e)| e.node.as_deref() == Some(node))
            .map(|(id, _)| *id)
            .min()
    }

    /// The live, paneless registry row working `node`: matched by exact
    /// node-id token in the worker name or registry-cwd basename equality
    /// (the worktree-per-node convention). Returns `(attach_id, name)`; a row
    /// with an attach target wins over a name-only match so the card routes
    /// whenever any matching row can be attached.
    fn node_registry_row(&self, node: &str) -> Option<(Option<String>, String)> {
        let mut named: Option<(Option<String>, String)> = None;
        for a in &self.agents {
            if a.mux.is_some() || a.exited {
                continue;
            }
            let cwd_match = Path::new(&a.cwd).file_name().and_then(|b| b.to_str()) == Some(node);
            if !cwd_match && !name_has_node_token(&a.name, node) {
                continue;
            }
            if a.attach_id.is_some() {
                return Some((a.attach_id.clone(), a.name.clone()));
            }
            named.get_or_insert((None, a.name.clone()));
        }
        named
    }

    /// The sideline row set as a PANE UNION (x-0090, Locked 5): every live pane
    /// in every squad/tab is a row, in (squad, tab, pane) order, carrying its
    /// `tab` so the client renders a tab-ordinal suffix. A pane is enriched from
    /// the registry entry that hosts it - `mux == (this session, pane)`, or a
    /// watch-only row whose `attach_id` reconciles to it (x-0090 attach map) -
    /// else it is a bare pane labelled from its `PaneEntry` (Discretion 5). One
    /// row per entity: a registry agent merged onto a pane never also renders
    /// watch-only. Truly paneless registry rows (bg/headless/daemon/roster)
    /// append AFTER the pane rows, matched to a squad by cwd (exact or child) or
    /// the `squad: None` catch-all. The fact-badge lattice is unchanged: a dead
    /// pane forces `exited` over any live-TTL badge (fact beats report).
    /// (x-cd67 US4) Compose a row's dim line-2 subline from the off-loop
    /// branch map + the cwd's tail segment: `<branch> · <tail>`, either part
    /// omitted if absent, both absent (an empty cwd) -> `None` (AC1-EDGE: no
    /// sub-row is emitted). The client renders it verbatim and truncates.
    fn compose_subline(&self, cwd: &str) -> Option<String> {
        subline_from(self.branch_by_cwd.get(cwd).map(String::as_str), cwd)
    }

    fn agent_rows(&self) -> Vec<AgentRow> {
        let mut out = Vec::new();
        // Which registry agents a pane row already claimed (so they don't
        // double-render as watch-only). Indexed like `self.agents`.
        let mut consumed = vec![false; self.agents.len()];
        // (x-9c5f) holder name -> pr_number, joining the live-claim holders map
        // (node -> holder) with the graph's node -> pr map, so a row whose name
        // holds a live claim on a pr-carrying node gets the peek `PR #N` label.
        // Harness-native claims make holder == worker name (x-3e70); a session-id
        // holder simply yields no label (Open Question 2: graceful absence).
        let pr_by_holder: HashMap<&str, u64> = self
            .backlog_holders
            .iter()
            .filter_map(|(node, holder)| self.backlog_pr.get(node).map(|pr| (holder.as_str(), *pr)))
            .collect();

        // 1. Pane rows: one per live tab leaf, deterministic (squad -> tab ->
        //    pane order). Iterating the tree (not `self.agents`) is what makes a
        //    bare shell pane a first-class row.
        for squad in &self.session.squads {
            for tab in &squad.tabs {
                for pid in tree::leaves(&tab.root) {
                    // The registry entry hosting this pane, if any: a
                    // same-session mux match, else a watch-only row the attach
                    // map reconciled onto this pane (x-0090). First match wins.
                    let matched = self.agents.iter().position(|a| match &a.mux {
                        Some((sess, pane)) => sess == &self.session_name && *pane == pid,
                        None => {
                            a.attach_id
                                .as_deref()
                                .and_then(|id| self.attached.get(id))
                                .copied()
                                == Some(pid)
                        }
                    });
                    // One lookup: liveness AND the bare-pane label read the same
                    // entry (a tree leaf reaped from `panes` is dying, so it
                    // forces `exited` - the fact-beats-report rule the old join
                    // used).
                    let pane_entry = self.panes.get(&pid);
                    let pane_dead = pane_entry.is_none();
                    let row = match matched {
                        Some(i) => {
                            consumed[i] = true;
                            let a = &self.agents[i];
                            let exited = a.exited || pane_dead;
                            AgentRow {
                                squad: Some(squad.id),
                                name: a.name.clone(),
                                pane_id: Some(pid),
                                badge: if exited { None } else { a.badge },
                                reason: if exited { None } else { a.reason.clone() },
                                exited,
                                answerable: if exited { None } else { a.answerable.clone() },
                                // A pane-hosted row focuses its pane; the attach
                                // target never rides it (wire contract).
                                attach_id: None,
                                external: a.external,
                                tab: Some(tab.id),
                                seen: self.seen.contains(&pid),
                                cwd_base: None,
                                tombstone: false,
                                subline: self.compose_subline(&a.cwd),
                                // Structural roster-dir tag wins (Locked
                                // Decision 6); else this pane's birth account.
                                account: a
                                    .account
                                    .clone()
                                    .or_else(|| pane_entry.and_then(|e| e.account.clone())),
                                updated_at: a.updated_at,
                                pr: pr_by_holder.get(a.name.as_str()).copied(),
                            }
                        }
                        None => {
                            // Bare pane: labelled from its own entry (node > cmd
                            // > cwd-basename > "shell"), matching the navigator's
                            // pane labels (v22) so the two agree.
                            let e = pane_entry;
                            AgentRow {
                                squad: Some(squad.id),
                                name: pane_label(
                                    e.and_then(|e| e.node.as_deref()),
                                    e.map(|e| e.cwd.as_str()).unwrap_or(""),
                                    e.and_then(|e| e.cmd.as_deref()),
                                ),
                                pane_id: Some(pid),
                                badge: None,
                                reason: None,
                                exited: pane_dead,
                                answerable: None,
                                attach_id: None,
                                external: false,
                                tab: Some(tab.id),
                                seen: self.seen.contains(&pid),
                                cwd_base: None,
                                tombstone: false,
                                subline: self
                                    .compose_subline(e.map(|e| e.cwd.as_str()).unwrap_or("")),
                                account: e.and_then(|e| e.account.clone()),
                                // A bare shell pane is not a registry worker: no
                                // activity stamp, no claim, no pr.
                                updated_at: None,
                                pr: None,
                            }
                        }
                    };
                    out.push(row);
                }
            }
        }

        // 2. Watch-only appendix: registry rows no pane claimed.
        for (i, a) in self.agents.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            match &a.mux {
                Some((sess, pane)) => {
                    // A row hosted in ANOTHER session is that server's to render.
                    if sess != &self.session_name {
                        continue;
                    }
                    // A same-session mux row whose pane left the tree entirely
                    // (fully reaped) is a dangling exited row - preserve the old
                    // behaviour (`find_pane` -> None squad, `exited`).
                    out.push(AgentRow {
                        squad: self.session.find_pane(*pane).map(|(sid, _)| sid),
                        name: a.name.clone(),
                        pane_id: Some(*pane),
                        badge: None,
                        reason: None,
                        exited: true,
                        answerable: None,
                        attach_id: None,
                        external: a.external,
                        tab: None,
                        seen: self.seen.contains(pane),
                        cwd_base: None,
                        tombstone: false,
                        subline: self.compose_subline(&a.cwd),
                        account: a.account.clone(),
                        updated_at: a.updated_at,
                        pr: pr_by_holder.get(a.name.as_str()).copied(),
                    });
                }
                None => {
                    // Truly paneless (bg/headless/daemon/roster). Its attach map
                    // pointed at no live pane (else a pane row claimed it), so it
                    // stays watch-only attachable - the AC1-FR revert.
                    let squad = self
                        .session
                        .squads
                        .iter()
                        .find(|s| s.owns_path(&a.cwd))
                        .map(|s| s.id);
                    // An orphan (no squad) carries its cwd basename so the client
                    // can disambiguate same-named workers under `~ elsewhere`
                    // (AC2-UI); a squad-matched row needs none.
                    let cwd_base = squad.is_none().then(|| {
                        Path::new(&a.cwd)
                            .file_name()
                            .and_then(|b| b.to_str())
                            .unwrap_or(a.cwd.as_str())
                            .to_string()
                    });
                    out.push(AgentRow {
                        squad,
                        name: a.name.clone(),
                        pane_id: None,
                        badge: if a.exited { None } else { a.badge },
                        reason: if a.exited { None } else { a.reason.clone() },
                        exited: a.exited,
                        answerable: if a.exited { None } else { a.answerable.clone() },
                        attach_id: if a.exited { None } else { a.attach_id.clone() },
                        external: a.external,
                        tab: None,
                        // A watch-only row has no pane to focus, so it is always
                        // unseen.
                        seen: false,
                        cwd_base,
                        tombstone: false,
                        subline: self.compose_subline(&a.cwd),
                        // The structural roster-dir tag: an isolated-account
                        // foreign row carries its source account here (piece 3).
                        account: a.account.clone(),
                        updated_at: a.updated_at,
                        pr: pr_by_holder.get(a.name.as_str()).copied(),
                    });
                }
            }
        }
        // 3. Synthesized tombstone rows (x-8f11 US4): each persisted member that
        //    died shows as a dimmed, dismissable row under its (live) squad. A
        //    member re-recruited to a live pane this session is skipped here (it
        //    already rendered pane-hosted above - Open Question 1: mid-session a
        //    tombstone otherwise persists until dismissed/restart).
        for (&sid, members) in &self.squad_members {
            if self.session.squad(sid).is_none() {
                continue;
            }
            for m in members.iter().filter(|m| m.tombstone) {
                if self.attached.contains_key(&m.attach_id) {
                    continue;
                }
                out.push(AgentRow {
                    squad: Some(sid),
                    name: format!("cc-{}", m.attach_id),
                    pane_id: None,
                    badge: None,
                    reason: None,
                    exited: true,
                    answerable: None,
                    // Carried so the client can DismissMember; exited: true keeps
                    // it out of the attach catalog gate (attach_id + !exited).
                    attach_id: Some(m.attach_id.clone()),
                    external: false,
                    tab: None,
                    seen: false,
                    cwd_base: None,
                    tombstone: true,
                    // A synthesized dead member has no cwd to derive a branch/tail.
                    subline: None,
                    account: None,
                    // A dead member carries no live claim or activity stamp.
                    updated_at: None,
                    pr: None,
                });
            }
        }
        // 4. External-lifecycle tombstone rows (x-7561): a persisted external
        //    record NOT currently live renders so `x` can act on it. The state
        //    maps onto the existing `exited` flag - stopped -> `exited` (rm);
        //    failed/unknown/stopping/removing -> `!exited` (stop / stop-retry),
        //    with the state as the row reason so an in-flight action is visible
        //    (AC1-UI). Deduped against live external rows (a still-live roster
        //    row wins; the record is stale until the next reconcile clears it).
        let live_ext: std::collections::HashSet<&str> = self
            .agents
            .iter()
            .filter(|a| a.external)
            .filter_map(|a| a.attach_id.as_deref())
            .collect();
        for r in &self.external_lifecycle {
            if live_ext.contains(r.attach_id.as_str()) {
                continue;
            }
            use crate::squad_store::ExternalState as S;
            let (exited, reason) = match r.state {
                S::Stopped => (true, None),
                S::Failed => (
                    false,
                    Some(r.reason.clone().unwrap_or_else(|| "stop failed".into())),
                ),
                S::Unknown => (false, Some("state unknown".to_string())),
                S::Stopping => (false, Some("stopping…".to_string())),
                S::Removing => (false, Some("removing…".to_string())),
            };
            let squad = self
                .session
                .squads
                .iter()
                .find(|s| s.owns_path(&r.cwd))
                .map(|s| s.id);
            let cwd_base = squad.is_none().then(|| {
                Path::new(&r.cwd)
                    .file_name()
                    .and_then(|b| b.to_str())
                    .unwrap_or(r.cwd.as_str())
                    .to_string()
            });
            out.push(AgentRow {
                squad,
                name: r.name.clone(),
                pane_id: None,
                badge: None,
                reason,
                exited,
                answerable: None,
                // Carried on an exited row so the client can send RemoveExternal;
                // on a live-ish row it is the StopExternal target. Either way the
                // attach-catalog gate (attach_id + !exited) never treats a stopped
                // tombstone as attachable.
                attach_id: Some(r.attach_id.clone()),
                external: true,
                tab: None,
                seen: false,
                cwd_base,
                tombstone: false,
                subline: self.compose_subline(&r.cwd),
                account: None,
                // An external row is never joined (respawn/pr are fno-registry
                // concerns); its stamps live in its own daemon.
                updated_at: None,
                pr: None,
            });
        }
        out
    }

    /// (x-4328) The insert half of the seen set: a one-shot side effect of
    /// an actual focus action (`Command::FocusPane`; hover-focus settles to
    /// a client-side `FocusPane`, so it rides this for free), never a
    /// per-pass level check - AC2-EDGE requires that parking on a pane while
    /// it is `Working` never marks a later `Done` seen, only a fresh focus
    /// action does. (x-0090) `Command::AttachAgent`'s reconcile-focus arm now
    /// calls this too: a second attach onto an already-mapped `Done` pane is a
    /// focus, so it clears unseen like FocusPane; the spawn arm mints a
    /// brand-new pane_id that can't already be `Done`, a no-op. A no-op
    /// when `pid`'s current badge isn't `Done`.
    fn mark_seen_if_done(&mut self, pid: u64) {
        // Read `self.agents` directly rather than `self.agent_rows()`: the
        // latter allocates + clones the full registry for a check that only
        // needs this one pane's badge (gemini review).
        if self.agents.iter().any(|a| {
            a.mux
                .as_ref()
                .is_some_and(|(sess, pane)| sess == &self.session_name && *pane == pid)
                && !a.exited
                && self.panes.contains_key(&pid)
                && a.badge == Some(AgentBadge::Done)
        }) {
            self.seen.insert(pid);
        }
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

    /// The line delta an interpreted wheel-scroll applies, or `None` when the
    /// event isn't a mux-interpreted scroll (a mouse-app passthrough, a select, or
    /// an ignore). The core drain folds a contiguous run of wheel ticks on one
    /// pane - including a direction reversal queued behind in-flight opposite
    /// ticks - into a single broadcast, so a fast trackpad flick settles in one
    /// frame instead of rubber-banding through every intermediate offset.
    fn scroll_delta(&self, pane: u64, event: &MouseEvent) -> Option<i32> {
        let modes = self.panes.get(&pane)?.vt.modes();
        match route_mouse(modes, event.kind) {
            MouseAction::Scroll(delta) => Some(delta),
            _ => None,
        }
    }

    /// Apply one wheel tick to a pane WITHOUT broadcasting, returning the
    /// resulting scroll offset. The drain applies a queued run tick-by-tick so
    /// `vt.scroll`'s per-tick clamp at the history top / live bottom is preserved
    /// - the algebraic net would wrongly cancel a tick that clamped, losing a
    /// reversal at a boundary - then broadcasts once. `0` if the pane is gone.
    fn scroll_tick(&mut self, pane: u64, delta: i32) -> usize {
        self.panes.get_mut(&pane).map_or(0, |e| e.vt.scroll(delta))
    }

    /// The pane's current scroll offset (0 = live bottom), or 0 if it's gone.
    fn scroll_offset(&self, pane: u64) -> usize {
        self.panes.get(&pane).map_or(0, |e| e.vt.display_offset())
    }

    /// The pane's viewport height in rows (0 if gone), the per-fold scroll cap.
    fn pane_rows(&self, pane: u64) -> u16 {
        self.panes.get(&pane).map_or(0, |e| e.vt.size().0)
    }

    /// Apply a single interpreted wheel tick and push one frame (the
    /// per-message path; the drain coalesces a run through `scroll_tick`).
    fn apply_scroll(&mut self, pane: u64, delta: i32) {
        self.scroll_tick(pane, delta);
        self.broadcast_pane(pane);
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
                // Rate-gate ONLY wheel ticks (brief Locked 2): a trackpad flood
                // piles up in the app after the finger stops, so drop stale
                // ticks beyond the budget before the PTY. Press/release/drag/move
                // pass through byte-identical. Gate before the pane borrow (it
                // needs &mut self.wheel_gate); the top-of-fn early return already
                // proved the pane live, so no dead-pane state is ever inserted.
                let forward = match event.kind {
                    MouseKind::WheelUp | MouseKind::WheelDown => {
                        wheel_gate(&mut self.wheel_gate, pane, event.kind, Instant::now())
                    }
                    _ => true,
                };
                if forward {
                    let bytes = sgr_mouse_bytes(&event);
                    if let Some(entry) = self.panes.get(&pane) {
                        let _ = entry.pty.write_input(&bytes);
                    }
                }
            }
            MouseAction::Scroll(delta) => self.apply_scroll(pane, delta),
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
    /// (idempotent) drops the highlight for all. A missing pane, or a step with no
    /// active search, gets a one-line notice to the requester, never a silent
    /// no-op or a panic. Only match counts + coordinates ever leave the server.
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
            // Idempotent: a no-match search_open already dropped the state while
            // the client still sends Clear on Esc, so guarding on has_search here
            // would misfire "no active search" on the common no-match-then-Esc.
            SearchOp::Clear => match self.panes.get_mut(&pane) {
                Some(e) => {
                    e.vt.search_clear();
                    self.broadcast_pane(pane);
                }
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

    /// Write `bytes` to `pane`'s PTY. When `guarded`, apply the same authority
    /// as the block-rerun path (idle badge FIRST, then the writer-claim
    /// interlock) immediately before the write - and because the core loop is
    /// serial, the check and the inject are atomic: no other input for this
    /// pane interleaves between them, so the writer-claim holder cannot start a
    /// burst in the gap. `agents` is the FRESH registry snapshot read off-loop
    /// for this send (not `self.agents`, which is parked with no viewer); `None`
    /// here means the read failed, so the guard fails closed. Raw `PaneSend`
    /// (`guarded == false`) is the writer-claim holder's own channel and stays
    /// unguarded (`agents` is unused).
    fn pane_send(
        &mut self,
        pane: u64,
        bytes: &[u8],
        guarded: bool,
        agents: Option<Vec<RegistryAgent>>,
    ) -> ServerMsg {
        let Some(entry) = self.panes.get(&pane) else {
            return dead_pane(pane);
        };
        if guarded {
            let Some(rows) = agents.as_deref() else {
                return ServerMsg::Err {
                    code: err_code::TARGET_NOT_IDLE,
                    msg: "agents registry unreadable - target agent state unknown".to_string(),
                };
            };
            if let Err(reason) = rerun_allowed(rows, &self.session_name, pane) {
                return ServerMsg::Err {
                    code: err_code::TARGET_NOT_IDLE,
                    msg: reason.to_string(),
                };
            }
            // A live relay holds the pane mid-write: bounce rather than
            // interleave bytes into its burst. A dead holder releases here so
            // the send resumes (mirrors the rerun-path interlock).
            if let Some(&holder) = self.claims.get(&pane) {
                if pid_alive(holder) {
                    return ServerMsg::Err {
                        code: err_code::TARGET_NOT_IDLE,
                        msg: "busy: relay".to_string(),
                    };
                }
                self.claims.remove(&pane);
            }
        }
        match entry.pty.write_input(bytes) {
            Ok(()) => ServerMsg::Ok,
            // A dead/wedged pane fails closed: the child exited (BrokenPipe) or
            // stopped reading (WouldBlock). The send did not land - never a
            // silent Ok.
            Err(e) => ServerMsg::Err {
                code: err_code::DEAD_PANE,
                msg: format!("pane {pane} send failed: {e}"),
            },
        }
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
        // W4 touch telemetry: one submitted answer = one steering action (a
        // bounced answer returned above and never emits).
        self.touch(pane, "answer", false);
    }

    /// (graph node id, squad cwd) for a `human_touch` emit on `pane`. Node id:
    /// the pane's `FNO_NODE` provenance (x-84a8); fallback, the owning squad's
    /// cwd basename when it is node-id shaped (the worktree-per-node
    /// convention). Neither -> None, and the event carries resolution=failed
    /// rather than being dropped (AC4-FR).
    fn pane_touch_provenance(&self, pane: u64) -> (Option<String>, Option<String>) {
        let cwd = self
            .session
            .find_pane(pane)
            .and_then(|(sid, _)| self.session.squad(sid))
            .map(|sq| sq.canonical_cwd().to_string());
        let node = self
            .panes
            .get(&pane)
            .and_then(|e| e.node.clone())
            .or_else(|| {
                cwd.as_deref()
                    .and_then(|c| Path::new(c).file_name())
                    .and_then(|b| b.to_str())
                    .filter(|b| node_id_shaped(b))
                    .map(str::to_owned)
            });
        (node, cwd)
    }

    /// Emit `human_touch` for one steering action on `pane` (W4 touch
    /// telemetry). `coalesced` applies the per-pane window (inject bursts);
    /// answer submits are one emit per action. The write rides the Python
    /// `type` envelope via a fire-and-forget `fno event emit` shell-out (the
    /// x-4e2d digest idiom) - no Rust-side `kind`, so the three-places rule
    /// never applies. The shell-out runs in the squad's cwd so the event
    /// lands in that project's events.jsonl. A failure bumps
    /// `touch_emit_failures` and never touches the steering path (AC4-ERR).
    fn touch(&mut self, pane: u64, source: &'static str, coalesced: bool) {
        if coalesced && !touch_coalesce(&mut self.touch_last_emit, pane, Instant::now()) {
            return;
        }
        // cfg!(test): in unit tests current_exe is the test binary, and
        // exec'ing it with event-emit args would re-enter the test harness.
        // FNO_TOUCH_EMIT=0 is the operator kill switch.
        if cfg!(test) || std::env::var_os("FNO_TOUCH_EMIT").is_some_and(|v| v == "0") {
            return;
        }
        let (node, cwd) = self.pane_touch_provenance(pane);
        let failures = Arc::clone(&self.touch_emit_failures);
        tokio::spawn(async move {
            let resolution = if node.is_some() { "ok" } else { "failed" };
            let data = serde_json::json!({
                "graph_node_id": node,
                "source": source,
                "resolution": resolution,
            })
            .to_string();
            const TOUCH_EMIT_TIMEOUT: Duration = Duration::from_secs(10);
            let mut cmd = tokio::process::Command::new(fno_bin());
            cmd.args([
                "event",
                "emit",
                "--type",
                "human_touch",
                "--source",
                "daemon",
                "--data",
                &data,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            let ok = matches!(
                tokio::time::timeout(TOUCH_EMIT_TIMEOUT, cmd.status()).await,
                Ok(Ok(s)) if s.success()
            );
            if !ok {
                // Counted AND visible (never swallowed): a 100%-failing
                // emitter silently inflates the autonomy rate, so each miss
                // logs to the server's stderr alongside the running total.
                let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
                eprintln!("fno mux: human_touch({source}) emit failed ({n} this session)");
            }
        });
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
                    .map(|s| s.canonical_cwd().to_string())
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
                let pid = tab.focus;
                // Capture membership BEFORE the reap clears it, reconcile AFTER
                // the close settles (so squad-survival is known) - user close
                // de-recruits (AC3-EDGE).
                let ctx = self.member_ctx(pid);
                let flow = self.close_pane(pid);
                self.reconcile_member_close(ctx, false);
                flow
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
                    .map(|s| s.canonical_cwd().to_string())
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
                    name: None,
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
                // De-recruit any member panes in this tab (AC3-EDGE), captured
                // before the reaps clear them; reconciled AFTER remove_tab so
                // squad-survival (survives vs de-persist) reflects reality.
                let ctxs: Vec<_> = pids
                    .iter()
                    .filter_map(|&pid| self.member_ctx(pid))
                    .collect();
                for pid in pids {
                    self.reap_pane(pid);
                }
                let outcome = self.session.remove_tab(sid, ti);
                for ctx in ctxs {
                    self.reconcile_member_close(Some(ctx), false);
                }
                match outcome {
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
            Command::FocusPane(pid) => {
                // Locate the leaf anywhere in the session, then view+focus it.
                let target = self.session.find_pane(pid).map(|(sid, ti)| {
                    (
                        sid,
                        self.session.squad(sid).expect("live squad").tabs[ti].id,
                    )
                });
                match target {
                    Some((sid, tid)) => {
                        self.set_view(client_id, sid, tid);
                        if let Some(tab) = self.viewed_tab_mut((sid, tid)) {
                            tab.focus = pid;
                        }
                        // (x-4328) AC1-HP: focusing a `Done` pane clears its
                        // unseen bit.
                        self.mark_seen_if_done(pid);
                        self.push_layout(true);
                    }
                    // The pane exited racing the click; fail-closed like the
                    // other catalog-named commands.
                    None => self.notice(client_id, "no such pane"),
                }
                Flow::Continue
            }
            Command::AttachAgent { id, placement } => {
                // Validate the jobId shape (8 hex digits) BEFORE it reaches the
                // argv - defense in depth even though spawn_pane_cmd never
                // builds a shell string (the id can only ever be `claude
                // attach`'s positional arg). A malformed id is refused
                // fail-closed, like the other catalog-named commands.
                if id.len() != 8 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    self.notice(client_id, "not an attachable agent");
                    return Flow::Continue;
                }
                // Attach ONLY a session actually surfaced in this sideline: a
                // live watch-only row (paneless, not exited) whose jobId matches
                // - the same catalog-membership refusal FocusPane/SelectTab use,
                // so a stale or never-surfaced id can never drive a spawn. A
                // roster-synthesized foreign row (x-0a2e: mux None, !exited,
                // attach_id set) satisfies this unchanged - it is exactly the
                // watch-only shape the gate was built for.
                if !self.attachable_agent(&id) {
                    self.notice(client_id, "no such agent");
                    return Flow::Continue;
                }
                // (x-9f75) Open-here is inherently "the focused pane of my current view", so a split or a
                // non-CurrentRoute target contradicts it - refuse pre-spawn (AC2-ERR).
                if placement.here
                    && (placement.split.is_some()
                        || !matches!(placement.target, PaneTarget::CurrentRoute))
                {
                    self.notice(client_id, "open-here takes no split or target");
                    return Flow::Continue;
                }
                // (x-0090) Reconcile: an id already mapped to a LIVE pane focuses
                // it instead of minting a duplicate tab (Locked 3; AC2-HP). A
                // stale mapping (pane reaped between reap and here) is dropped and
                // falls through to a fresh spawn. Single-threaded core loop, so
                // click 2 sees click 1's insert (AC2-FR double-action guard).
                if let Some(&pid) = self.attached.get(&id) {
                    if self.panes.contains_key(&pid) {
                        if let Some((sid, ti)) = self.session.find_pane(pid) {
                            let tid = self.session.squad(sid).expect("live squad").tabs[ti].id;
                            self.set_view(client_id, sid, tid);
                            if let Some(tab) = self.viewed_tab_mut((sid, tid)) {
                                tab.focus = pid;
                            }
                            // The focus arm clears a Done pane's unseen bit, like
                            // FocusPane - AttachAgent is no longer spawn-only.
                            self.mark_seen_if_done(pid);
                            // A repeated attach never mints a second pane; the
                            // notice makes the idempotent focus visible (AC3-HP).
                            self.notice(client_id, "already attached; focused existing pane");
                            self.push_layout(true);
                            return Flow::Continue;
                        }
                    }
                    self.attached.remove(&id);
                }
                // (x-9f75) Open-here: repoint the focused viewer pane at B (not a tab/split). Runs after
                // reconcile (an already-paned target focuses, Locked 5 / AC1-EDGE), so B is fresh here.
                if placement.here {
                    // Displacement guard, pre-spawn: only an attach-VIEWER (a value in `attached`) is
                    // displaceable - displacing a direct/shell pane would kill its live PTY child. Reading
                    // the CURRENT focus (re-anchored after an exit) is what makes AC2-FR hold. (Locked 3, AC1-ERR)
                    let Some(focus) = self.viewed_tab(view).map(|t| t.focus) else {
                        self.notice(client_id, "no focused pane to open here");
                        return Flow::Continue;
                    };
                    let displaced = self
                        .attached
                        .iter()
                        .find(|(_, &p)| p == focus)
                        .map(|(k, _)| k.clone());
                    let Some(displaced_id) = displaced else {
                        self.notice(
                            client_id,
                            "focused pane is not a detachable viewer - use split or new tab",
                        );
                        return Flow::Continue;
                    };
                    // Anchor the spawn at B's row cwd, else the viewed squad's cwd (same rule as a fresh attach).
                    let row_cwd = self
                        .agents
                        .iter()
                        .find(|a| {
                            a.mux.is_none() && !a.exited && a.attach_id.as_deref() == Some(&id)
                        })
                        .map(|a| a.cwd.clone())
                        .unwrap_or_default();
                    let spawn_cwd = if row_cwd.is_empty() {
                        self.session
                            .squad(view.0)
                            .map(|s| s.canonical_cwd().to_string())
                            .unwrap_or_default()
                    } else {
                        row_cwd
                    };
                    let (rows, cols) = self
                        .clients
                        .iter()
                        .find(|c| c.id == client_id)
                        .map(|c| c.dims)
                        .unwrap_or((vp.rows, vp.cols));
                    // Spawn-first (Locked 4): a spawn failure leaves the layout untouched (AC3-ERR).
                    let (acct, cd) = self.attach_account_ctx(&id);
                    let argv = attach_argv(&id, acct.as_deref(), cd.as_deref());
                    let new_pid = match self.spawn_pane_cmd(&argv, rows, cols, &spawn_cwd) {
                        Ok(p) => p,
                        Err(e) => {
                            self.notice(client_id, format!("attach failed: {e}"));
                            return Flow::Continue;
                        }
                    };
                    // Swap-second: replace_leaf repoints the focused leaf at the new viewer, moving focus with it.
                    let Some(tab) = self.viewed_tab_mut(view) else {
                        self.reap_pane(new_pid);
                        self.notice(client_id, "view changed; open-here aborted");
                        return Flow::Continue;
                    };
                    if !tree::replace_leaf(tab, focus, new_pid) {
                        // Focus raced out of the tree (a pane exit): the new viewer has nowhere to land.
                        self.reap_pane(new_pid);
                        self.notice(client_id, "focused pane changed; open-here aborted");
                        return Flow::Continue;
                    }
                    // Insert BEFORE the reap: reap_pane drops every mapping onto `focus`, so inserting first
                    // clears A (it resurfaces watch-only) while B's mapping (new_pid != focus) survives.
                    self.attached.insert(id, new_pid);
                    // Reap-last (Locked 4): F's viewer dies but the displaced session keeps running detached
                    // and resurfaces watch-only (x-7561 external-lifecycle - viewport moved, nothing killed).
                    self.reap_pane(focus);
                    self.notice(
                        client_id,
                        format!("opened here; {displaced_id} detached (watch-only)"),
                    );
                    self.push_layout(true);
                    return Flow::Continue;
                }
                // The watch-only row's OWN cwd anchors the attach process
                // (AC8-EDGE): squad target selection never rewrites it, so an
                // origin-less named target still starts claude in the agent's
                // dir. Captured before target resolution because owner routing
                // and the spawn cwd both derive from this one row.
                let row_cwd = self
                    .agents
                    .iter()
                    .find(|a| a.mux.is_none() && !a.exited && a.attach_id.as_deref() == Some(&id))
                    .map(|a| a.cwd.clone())
                    .unwrap_or_default();
                // Resolve the OWNING squad (Locked 2) as the CurrentRoute
                // default: the squad whose `owns_path` matches the row cwd, so
                // the attach lands where the agent lives, not the viewer's
                // squad; fall back to the viewed squad for an orphan (AC1-EDGE).
                let owner = self
                    .session
                    .squads
                    .iter()
                    .find(|s| !row_cwd.is_empty() && s.owns_path(&row_cwd))
                    .map(|s| s.id)
                    .unwrap_or(view.0);
                // An explicit UI target overrides owner routing for a fresh
                // attach (Locked 7); a stale/unknown target fails closed with no
                // spawn (AC4). Owner is always live, so resolution yields Some.
                let dest = match self.resolve_placement_target(&placement.target, Some(owner)) {
                    Ok(d) => d,
                    Err(e) => {
                        self.notice(client_id, e);
                        return Flow::Continue;
                    }
                };
                // Spawn `claude attach <id>`: the claude supervisor PTYs the
                // detached bg session into this pane. cwd is the agent row's,
                // falling back to the owner squad's only when the row lacks one.
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                let spawn_cwd = if row_cwd.is_empty() {
                    self.session
                        .squad(owner)
                        .map(|s| s.canonical_cwd().to_string())
                        .unwrap_or_default()
                } else {
                    row_cwd
                };
                let (acct, cd) = self.attach_account_ctx(&id);
                let argv = attach_argv(&id, acct.as_deref(), cd.as_deref());
                let pid = match self.spawn_pane_cmd(&argv, rows, cols, &spawn_cwd) {
                    Ok(p) => p,
                    Err(e) => {
                        self.notice(client_id, format!("attach failed: {e}"));
                        return Flow::Continue;
                    }
                };
                // Place through the shared helper (Locked 1): a new tab, or a
                // directional split beside the selected squad's active-tab
                // focus. A
                // refusal reaps the pane and leaves the row watch-only (AC7);
                // the mapping is recorded ONLY after placement succeeds.
                let (sid, tid, fell_back) =
                    match self.place_spawned_pane(dest, &spawn_cwd, pid, placement.split) {
                        Ok(landing) => landing,
                        Err(e) => {
                            self.notice(client_id, e);
                            return Flow::Continue;
                        }
                    };
                self.attached.insert(id, pid);
                self.set_view(client_id, sid, tid);
                if fell_back {
                    self.notice(client_id, "tab full - opened as tab");
                }
                self.push_layout(true);
                Flow::Continue
            }
            Command::DispatchNode { node, account } => {
                // Targeted work-queue dispatch (a clicked card, x-a496). Reuses
                // the leader+g porcelain pinned to `--node`; the claim race
                // (already-worked node bounces `already-dispatching`) and lane
                // cap live in `fno dispatch one`. Routes through CoreMsg::Command,
                // so the read-only-observer refusal already fired upstream.
                //
                // Re-check readiness against the server's OWN backlog snapshot
                // (codex peer review): the client already gates the confirm to a
                // ready card, but the server's snapshot is fresher, so a card that
                // went blocked/in-flight between the client's Layout and the click
                // is refused here - it must not start work leader+g would never
                // pick. An unknown or non-ready id fails closed to a notice, like
                // the other catalog-named commands (and covers an empty id).
                if card_ready_to_dispatch(&self.backlog, &node) {
                    self.dispatch_next(client_id, Some(node), account);
                } else if let Some(route) = self.inflight_route(&node) {
                    // The client's Layout was stale: the card went in-flight
                    // between publish and click, but the server can route it -
                    // focus/attach instead of refusing (x-54fa AC2-ERR). The
                    // recursion reuses the FocusPane/AttachAgent gates verbatim
                    // (catalog membership, jobId shape), so this adds no second
                    // spawn path.
                    return self.command(client_id, route);
                } else if let Some(hint) = self.inflight_hint(&node) {
                    // In flight but unroutable: say where the work is, the
                    // same copy a routed v18 card click would show.
                    self.notice(client_id, hint);
                } else {
                    self.notice(client_id, "card not ready to dispatch");
                }
                Flow::Continue
            }
            Command::NewSquad { name, origin } => {
                // Explicit named-workspace creation (Unit 2). A blank/whitespace
                // name is refused fail-closed - nothing is created (AC1-ERR,
                // epic Boundaries), same shape as the other catalog commands.
                let name = name.trim();
                if name.is_empty() {
                    self.notice(client_id, "name required");
                    return Flow::Continue;
                }
                // Name is the durable identity (Locked Decision 4): a duplicate
                // of a live OR persisted named squad is refused fail-closed,
                // same shape as the blank refusal.
                if self.named_squad_taken(name) {
                    self.notice(client_id, "name taken");
                    return Flow::Continue;
                }
                // Seed the shell at the given origin, else the sender's current
                // squad cwd (a new workspace opens where you are). PTY-first
                // ordering (Locked 7): a spawn failure mutates no model. Consume
                // `origin` into the origins vec once, then seed the shell at
                // origins[0] (else the sender's current squad cwd).
                let origins: Vec<String> = origin.into_iter().collect();
                let cwd = origins.first().cloned().unwrap_or_else(|| {
                    self.session
                        .squad(view.0)
                        .map(|s| s.canonical_cwd().to_string())
                        .unwrap_or_default()
                });
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                let pid = match self.spawn_pane(rows, cols, &cwd) {
                    Ok(p) => p,
                    Err(e) => {
                        self.notice(client_id, format!("new workspace failed: {e}"));
                        return Flow::Continue;
                    }
                };
                let sid = self.next_squad_id;
                self.next_squad_id += 1;
                let tid = self.session.mint_tab_id();
                self.session.add_squad(
                    sid,
                    origins,
                    Some(name.to_string()),
                    Tab {
                        name: None,
                        id: tid,
                        root: Node::Leaf(pid),
                        focus: pid,
                    },
                );
                // Track + persist the new (empty-membership) workspace so it
                // restores next session even before any recruit (AC1-EDGE).
                self.squad_members.insert(sid, Vec::new());
                self.persist_squad(sid);
                self.set_view(client_id, sid, tid);
                self.push_layout(true);
                Flow::Continue
            }
            Command::RenameTab { tab, name } => {
                // Explicit tab rename (x-c150). A stale/unknown id (the tab
                // closed racing the overlay) is refused fail-closed with a
                // notice, like SelectTab - no mutation.
                match self.session.find_tab(tab) {
                    Some((sid, ti)) => {
                        let clean = sanitize_tab_name(&name);
                        let t = &mut self
                            .session
                            .squad_mut(sid)
                            .expect("find_tab live squad")
                            .tabs[ti];
                        // Blank-after-sanitize CLEARS the rename back to the
                        // derived label (Locked 2: "reset to auto" is a
                        // meaningful rename target).
                        t.name = (!clean.is_empty()).then_some(clean);
                        self.push_layout(true);
                    }
                    None => self.notice(client_id, "no such tab"),
                }
                Flow::Continue
            }
            Command::RenameSquad { squad, name } => {
                // Explicit squad rename (x-96e8). A stale/unknown id (the squad
                // died racing the overlay) is refused fail-closed with a notice,
                // like SelectSquad - no mutation.
                match self.session.squad(squad) {
                    Some(sq) => {
                        let clean = sanitize_name(&name, MAX_SQUAD_NAME);
                        // Blank-after-sanitize CLEARS back to the derived label
                        // (the RenameTab precedent) - EXCEPT an origin-less
                        // squad, whose derived label would be empty: there a
                        // blank is refused (nothing to fall back to).
                        if clean.is_empty() && sq.origins.is_empty() {
                            self.notice(client_id, "name required");
                            return Flow::Continue;
                        }
                        let new_name = (!clean.is_empty()).then_some(clean);
                        let old_name = sq.name.clone();
                        // Uniqueness (Locked 4): renaming onto a DIFFERENT live
                        // or persisted name is refused, like NewSquad.
                        if let Some(nn) = new_name.as_deref() {
                            if old_name.as_deref() != Some(nn) && self.named_squad_taken(nn) {
                                self.notice(client_id, "name taken");
                                return Flow::Continue;
                            }
                        }
                        // Snapshot membership once (a rename never changes it);
                        // its presence is what marks the squad persisted (gemini
                        // review: one lookup, not contains_key + get).
                        let tracked_members = self.squad_members.get(&squad).cloned();
                        self.session
                            .squad_mut(squad)
                            .expect("squad() live above")
                            .name = new_name.clone();
                        self.push_layout(true);
                        // Write-through only for persisted (tracked) squads. The
                        // store is name-keyed: a rename is ONE atomic delete-old
                        // + upsert-new (so a concurrent restore never sees a
                        // window with neither name); a CLEAR turns the workspace
                        // unnamed, so it leaves the store entirely.
                        if let Some(members) = tracked_members {
                            match (old_name, new_name) {
                                (Some(old), Some(new)) => {
                                    let origins = self
                                        .session
                                        .squad(squad)
                                        .map(|s| s.origins.clone())
                                        .unwrap_or_default();
                                    if let Err(e) =
                                        crate::squad_store::rename(&old, &new, &origins, &members)
                                    {
                                        self.persist_degraded(&e);
                                    }
                                }
                                (Some(old), None) => {
                                    self.persist_remove_name(&old);
                                    self.squad_members.remove(&squad);
                                }
                                // A tracked squad is always named, so these arms
                                // are unreachable in practice; upsert is the safe
                                // default if one ever occurs.
                                (None, _) => self.persist_squad(squad),
                            }
                        }
                    }
                    None => self.notice(client_id, "no such squad"),
                }
                Flow::Continue
            }
            Command::RemoveSquad(id) => {
                // Close a whole workspace (x-96e8): reap every pane across all
                // its tabs, drop the squad, re-anchor views. Destructiveness is
                // gated client-side by a confirm; the server just executes.
                let Some(pos) = self.session.squads.iter().position(|s| s.id == id) else {
                    self.notice(client_id, "no such squad");
                    return Flow::Continue;
                };
                // De-persist the whole workspace up front (user dismissed it -
                // it must not return at restart). Reaping its member panes below
                // then no-ops on the store (the entry and tracking are gone).
                if let Some(name) = self.session.squads[pos].name.clone() {
                    if self.squad_members.remove(&id).is_some() {
                        self.persist_remove_name(&name);
                    }
                }
                let pids: Vec<u64> = self.session.squads[pos]
                    .tabs
                    .iter()
                    .flat_map(|t| tree::leaves(&t.root))
                    .collect();
                let tids: Vec<TabId> = self.session.squads[pos].tabs.iter().map(|t| t.id).collect();
                for pid in pids {
                    self.reap_pane(pid);
                }
                self.session.squads.remove(pos);
                for tid in tids {
                    self.tab_areas.remove(&tid);
                }
                // The last squad ends the session (Locked Decision 8), exactly
                // like closing its tabs one at a time.
                if self.session.squads.is_empty() {
                    self.session.active_squad = None;
                    return Flow::Shutdown;
                }
                if self.session.active_squad == Some(id) {
                    self.session.active_squad = Some(self.session.squads[0].id);
                }
                self.reanchor_views();
                self.push_layout(true);
                Flow::Continue
            }
            Command::MoveSquad { squad, delta } => {
                // Reorder the sideline (x-96e8). Pure presentation move: clamp
                // to the list bounds; an already-at-edge move is a silent no-op
                // (holding a reorder key at the top must not bell).
                let Some(idx) = self.session.squads.iter().position(|s| s.id == squad) else {
                    self.notice(client_id, "no such squad");
                    return Flow::Continue;
                };
                let len = self.session.squads.len() as i64;
                let new = (idx as i64 + delta as i64).clamp(0, len - 1) as usize;
                if new != idx {
                    let sq = self.session.squads.remove(idx);
                    self.session.squads.insert(new, sq);
                    self.push_layout(true);
                }
                Flow::Continue
            }
            Command::MoveTab { tab, squad } => {
                // Re-home a whole tab into another squad (x-96e8). move_tab does
                // the pure data surgery; the view fixup lives here.
                match self.session.move_tab(tab, squad) {
                    MoveTabOutcome::Refused(msg) => self.notice(client_id, msg),
                    outcome => {
                        // A viewer watching the moved tab FOLLOWS it into dst
                        // (content continuity beats spatial position); set_view
                        // validates the (dst, tab) pair.
                        let movers: Vec<u64> = self
                            .clients
                            .iter()
                            .filter(|c| c.view.1 == tab)
                            .map(|c| c.id)
                            .collect();
                        for cid in movers {
                            self.set_view(cid, squad, tab);
                        }
                        // Source squad died (its last tab moved out): its other
                        // viewers, if any, re-anchor to a survivor.
                        if matches!(outcome, MoveTabOutcome::MovedSquadRemoved) {
                            self.reanchor_views();
                        }
                        self.push_layout(true);
                    }
                }
                Flow::Continue
            }
            Command::ReorderTab { squad, tab, delta } => {
                let Some((current_squad, idx)) = self.session.find_tab(tab) else {
                    self.notice(client_id, "no such tab");
                    return Flow::Continue;
                };
                if current_squad != squad {
                    self.notice(client_id, "tab moved to another workspace");
                    return Flow::Continue;
                }
                let changed = {
                    let squad = self.session.squad_mut(squad).expect("find_tab live squad");
                    let new =
                        (idx as i64 + delta as i64).clamp(0, squad.tabs.len() as i64 - 1) as usize;
                    if new == idx {
                        false
                    } else {
                        let active = squad.tabs.get(squad.active_tab).map(|tab| tab.id);
                        let moved = squad.tabs.remove(idx);
                        squad.tabs.insert(new, moved);
                        squad.active_tab = active
                            .and_then(|id| {
                                squad.tabs.iter().position(|candidate| candidate.id == id)
                            })
                            .unwrap_or_else(|| {
                                squad.active_tab.min(squad.tabs.len().saturating_sub(1))
                            });
                        true
                    }
                };
                if changed {
                    self.push_layout(true);
                }
                Flow::Continue
            }
            Command::RecruitAgents { squad, ids } => {
                // Bulk recruit (x-8f11 US3). The server is the authoritative
                // gate: blank name / empty ids refused fail-closed; each id
                // re-validated through the exact AttachAgent gates; dedup no-op
                // for an already-paned or already-member id; one write-through.
                let name = squad.trim().to_string();
                if name.is_empty() {
                    self.notice(client_id, "name required");
                    return Flow::Continue;
                }
                if ids.is_empty() {
                    self.notice(client_id, "no agents selected");
                    return Flow::Continue;
                }
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                // Target the live named squad if one exists, else create it lazily
                // on the first successful recruit (no empty squad on all-skip).
                let mut sid = self
                    .session
                    .squads
                    .iter()
                    .find(|s| s.name.as_deref() == Some(name.as_str()))
                    .map(|s| s.id);
                // A name that exists ONLY in the persisted store (another mux
                // server created it after this server's one-time restore, or
                // restore skipped it on a spawn failure) must NOT be re-created
                // here: the create path would upsert by name and DROP that
                // entry's persisted members (codex review). Refuse fail-closed,
                // like NewSquad - restore/attach it first, or pick another name.
                if sid.is_none()
                    && crate::squad_store::load()
                        .squads
                        .iter()
                        .any(|s| s.name == name)
                {
                    self.notice(
                        client_id,
                        "name taken by a persisted workspace not restored here",
                    );
                    return Flow::Continue;
                }
                let mut recruited = 0usize;
                let mut skipped: Vec<String> = Vec::new();
                for id in &ids {
                    if id.len() != 8 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                        skipped.push(format!("{id} (bad id)"));
                        continue;
                    }
                    if !self.attachable_agent(id) {
                        skipped.push(format!("{id} (not attachable)"));
                        continue;
                    }
                    // Dedup (AC2-EDGE): an id with a live pane, or already a
                    // member of the target squad, is a no-op counted as skipped.
                    let already_member = sid
                        .and_then(|s| self.squad_members.get(&s))
                        .is_some_and(|ms| ms.iter().any(|m| m.attach_id == *id));
                    if self.attached.contains_key(id) || already_member {
                        skipped.push(format!("{id} (already recruited)"));
                        continue;
                    }
                    let cwd = sid
                        .and_then(|s| self.session.squad(s))
                        .map(|s| s.canonical_cwd().to_string())
                        .unwrap_or_default();
                    let (acct, cd) = self.attach_account_ctx(&id);
                    let argv = attach_argv(&id, acct.as_deref(), cd.as_deref());
                    let pid = match self.spawn_pane_cmd(&argv, rows, cols, &cwd) {
                        Ok(p) => p,
                        Err(e) => {
                            skipped.push(format!("{id} ({e})"));
                            continue;
                        }
                    };
                    let tid = self.session.mint_tab_id();
                    let tab = Tab {
                        name: None,
                        id: tid,
                        root: Node::Leaf(pid),
                        focus: pid,
                    };
                    match sid {
                        Some(s) => {
                            self.session
                                .squad_mut(s)
                                .expect("target squad live")
                                .tabs
                                .push(tab);
                        }
                        None => {
                            // Create the named workspace (no origin) with this as
                            // its first tab.
                            let ns = self.next_squad_id;
                            self.next_squad_id += 1;
                            self.session
                                .add_squad(ns, Vec::new(), Some(name.clone()), tab);
                            self.squad_members.insert(ns, Vec::new());
                            sid = Some(ns);
                        }
                    }
                    let s = sid.expect("set above");
                    self.attached.insert(id.clone(), pid);
                    self.squad_members.entry(s).or_default().push(
                        crate::squad_store::StoredMember {
                            attach_id: id.clone(),
                            tombstone: false,
                        },
                    );
                    recruited += 1;
                }
                if recruited > 0 {
                    let s = sid.expect("recruited > 0 implies a squad");
                    self.persist_squad(s);
                    // Show the operator their new team: switch to the target
                    // squad's active tab.
                    if let Some(sq) = self.session.squad(s) {
                        let tid = sq
                            .tabs
                            .get(sq.active_tab)
                            .or_else(|| sq.tabs.first())
                            .map(|t| t.id);
                        if let Some(tid) = tid {
                            self.set_view(client_id, s, tid);
                        }
                    }
                }
                let msg = if skipped.is_empty() {
                    format!("recruited {recruited}")
                } else {
                    format!(
                        "recruited {recruited}, skipped {}: {}",
                        skipped.len(),
                        skipped.join(", ")
                    )
                };
                self.notice(client_id, msg);
                self.push_layout(true);
                Flow::Continue
            }
            Command::DismissMember { squad, attach_id } => {
                // Dismiss a TOMBSTONED member from a persisted workspace (x-8f11
                // US4). Only a tombstone is dismissable - a live member leaves by
                // closing its pane. Unknown workspace/member is refused.
                let Some(members) = self.squad_members.get_mut(&squad) else {
                    self.notice(client_id, "no such workspace");
                    return Flow::Continue;
                };
                let before = members.len();
                members.retain(|m| !(m.attach_id == attach_id && m.tombstone));
                if members.len() == before {
                    self.notice(client_id, "no such tombstoned member");
                    return Flow::Continue;
                }
                self.persist_squad(squad);
                self.push_layout(true);
                Flow::Continue
            }
            Command::StopAgent { name } => {
                // Stop a live sideline row (x-76ea). `resolve_lifecycle_target`
                // validates the name against THIS server's catalog fail-closed;
                // the shell is idempotent (already-exited is a clean no-op), so
                // the exited flag is unused here.
                match self.resolve_lifecycle_target(&name) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(_row) => self.agent_action(client_id, "stop", name),
                }
                Flow::Continue
            }
            Command::RemoveAgent { name } => {
                // Remove an exited sideline row (x-76ea). Same resolution as
                // StopAgent, plus the stop-then-rm ordering: a still-live row is
                // refused with the stop-first reason (the CLI enforces this too,
                // but refusing here keeps the notice specific and skips a doomed
                // subprocess).
                // `.map(|a| a.exited)` drops the row borrow before the arm bodies
                // call `&mut self` methods (the resolver now returns a reference).
                match self.resolve_lifecycle_target(&name).map(|a| a.exited) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(false) => {
                        self.notice(client_id, format!("{name} is still live - stop it first"))
                    }
                    Ok(true) => self.agent_action(client_id, "rm", name),
                }
                Flow::Continue
            }
            Command::PeekAgent { name, seq } => {
                // Read-only transcript fetch (x-c376): shell `fno agents peek`
                // off-loop and reply to this client only. No catalog validation -
                // an unknown name returns peek's own exit-13 body, which the
                // overlay renders (fail-open, never a refusal notice).
                self.peek_agent(client_id, name, seq);
                Flow::Continue
            }
            Command::ReapAgents => {
                // Bulk-reap every exited fno-agent registry row (x-7561). The
                // requester is already known-interactive (the `mutating_sender`
                // gate drops a passive client's Command upstream). The reap verb
                // owns the candidate set, so there is no per-row resolution and
                // zero candidates is a visible successful `reaped 0`. Off-loop +
                // bounded, mirroring `agent_action`; the registry poll owns the
                // row-vanish, this notice is advisory. The immediate `reaping…`
                // notice gives visible in-flight feedback (codex P2) before the
                // up-to-20s subprocess, since reap has no row-level state.
                self.notice(client_id, "reaping exited agents…");
                self.reap_action(client_id);
                Flow::Continue
            }
            Command::StopExternal { attach_id, name: _ } => {
                // Stop a live external claude-daemon row (or retry a failed/unknown
                // tombstone) by stable attach id (x-7561). Validate the id names
                // an actionable external target NOW (AC1-ERR stale refusal), then
                // the durable CAS gates the spawn.
                if !crate::squad_store::valid_attach_id(&attach_id) {
                    // The id rides from the client (which read it off an
                    // unvalidated roster row); reject a non-8-hex value before it
                    // is persisted or reaches the `claude stop` argv (codex P2 -
                    // a dash-prefixed id could be read as a CLI option).
                    self.notice(client_id, "invalid external id");
                    return Flow::Continue;
                }
                match self.resolve_external_stop_target(&attach_id) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok((rname, cwd)) => {
                        match crate::squad_store::begin_external_stop(&attach_id, &rname, &cwd) {
                            Err(e) => self.persist_degraded(&e),
                            Ok(crate::squad_store::LifecycleCas::Refused(r)) => {
                                self.notice(client_id, r)
                            }
                            Ok(crate::squad_store::LifecycleCas::Committed { generation }) => {
                                self.refresh_external_lifecycle();
                                self.notice(client_id, format!("stopping {rname}…"));
                                self.external_action(
                                    client_id,
                                    "stop",
                                    attach_id,
                                    generation,
                                    crate::squad_store::ExternalState::Stopping,
                                );
                            }
                        }
                    }
                }
                Flow::Continue
            }
            Command::RemoveExternal { attach_id, name } => {
                // Remove a STOPPED external tombstone by attach id (x-7561). No
                // live-row lookup - the target is a persisted tombstone; the CAS
                // itself is the stop-before-rm gate (refuses any non-stopped
                // state with a specific reason).
                if !crate::squad_store::valid_attach_id(&attach_id) {
                    self.notice(client_id, "invalid external id");
                    return Flow::Continue;
                }
                match crate::squad_store::begin_external_rm(&attach_id) {
                    Err(e) => self.persist_degraded(&e),
                    Ok(crate::squad_store::LifecycleCas::Refused(r)) => self.notice(client_id, r),
                    Ok(crate::squad_store::LifecycleCas::Committed { generation }) => {
                        self.refresh_external_lifecycle();
                        self.notice(client_id, format!("removing {name}…"));
                        self.external_action(
                            client_id,
                            "rm",
                            attach_id,
                            generation,
                            crate::squad_store::ExternalState::Removing,
                        );
                    }
                }
                Flow::Continue
            }
            Command::MailAgent { name, text } => {
                // (x-9c5f) Free-text reply from peek (`m`). Resolve fail-closed
                // (mail to an EXITED row is legal - it queues durable; an external
                // row is refused), sanitize the text (blank/over-cap refused,
                // never truncated), then shell `fno mail send` off-loop.
                match self.resolve_lifecycle_target(&name) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(_) => match sanitize_mail_text(&text) {
                        Err(msg) => self.notice(client_id, msg),
                        Ok(clean) => self.mail_agent(client_id, name, clean),
                    },
                }
                Flow::Continue
            }
            Command::RespawnAgent { name } => {
                // (x-9c5f) Respawn an exited claude bg row from peek (`r`). Copy
                // the two fields out via `.map` so the row borrow is dropped
                // before the arm bodies touch `&mut self`. Refuse a still-live
                // row, a uuid-less row (also covers non-claude - derive_rows only
                // carries the uuid for claude), and a malformed uuid (shape gate
                // before argv); else shell `fno agents spawn --resume` off-loop.
                // Carry the row's cwd + account too: `fno agents spawn` defaults
                // to the CANONICAL checkout (x-85fe), NOT the row's worktree, so a
                // cross-worktree revival must pass `--cwd <recorded>` or it comes
                // back in main; an isolated-account session needs `--account`
                // (its uuid lives in that account's config dir). The row is the
                // only source of these - they are not on the `--resume` uuid.
                let resolved = self.resolve_lifecycle_target(&name).map(|a| {
                    (
                        a.exited,
                        a.claude_session_uuid.clone(),
                        a.cwd.clone(),
                        a.account.clone(),
                    )
                });
                match resolved {
                    Err(msg) => self.notice(client_id, msg),
                    Ok((false, ..)) => self.notice(client_id, format!("{name} is still live")),
                    Ok((true, None, ..)) => self.notice(
                        client_id,
                        format!("{name}: no claude session recorded - cannot respawn"),
                    ),
                    Ok((true, Some(uuid), cwd, account)) => {
                        if valid_session_uuid(&uuid) {
                            self.respawn_agent(client_id, name, uuid, cwd, account);
                        } else {
                            self.notice(
                                client_id,
                                format!("{name}: malformed session id - cannot respawn"),
                            );
                        }
                    }
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

    /// Re-publish `clients.len()` to the periodic readers. `send_if_modified`
    /// so a no-change pass wakes nobody.
    fn publish_client_count(&self) {
        let n = self.clients.len();
        self.client_count.send_if_modified(|c| {
            let changed = *c != n;
            *c = n;
            changed
        });
    }

    fn handle(&mut self, msg: CoreMsg) -> Flow {
        let flow = self.handle_msg(msg);
        // Choke point: every message-driven `clients` mutation (attach,
        // detach, Gone, dead-client sweeps under push_layout) has returned
        // by here. The main-loop tail covers the non-message sweeps.
        self.publish_client_count();
        flow
    }

    fn handle_msg(&mut self, msg: CoreMsg) -> Flow {
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
                    // W4 touch telemetry: a keystroke past the relay guard is
                    // a human steering this pane; PaneSend (script API) and
                    // relay writes never reach here.
                    self.touch(focus, "inject", true);
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
                e2e_log(format_args!("resize client {id} -> {rows}x{cols}"));
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
            CoreMsg::DispatchNext { id, account } => {
                self.dispatch_next(id, None, account);
                Flow::Continue
            }
            CoreMsg::DispatchResult { id, notice } => {
                if !notice.is_empty() {
                    self.notice(id, notice);
                }
                Flow::Continue
            }
            CoreMsg::PeekResult {
                id,
                seq,
                name,
                lines,
            } => {
                // Route the shelled transcript to the requesting client only
                // (x-c376). A vanished client (detached before the read finished)
                // is a silent no-op - the reply just drops.
                if let Some(c) = self.clients.iter().find(|c| c.id == id) {
                    let _ = c
                        .reliable_tx
                        .try_send(ServerMsg::PeekBody { seq, name, lines });
                }
                Flow::Continue
            }
            CoreMsg::ExternalLifecycleSync {
                to,
                records,
                notices,
            } => {
                // (x-7561) An off-loop external action / reconcile finished: swap
                // in the fresh render snapshot, re-push the sideline, and surface
                // the outcome (to one client for an action, to all for a
                // reconcile). This is the ONLY writer of `external_lifecycle` on
                // the core loop, so a stale action's late sync just re-renders
                // the durable truth it already re-read.
                self.external_lifecycle = records;
                self.push_layout(true);
                for n in notices {
                    match to {
                        Some(cid) => self.notice(cid, n),
                        None => self.notice_all(n),
                    }
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
                e2e_log(format_args!("client {id} gone"));
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
                placement,
                reply,
            } => {
                let rows = rows.unwrap_or(vt::DEFAULT_ROWS);
                let cols = cols.unwrap_or(vt::DEFAULT_COLS);
                let msg = match self.run_pane(squad_key, cwd, argv, rows, cols, claim, placement) {
                    Ok(pane_id) => ServerMsg::PaneSpawned { pane_id },
                    Err(e) => ServerMsg::Err {
                        code: err_code::SPAWN_FAILED,
                        msg: e,
                    },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneSend {
                pane,
                bytes,
                guarded,
                agents,
                reply,
            } => {
                let msg = self.pane_send(pane, &bytes, guarded, agents);
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
            CoreMsg::AgentRows { rows, branches } => {
                self.agents = rows;
                self.branch_by_cwd = branches;
                // Rects are unchanged; only the Layout's agent rows moved -
                // push without re-emitting frames (AC1-UI: visible within one
                // layout push; AC2-UI: the read happened off-loop).
                self.push_layout(false);
                Flow::Continue
            }
            CoreMsg::BacklogCards {
                cards,
                holders,
                prs,
            } => {
                // Same as AgentRows: only sideline data moved, so push the
                // Layout without a frame re-emit (x-6f77).
                self.backlog = cards;
                self.backlog_holders = holders;
                self.backlog_pr = prs;
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

/// (x-cd67 US4) Join a resolved branch and a cwd tail into a sideline subline.
/// `<branch> · <tail>`; a missing branch leaves the tail alone, an empty cwd
/// (no tail, no branch) yields `None` so no sub-row is emitted (AC1-EDGE).
fn subline_from(branch: Option<&str>, cwd: &str) -> Option<String> {
    // `Path::file_name` handles trailing slashes and platform separators (gemini
    // review); an empty cwd yields no tail.
    let tail = Path::new(cwd).file_name().and_then(|s| s.to_str());
    match (branch, tail) {
        (Some(b), Some(t)) => Some(format!("{b} · {t}")),
        (Some(b), None) => Some(b.to_string()),
        (None, Some(t)) => Some(t.to_string()),
        (None, None) => None,
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
    // Attached-client count for the periodic readers (x-4e30): Core owns the
    // sender; each reader holds a receiver as its work gate + 0->1 wakeup.
    let (client_count_tx, client_count_rx) = watch::channel(0usize);

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
        branch_by_cwd: HashMap::new(),
        backlog: Vec::new(),
        backlog_holders: HashMap::new(),
        backlog_pr: HashMap::new(),
        claim_eligible: HashSet::new(),
        claims: HashMap::new(),
        touch_last_emit: HashMap::new(),
        wheel_gate: HashMap::new(),
        touch_emit_failures: Arc::new(AtomicU64::new(0)),
        client_count: client_count_tx,
        seen: HashSet::new(),
        attached: HashMap::new(),
        squad_members: HashMap::new(),
        external_lifecycle: Vec::new(),
        persist_degraded_notified: false,
        restored: false,
    };

    // The off-loop registry reader (4a-G2): a 1s interval task stats/reads
    // BOTH the fno-agents registry AND claude's daemon roster (x-0a2e) on the
    // blocking pool, unions them into the agent row set (TTL aging + roster
    // liveness upgrade + foreign rows included), and sends it to the core only
    // when the MERGED set changed. Each file is behind its own mtime+len gate,
    // so a roster-only change publishes and an idle tick reads nothing. The
    // render path never touches either file (AC2-UI; the origin freeze class),
    // and staleness stays bounded by this one interval.
    {
        let core_tx = core_tx.clone();
        let reg_path = agents_view::registry_path();
        let roster_path = agents_view::roster_path();
        let mut count_rx = client_count_rx.clone();
        tokio::spawn(async move {
            let mut state = agents_view::ReaderState::default();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Stat + conditional read of one file behind an mtime+len gate,
            // both off the core loop. Returns (fresh stamp, raw-if-changed).
            async fn scan(
                path: std::path::PathBuf,
                cached: Option<(std::time::SystemTime, u64)>,
            ) -> (Option<(std::time::SystemTime, u64)>, Option<String>) {
                let stat_path = path.clone();
                let stamp = tokio::task::spawn_blocking(move || {
                    std::fs::metadata(&stat_path)
                        .ok()
                        .map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()))
                })
                .await
                .ok()
                .flatten();
                let raw = if stamp != cached {
                    tokio::task::spawn_blocking(move || std::fs::read_to_string(&path).ok())
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                (stamp, raw)
            }
            loop {
                // Gate the registry+roster read on an attached client (x-4e30).
                // The `changed()` arm IS the 0->1 kick: an attach wakes the
                // parked reader at once so the first overlay is fresh (AC3-FR).
                tokio::select! {
                    _ = tick.tick() => {}
                    res = count_rx.changed() => {
                        if res.is_err() {
                            return; // Core dropped; server shutting down
                        }
                    }
                }
                if *count_rx.borrow() == 0 {
                    continue; // no viewer -> skip both file reads entirely
                }
                let (reg_stamp, reg_raw) = scan(reg_path.clone(), state.reg_stamp()).await;
                let (roster_stamp, roster_raw) =
                    scan(roster_path.clone(), state.roster_stamp()).await;
                // (x-c914) Each registered isolated account's roster.json, folded
                // into the union tagged by account (managed accounts share
                // ~/.claude and add no dir). The config re-read is tiny and
                // gated on an attached viewer; each roster read is stamp-gated
                // per dir by `isolated_stamp`, so only a changed dir re-reads.
                let iso_paths = tokio::task::spawn_blocking(agents_view::isolated_roster_paths)
                    .await
                    .unwrap_or_default();
                let mut isolated = Vec::with_capacity(iso_paths.len());
                for (account, path) in iso_paths {
                    let (stamp, raw) = scan(path, state.isolated_stamp(&account)).await;
                    isolated.push(agents_view::IsolatedRead {
                        account,
                        stamp,
                        raw,
                    });
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Some(rows) = state.tick(
                    reg_stamp,
                    move || reg_raw,
                    roster_stamp,
                    move || roster_raw,
                    isolated,
                    now,
                ) {
                    // (x-cd67 US4) Resolve the git branch per UNIQUE row cwd,
                    // off the core loop, on the blocking pool - bounded file
                    // reads only, per-cwd degradation on failure (AC1-FR). This
                    // rides the same change-gated emit as the row set, so the
                    // reads happen only when the set moved, never on idle ticks.
                    let cwds: Vec<String> = {
                        let mut seen = std::collections::HashSet::new();
                        rows.iter()
                            .map(|r| r.cwd.clone())
                            .filter(|c| !c.is_empty() && seen.insert(c.clone()))
                            .collect()
                    };
                    let branches = tokio::task::spawn_blocking(move || {
                        cwds.into_iter()
                            .filter_map(|c| {
                                agents_view::resolve_branch(std::path::Path::new(&c))
                                    .map(|b| (c, b))
                            })
                            .collect::<HashMap<String, String>>()
                    })
                    .await
                    .unwrap_or_default();
                    if core_tx
                        .send(CoreMsg::AgentRows { rows, branches })
                        .await
                        .is_err()
                    {
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
        let mut count_rx = client_count_rx.clone();
        tokio::spawn(async move {
            let mut state = backlog_view::ReaderState::default();
            // The last-good claim sweep (x-54fa): `None` until the first
            // success (render un-overlaid), then only ever replaced by a
            // fresher success — a sweep failure keeps this tick's overlay.
            let mut last_live: Option<HashMap<String, String>> = None;
            let mut sweep_failing = false;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // Gate the per-tick claim-sweep SUBPROCESS + graph.json read on
                // an attached client (x-4e30). This is the idle-CPU root fix:
                // an orphaned server with no viewer stops fork/exec'ing a whole
                // `fno-agents claim sweep` process every second. The
                // `changed()` arm is the 0->1 kick so the first attach's
                // overlay is not up to 1s stale (AC3-FR).
                tokio::select! {
                    _ = tick.tick() => {}
                    res = count_rx.changed() => {
                        if res.is_err() {
                            return; // Core dropped; server shutting down
                        }
                    }
                }
                if *count_rx.borrow() == 0 {
                    continue; // no viewer -> no sweep subprocess, no read
                }
                // Claims change without graph.json mtime changes (release, TTL
                // expiry, new dispatch), so the sweep runs every tick, bounded
                // + fail-open. Failure is logged once per state change, not
                // per tick (a permanently-missing fno-agents is one line).
                match run_claim_sweep().await {
                    Some(live) => {
                        if sweep_failing {
                            eprintln!("fno mux: claim sweep recovered");
                        }
                        sweep_failing = false;
                        last_live = Some(live);
                    }
                    None => {
                        if !sweep_failing {
                            eprintln!("fno mux: claim sweep failed; keeping last-good overlay");
                            sweep_failing = true;
                        }
                    }
                }
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
                if let Some((cards, prs)) = state.tick(stamp, move || raw, last_live.as_ref()) {
                    let holders = last_live.clone().unwrap_or_default();
                    if core_tx
                        .send(CoreMsg::BacklogCards {
                            cards,
                            holders,
                            prs,
                        })
                        .await
                        .is_err()
                    {
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
                    // Peer pid names WHICH client process this is (the e2e
                    // harness logs its children's pids for the join).
                    e2e_log(format_args!(
                        "conn {id} accepted (peer pid {:?})",
                        stream.peer_cred().ok().and_then(|c| c.pid())
                    ));
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

    // FNO_E2E idle-exit reaper (x-4e30, Fix 2): the ONLY reaper that survives
    // all four leak paths — panic=abort, SIGKILL of the test binary, a
    // cargo-test timeout, and the untracked client-autospawned setsid server —
    // because it consults neither the parent (ppid==1 by design in prod) nor a
    // Drop guard (never runs on SIGKILL/abort). Armed always, runtime no-op
    // without the marker: a production mux MUST persist across client detach
    // (Locked Decision 2, AC2-EDGE). The deadline re-arms on activity — a
    // client-count change (covers the 0->1 attach edge) OR any pane output —
    // so a working client-less script session (`pane run`, script_api_e2e with
    // `sleep 30` panes) survives; only a truly silent, viewer-less orphan
    // reaches the grace and reaps.
    let idle_exit_e2e = std::env::var_os("FNO_E2E").is_some();
    let idle_grace = Duration::from_millis(
        std::env::var("FNO_IDLE_EXIT_GRACE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_000),
    );
    let mut idle_count_rx = client_count_rx.clone();
    let mut idle_deadline = tokio::time::Instant::now() + idle_grace;

    // x-0296 diagnostics: which panes' output the CORE LOOP has seen. Pairs
    // with the pty reader thread's own first-chunk line to split "shell never
    // spoke" from "core loop never drained it".
    let mut e2e_first_out: HashSet<u64> = HashSet::new();

    let flow = loop {
        tokio::select! {
            chunk = out_rx.recv() => {
                // Pane output is a liveness signal (x-4e30): re-arm the idle
                // reaper so a working client-less script session never reaps.
                // Gated on the marker so a high-throughput prod pane pays no
                // Instant::now() per chunk (gemini review).
                if idle_exit_e2e {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
                // out_tx lives in Core, so recv never yields None.
                let Some((pid, bytes)) = chunk else { break Flow::Shutdown };
                if e2e_first_out.insert(pid) {
                    e2e_log(format_args!(
                        "core loop: first output from pane {pid} ({} bytes)",
                        bytes.len()
                    ));
                }
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
                e2e_log(format_args!("pane {pid} child exited"));
                // A worker that died on its own (churn) tombstones its member
                // BEFORE the reap clears the mapping (AC4-EDGE).
                let ctx = core.member_ctx(pid);
                core.reconcile_member_close(ctx, true);
                if core.close_pane(pid) == Flow::Shutdown {
                    e2e_log(format_args!("last pane gone; shutting down"));
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
                } else if let CoreMsg::Mouse { id, pane, event } = msg {
                    // Wheel-scroll coalescing (mirrors the resize-storm coalescer
                    // above): fold a contiguous run of interpreted wheel ticks on
                    // one pane into ONE broadcast. Each tick is applied IN ORDER so
                    // vt.scroll's per-tick clamp is preserved (algebraic netting
                    // would cancel a clamped tick and lose a reversal at a
                    // boundary); only the intermediate frames are skipped, so a
                    // reversal queued behind in-flight opposite ticks lands in one
                    // frame instead of rubber-banding through every offset. A
                    // non-scroll event (passthrough/select) or passive sender stops
                    // the fold, so ordering and read-only gating stay unchanged.
                    if core.is_passive(id) {
                        if core.handle(CoreMsg::Mouse { id, pane, event }) == Flow::Shutdown {
                            break Flow::Shutdown;
                        }
                    } else if let Some(d0) = core.scroll_delta(pane, &event) {
                        let before = core.scroll_offset(pane) as i32;
                        core.scroll_tick(pane, d0);
                        let mut trailer = None;
                        while let Ok(m) = core_rx.try_recv() {
                            if let CoreMsg::Mouse { id: mid, pane: mpane, event: mev } = &m {
                                if *mpane == pane && !core.is_passive(*mid) {
                                    if let Some(d) = core.scroll_delta(pane, mev) {
                                        core.scroll_tick(pane, d);
                                        continue;
                                    }
                                }
                            }
                            trailer = Some(m);
                            break;
                        }
                        // Cap the fold's net move to one viewport: a fast trackpad
                        // flick drops many ticks in a single drain and would
                        // otherwise jump hundreds of lines at once ("too fast").
                        // The in-order clamp above is intact; this only bounds the
                        // aggregate, so a lone wheel notch (well under a screen)
                        // passes through untouched.
                        let after = core.scroll_offset(pane) as i32;
                        let cap = (core.pane_rows(pane) as i32).max(MOUSE_WHEEL_LINES);
                        let bounded = bounded_scroll_target(before, after, cap);
                        if bounded != after {
                            core.scroll_tick(pane, bounded - after);
                        }
                        if bounded != before {
                            core.broadcast_pane(pane);
                        }
                        if let Some(m) = trailer {
                            if core.handle(m) == Flow::Shutdown {
                                break Flow::Shutdown;
                            }
                        }
                    } else if core.handle(CoreMsg::Mouse { id, pane, event }) == Flow::Shutdown {
                        break Flow::Shutdown;
                    }
                } else if core.handle(msg) == Flow::Shutdown {
                    break Flow::Shutdown;
                }
            }
            // A client-count change is activity (covers the 0->1 attach edge):
            // re-arm the grace window. Disabled in prod (the reaper arm below
            // is off without the marker), so no watch-channel wakeups there
            // (gemini review).
            res = idle_count_rx.changed(), if idle_exit_e2e => {
                if res.is_ok() {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
            }
            // The reaper (x-4e30): enabled only under FNO_E2E. On grace with no
            // activity, reap iff no client is attached — a still-attached
            // session is in use, so re-arm and keep serving. Replicate the
            // CoreMsg::Kill teardown (kill every pane PTY + Flow::Shutdown so
            // SocketGuard unlinks); NEVER std::process::exit, which would
            // orphan the pane shells and leak the socket file.
            _ = tokio::time::sleep_until(idle_deadline), if idle_exit_e2e => {
                if *idle_count_rx.borrow() == 0 {
                    eprintln!("fno mux: idle-exit (FNO_E2E): no client for grace window");
                    for entry in core.panes.values() {
                        entry.pty.kill();
                    }
                    break Flow::Shutdown;
                }
                idle_deadline = tokio::time::Instant::now() + idle_grace;
            }
            _ = async { sigterm.as_mut().unwrap().recv().await }, if sigterm.is_some() => break Flow::Shutdown,
            _ = async { sigint.as_mut().unwrap().recv().await }, if sigint.is_some() => break Flow::Shutdown,
        }
        // Loop-tail choke point (x-4e30): the out_rx/exit_rx arms mutate
        // `clients` via the dead-client sweeps (broadcast_pane /
        // sync_focused_modes / close_pane) without a `handle()` call, so the
        // handle-tail publish alone would leave the count stale on exactly
        // the orphan path the readers gate on.
        core.publish_client_count();
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

/// Read the agents registry FRESH for a guarded `PaneSend`, off the core loop.
/// The server's `self.agents` overlay is parked whenever no client is attached
/// (the reader `continue`s on a zero client count), so a headless one-shot
/// block-pipe must not trust it. Reads the server's OWN registry path, closing
/// the client/server HOME-divergence gap the client-side guard had. `Some(rows)`
/// is the idle authority (empty => no agents => proceed); `None` means the
/// registry is unreadable or malformed and the caller fails closed. A missing
/// file means no daemon and no agents, which proceeds.
async fn read_guard_agents() -> Option<Vec<RegistryAgent>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // std::fs on a blocking pool (this crate's tokio has no `fs` feature); the
    // same shape the overlay reader uses.
    let read =
        tokio::task::spawn_blocking(|| std::fs::read_to_string(agents_view::registry_path())).await;
    match read {
        Ok(Ok(raw)) => agents_view::derive_rows(&raw, now),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Ok(Err(_)) => None, // unreadable -> fail closed
        Err(_) => None,     // blocking task join error -> fail closed
    }
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
            placement,
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
                    placement,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneSend {
            pane,
            bytes,
            guarded,
        } => {
            // A guarded send reads the agents registry FRESH here, off the core
            // loop: the server's own overlay cache (`self.agents`) is parked
            // whenever no client is attached, so a headless one-shot block-pipe
            // would otherwise guard against a stale/empty snapshot and inject
            // into a busy agent. Reading on the server (its own registry path)
            // is what closes the client/server HOME-divergence gap; passing the
            // snapshot into the core loop keeps the check + inject atomic.
            let agents = if guarded {
                read_guard_agents().await
            } else {
                None
            };
            core_tx
                .send(CoreMsg::PaneSend {
                    pane,
                    bytes,
                    guarded,
                    agents,
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
            e2e_log(format_args!("conn {id} attach read ({rows}x{cols})"));
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
    e2e_log(format_args!("conn {id} squad key resolved"));

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
            Ok(ClientMsg::DispatchNext { account }) => {
                if core_tx
                    .send(CoreMsg::DispatchNext { id, account })
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
    fn account_from_argv_reads_the_fno_account_token() {
        // x-c914: the birth account rides the same env(1) wrapper as FNO_NODE.
        let from =
            |a: &[&str]| account_from_argv(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            from(&["env", "FNO_NODE=x-1", "FNO_ACCOUNT=readyrule", "claude"]),
            Some("readyrule".to_string())
        );
        // Default account (no token) / ad-hoc pane / empty value -> None.
        assert_eq!(from(&["env", "FNO_NODE=x-1", "claude"]), None);
        assert_eq!(from(&["claude"]), None);
        assert_eq!(from(&["env", "FNO_ACCOUNT=", "claude"]), None);
    }

    #[test]
    fn attach_argv_routes_isolated_account_to_its_daemon(/* codex P1 */) {
        set_attach_program(&["claude", "attach"]); // pin the base (no leak)
                                                   // Default account: no env wrapper (byte-identical to the bare attach).
        assert_eq!(
            attach_argv("job1", None, None),
            vec![
                "claude".to_string(),
                "attach".to_string(),
                "job1".to_string()
            ]
        );
        // Isolated account: wrapped so `claude attach` hits THAT daemon, with the
        // birth account stamped for the re-attached pane's glyph.
        let dir = std::path::Path::new("/home/u/.claude-alt");
        assert_eq!(
            attach_argv("job1", Some("readyrule"), Some(dir)),
            vec![
                "env".to_string(),
                "CLAUDE_CONFIG_DIR=/home/u/.claude-alt".to_string(),
                "FNO_ACCOUNT=readyrule".to_string(),
                "claude".to_string(),
                "attach".to_string(),
                "job1".to_string(),
            ]
        );
    }

    #[test]
    fn env_provenance_survives_the_account_scrub_prefix() {
        // The REAL `_mesh_env_wrapper` output for an --account spawn: the auth-var
        // scrub (`-u VAR` pairs) leads the assignments. Both FNO_ACCOUNT AND
        // FNO_NODE must still parse past it (codex P1: a naive scan stopped on
        // `-u` and dropped both, so the badge never showed and node provenance
        // was lost for every routed spawn).
        let argv: Vec<String> = [
            "env",
            "-u",
            "ANTHROPIC_API_KEY",
            "-u",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "FNO_AGENT_SELF=w",
            "FNO_NODE=x-1",
            "FNO_ACCOUNT=readyrule",
            "CLAUDE_CONFIG_DIR=/home/u/.claude-alt",
            "claude",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(node_from_argv(&argv), Some("x-1".to_string()));
        assert_eq!(account_from_argv(&argv), Some("readyrule".to_string()));
        assert_eq!(cmd_from_argv(&argv).as_deref(), Some("claude"));
    }

    #[test]
    fn pane_label_prefers_cmd_then_node_then_cwd_then_shell() {
        // The navigator's pane label (v22, x-653d): cmd is the intra-tab
        // discriminator, then node, then the cwd basename, else "shell".
        assert_eq!(
            pane_label(Some("x-abcd"), "/home/u/proj", Some("claude")),
            "claude"
        );
        assert_eq!(pane_label(Some("x-abcd"), "/home/u/proj", None), "x-abcd");
        assert_eq!(pane_label(None, "/home/u/proj", None), "proj");
        assert_eq!(pane_label(None, "", None), "shell");
        // A control-only candidate sanitizes to empty and falls through.
        assert_eq!(pane_label(None, "/home/u/proj", Some("\u{7}")), "proj");
    }

    #[test]
    fn cmd_from_argv_takes_the_command_basename_past_the_env_wrapper() {
        let cmd = |a: &[&str]| cmd_from_argv(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            cmd(&["env", "FNO_NODE=x-1", "claude", "--bg"]),
            Some("claude".into())
        );
        assert_eq!(cmd(&["/usr/bin/htop"]), Some("htop".into()));
        // An env run with no command yields None; spawn never fails on labeling.
        assert_eq!(cmd(&["env", "A=1"]), None);
        assert_eq!(cmd(&[]), None);
    }

    #[test]
    fn tab_label_resolves_the_locked_derivation_chain() {
        // Explicit rename wins outright.
        assert_eq!(
            tab_label(
                Some("debug"),
                Some((Some("x-1"), "/w/x-2", Some("claude"))),
                "/w",
                0
            ),
            "debug"
        );
        // FNO_NODE provenance beats cwd + cmd (AC1-HP).
        assert_eq!(
            tab_label(
                None,
                Some((Some("x-abcd"), "/w/x-2", Some("claude"))),
                "/w",
                0
            ),
            "x-abcd"
        );
        // A spawn cwd whose basename differs from the squad's outranks the
        // cmd label (AC2-EDGE: the worktree-per-node case).
        assert_eq!(
            tab_label(
                None,
                Some((
                    None,
                    "/conductor/workspaces/footnote/x-9f21",
                    Some("claude")
                )),
                "/code/footnote",
                1
            ),
            "x-9f21"
        );
        // Same basename would just echo the squad label -> cmd.
        assert_eq!(
            tab_label(
                None,
                Some((None, "/code/footnote", Some("htop"))),
                "/code/footnote",
                1
            ),
            "htop"
        );
        // Every source empty -> the bare 1-based index, exactly today's
        // label (AC1-EDGE, AC2-FR: nothing errors, logs, or bells).
        assert_eq!(
            tab_label(
                None,
                Some((None, "/code/footnote", None)),
                "/code/footnote",
                2
            ),
            "3"
        );
    }

    #[test]
    fn tab_label_stale_focused_pane_falls_back_to_index() {
        // AC3-FR: tab.focus names a reaped pane (mid-reap race) - the chain
        // skips provenance/cwd/cmd and terminates at the index, no panic.
        assert_eq!(tab_label(None, None, "/w", 0), "1");
    }

    #[test]
    fn tab_label_sanitizes_derived_candidates_and_skips_empty_ones() {
        // codex peer review: derived sources (FNO_NODE, dir names, argv) admit
        // control bytes and land in chrome cells - sanitize like a rename.
        assert_eq!(
            tab_label(None, Some((Some("\x1b[31mx-1"), "/w", None)), "/w", 0),
            "[31mx-1"
        );
        // A whitespace-only node sanitizes to empty and falls through to the
        // next source instead of rendering a blank label.
        assert_eq!(
            tab_label(None, Some((Some("   "), "/w/x-2", None)), "/w", 0),
            "x-2"
        );
        // A control-char-only dir basename falls through to cmd.
        assert_eq!(
            tab_label(None, Some((None, "/w/\x01\x02", Some("htop"))), "/w", 0),
            "htop"
        );
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
            attach_id: None,
            external: false,
            account: None,
            claude_session_uuid: None,
            updated_at: None,
        }
    }

    fn agent(pane: u64, badge: Option<AgentBadge>, exited: bool) -> RegistryAgent {
        agent_in("main", pane, badge, exited)
    }

    #[test]
    fn watch_only_bg_row_surfaces_while_foreign_pane_is_skipped() {
        // An `fno agents spawn --substrate bg` worker writes a paneless
        // (`mux: None`) registry row. It MUST surface as a watch-only AgentRow,
        // even alongside a pane row hosted by another mux session. The
        // session-id skip in `agent_rows()` only eats ANOTHER session's live
        // pane; it must never drop a paneless bg/headless row. Guards a future
        // membership-first rewrite of `agent_rows()` from re-dropping bg rows.
        let mut core = empty_core();
        core.session_name = "main".into();
        core.agents = vec![
            // A pane hosted by ANOTHER session -> that session's server renders
            // it; correctly skipped here.
            RegistryAgent {
                name: "foreign-pane".into(),
                cwd: "/other".into(),
                exited: false,
                badge: None,
                reason: None,
                mux: Some(("other".into(), 5)),
                answerable: None,
                attach_id: None,
                external: false,
                account: None,
                claude_session_uuid: None,
                updated_at: None,
            },
            // A bg worker: paneless, no squad match -> watch-only orphan, and
            // it carries a claude jobId so the sideline can attach it.
            RegistryAgent {
                name: "bg-worker".into(),
                cwd: "/bg".into(),
                exited: false,
                badge: None,
                reason: None,
                mux: None,
                answerable: None,
                attach_id: Some("c19cd2c3".into()),
                external: false,
                account: None,
                claude_session_uuid: None,
                updated_at: None,
            },
        ];
        let rows = core.agent_rows();
        assert!(
            !rows.iter().any(|r| r.name == "foreign-pane"),
            "a pane hosted by another session must be skipped"
        );
        let bg = rows
            .iter()
            .find(|r| r.name == "bg-worker")
            .expect("a paneless bg row must surface as a watch-only row");
        assert_eq!(
            bg.squad, None,
            "an unmatched bg row is an orphan (squad None)"
        );
        assert_eq!(bg.pane_id, None, "a watch-only row has no pane");
        assert!(!bg.exited);
        assert_eq!(
            bg.attach_id.as_deref(),
            Some("c19cd2c3"),
            "the claude jobId must carry through so the sideline can attach it"
        );
    }

    #[test]
    fn agent_rows_match_pane_hosted_by_membership_and_watch_only_by_origins() {
        // Change #5. A pane-hosted agent's row renders under the squad its pane
        // lives in (membership), REGARDLESS of the pane's cwd (AC1-HP). A
        // watch-only row falls back to cwd, now against ANY origin exact-or-child
        // (AC2-EDGE), so a multi-origin squad claims a worker under origins[1].
        let mut core = empty_core();
        core.session_name = "main".into();
        // Squad 1: origin far from the pane's registry cwd ("/w" via agent_in).
        core.session.add_squad(
            1,
            vec!["/origins/one".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(42),
                focus: 42,
            },
        );
        // Squad 2: two origins; a watch-only row's cwd is a child of the SECOND.
        core.session.add_squad(
            2,
            vec!["/grp/frontend".into(), "/grp/backend".into()],
            Some("stack".into()),
            Tab {
                name: None,
                id: 2,
                root: Node::Leaf(50),
                focus: 50,
            },
        );
        core.agents = vec![
            // Pane-hosted in THIS session at pane 42 (which lives in squad 1),
            // but its registry cwd "/w" matches no origin - membership must win.
            agent_in("main", 42, None, false),
            RegistryAgent {
                name: "watcher".into(),
                cwd: "/grp/backend/sub/dir".into(),
                exited: false,
                badge: None,
                reason: None,
                mux: None,
                answerable: None,
                attach_id: None,
                external: false,
                account: None,
                claude_session_uuid: None,
                updated_at: None,
            },
        ];
        let rows = core.agent_rows();
        let hosted = rows.iter().find(|r| r.pane_id == Some(42)).unwrap();
        assert_eq!(
            hosted.squad,
            Some(1),
            "a pane-hosted agent matches by membership even when its cwd matches no origin"
        );
        let watcher = rows.iter().find(|r| r.name == "watcher").unwrap();
        assert_eq!(
            watcher.squad,
            Some(2),
            "a watch-only row matches a squad via a child of origins[1]"
        );
    }

    #[test]
    fn external_synthesized_row_passes_the_attach_catalog_gate() {
        // AC2-HP: a roster-synthesized foreign row (mux None, !exited, attach_id
        // set, external true) is attachable through the EXISTING catalog gate,
        // with no new spawn path. An exited or pane-hosted row is refused, like
        // any non-attachable registry row.
        let mut core = empty_core();
        core.agents = vec![
            RegistryAgent {
                name: "think-x-9999".into(),
                cwd: "/w".into(),
                exited: false,
                badge: None,
                reason: None,
                mux: None,
                answerable: None,
                attach_id: Some("ab12cd34".into()),
                external: true,
                account: None,
                claude_session_uuid: None,
                updated_at: None,
            },
            // An exited external row (dead pane beat the upgrade): not attachable.
            RegistryAgent {
                name: "dead-ext".into(),
                cwd: "/w".into(),
                exited: true,
                badge: None,
                reason: None,
                mux: None,
                answerable: None,
                attach_id: Some("ffffffff".into()),
                external: true,
                account: None,
                claude_session_uuid: None,
                updated_at: None,
            },
        ];
        assert!(
            core.attachable_agent("ab12cd34"),
            "a live foreign row is attachable"
        );
        assert!(
            !core.attachable_agent("ffffffff"),
            "an exited foreign row is refused"
        );
        assert!(
            !core.attachable_agent("deadbeef"),
            "an id naming no surfaced row is refused"
        );
    }

    #[test]
    fn dead_pane_beats_roster_liveness_upgrade() {
        // AC2-EDGE: an upgraded (roster-present, external) registry row whose
        // mux ref points to a dead pane in THIS session renders exited - the
        // pane fact stays senior over the merge's un-exit.
        let mut core = empty_core();
        core.session_name = "main".into();
        core.session.add_squad(
            1,
            vec!["/w".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        // merge_rows would have set exited=false + external=true on this row,
        // but its mux pane (77) is absent from core.panes -> pane_dead.
        core.agents = vec![RegistryAgent {
            name: "upgraded".into(),
            cwd: "/w".into(),
            exited: false,
            badge: None,
            reason: None,
            mux: Some(("main".into(), 77)),
            answerable: None,
            attach_id: Some("ab12cd34".into()),
            external: true,
            account: None,
            claude_session_uuid: None,
            updated_at: None,
        }];
        let rows = core.agent_rows();
        let row = rows.iter().find(|r| r.name == "upgraded").unwrap();
        assert!(row.exited, "a dead pane forces exited despite the upgrade");
        assert!(row.external, "provenance still rides through");
        assert_eq!(row.attach_id, None, "an exited row drops its attach target");
    }

    #[test]
    fn new_squad_rejects_a_blank_name_and_creates_nothing() {
        // Change #2 / AC1-ERR: a whitespace-only name is refused fail-closed -
        // no squad, no pane. PTY-free: the reject returns before any spawn.
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        let flow = core.command(
            1,
            Command::NewSquad {
                name: "   ".into(),
                origin: None,
            },
        );
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(core.session.squads.len(), 1, "blank name creates no squad");
        assert!(core.panes.is_empty(), "blank name spawns no pane");
    }

    #[test]
    fn rename_tab_round_trips_and_blank_clears() {
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        // The rename stores the trimmed name (AC2-HP's server half)...
        core.command(
            1,
            Command::RenameTab {
                tab: 5,
                name: "  debug ".into(),
            },
        );
        assert_eq!(
            core.session.squads[0].tabs[0].name.as_deref(),
            Some("debug")
        );
        // ...and a blank rename CLEARS it back to the derived label (AC3-HP,
        // Locked 2) - a clear, never an error. (Re-register the sender: the
        // test client's dropped receiver made the rename's own layout push
        // reap it, exactly like a real disconnect.)
        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RenameTab {
                tab: 5,
                name: "   ".into(),
            },
        );
        assert_eq!(core.session.squads[0].tabs[0].name, None);
    }

    #[test]
    fn rename_tab_stale_id_is_refused_without_mutation() {
        // AC1-ERR: a RenameTab naming a closed tab mutates nothing.
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: Some("keep".into()),
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        let flow = core.command(
            1,
            Command::RenameTab {
                tab: 999,
                name: "x".into(),
            },
        );
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(
            core.session.squads[0].tabs[0].name.as_deref(),
            Some("keep"),
            "a stale id must not touch any live tab"
        );
    }

    #[test]
    fn rename_tab_sanitizes_hostile_wire_names() {
        // AC2-ERR: control chars are stripped and the stored name is capped -
        // the wire is not the overlay, so the server owns the guarantee.
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RenameTab {
                tab: 5,
                name: format!("\x1b[31m{}", "a".repeat(200)),
            },
        );
        let stored = core.session.squads[0].tabs[0].name.clone().unwrap();
        assert_eq!(stored, format!("[31m{}", "a".repeat(MAX_TAB_NAME - 4)));
    }

    // -- x-96e8 squad management verbs ----------------------------------

    fn leaf_tab(id: TabId, pane: u64) -> Tab {
        Tab {
            name: None,
            id,
            root: Node::Leaf(pane),
            focus: pane,
        }
    }

    // -- x-3e38 pane placement (target resolution + atomic commit) ------

    #[test]
    fn resolve_placement_target_current_route_passes_through() {
        // CurrentRoute yields the caller's default (a cwd/owner squad, or None
        // when a squad must still be born) with no lookup.
        let core = empty_core();
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::CurrentRoute, Some(7))
                .unwrap(),
            Some(7)
        );
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::CurrentRoute, None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn resolve_placement_target_explicit_hit_miss_and_id() {
        // AC2-HP + AC4: an exact name/id resolves; a missing one fails closed.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], Some("review".into()), leaf_tab(5, 1));
        core.session
            .add_squad(2, vec!["/repos/default".into()], None, leaf_tab(6, 2));
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::SquadName(" review ".into()), None)
                .unwrap(),
            Some(1),
            "name is trimmed before match"
        );
        assert!(core
            .resolve_placement_target(&PaneTarget::SquadName("ghost".into()), None)
            .is_err());
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::SquadName("default".into()), None)
                .unwrap(),
            Some(2),
            "derived display names are targetable"
        );
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::SquadId(1), None)
                .unwrap(),
            Some(1)
        );
        assert!(core
            .resolve_placement_target(&PaneTarget::SquadId(99), None)
            .is_err());
    }

    #[test]
    fn place_spawned_pane_new_tab_then_directional_split() {
        // AC1-HP + AC2-HP: omitted split mints a new tab; a direction inserts
        // beside the destination's active-tab focus in that same tab.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));

        let (sid, _tid, _) = core.place_spawned_pane(Some(1), "/a", 2, None).unwrap();
        assert_eq!(sid, 1);
        assert_eq!(
            core.session.squad(1).unwrap().tabs.len(),
            2,
            "omitted split pushes a new tab"
        );

        let tabs_before = core.session.squad(1).unwrap().tabs.len();
        let (_sid, tid, _) = core
            .place_spawned_pane(Some(1), "/a", 3, Some(Dir::Right))
            .unwrap();
        assert_eq!(
            core.session.squad(1).unwrap().tabs.len(),
            tabs_before,
            "a directional split adds no tab"
        );
        let tab = core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|t| t.id == tid)
            .unwrap();
        assert_eq!(
            tree::leaves(&tab.root),
            vec![1, 3],
            "right places the new pane after the focused leaf"
        );
        assert_eq!(tab.focus, 3, "the new pane takes focus");
    }

    #[test]
    fn place_spawned_pane_current_route_miss_births_first_tab() {
        // AC6-EDGE: no squad yet + a split request -> the squad is born from the
        // route with the pane as its lone first tab (split collapses).
        let mut core = empty_core();
        let (sid, tid, _) = core
            .place_spawned_pane(None, "/fresh", 9, Some(Dir::Left))
            .unwrap();
        let sq = core.session.squad(sid).unwrap();
        assert_eq!(sq.tabs.len(), 1);
        assert_eq!(sq.tabs[0].id, tid);
        assert_eq!(tree::leaves(&sq.tabs[0].root), vec![9]);
        assert_eq!(sq.origins, vec!["/fresh".to_string()]);
    }

    #[test]
    fn place_spawned_pane_min_size_refusal_falls_back_to_new_tab() {
        // AC3-FR (x-9f75): a split that would violate minimum size no longer reaps - the pane lands as a new
        // tab in the same squad, the crowded tab is untouched, and the caller is signaled to notice.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        // 8 cols cannot hold two MIN_COLS(8)-wide halves -> horizontal refusal.
        core.tab_areas.insert(5, (40, 8));
        let before = core.session.squad(1).unwrap().tabs[0].root.clone();

        let (_sid, tid, fell_back) = core
            .place_spawned_pane(Some(1), "/a", 3, Some(Dir::Right))
            .unwrap();
        assert!(fell_back, "the split refusal signals a fallback");
        assert_eq!(
            core.session.squad(1).unwrap().tabs[0].root,
            before,
            "the crowded tab is untouched"
        );
        let squad = core.session.squad(1).unwrap();
        assert_eq!(squad.tabs.len(), 2, "the pane landed as a new tab");
        assert_eq!(
            squad.tabs.iter().find(|t| t.id == tid).unwrap().root,
            Node::Leaf(3)
        );
    }

    #[test]
    fn rename_squad_blank_clears_origin_squad_and_refuses_origin_less() {
        // AC1-EDGE + AC1-ERR (server half): a blank rename clears an origin-
        // backed squad to its derived label, but an origin-less (NewSquad)
        // squad has no derivable label, so the blank is refused (name kept).
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/x".into()], Some("work".into()), leaf_tab(5, 1));
        core.session
            .add_squad(2, vec![], Some("scratch".into()), leaf_tab(6, 2));

        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RenameSquad {
                squad: 1,
                name: "  oss ".into(),
            },
        );
        assert_eq!(core.session.squads[0].name.as_deref(), Some("oss"));

        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RenameSquad {
                squad: 1,
                name: "   ".into(),
            },
        );
        assert_eq!(
            core.session.squads[0].name, None,
            "blank clears an origin-backed squad to its derived label"
        );

        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RenameSquad {
                squad: 2,
                name: "".into(),
            },
        );
        assert_eq!(
            core.session.squads[1].name.as_deref(),
            Some("scratch"),
            "an origin-less squad refuses a blank (nothing to derive)"
        );
    }

    #[test]
    fn remove_squad_reanchors_then_last_ends_the_session() {
        // AC2-HP / AC2-EDGE (server half): removing a squad drops it and re-
        // anchors active_squad; removing the last squad ends the session.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        core.session
            .add_squad(2, vec!["/b".into()], None, leaf_tab(6, 2));
        core.session.active_squad = Some(1);

        core.clients.push(client(1, 5, (24, 80), false));
        let flow = core.command(1, Command::RemoveSquad(1));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(core.session.squads.len(), 1);
        assert_eq!(core.session.squad(1), None);
        assert_eq!(
            core.session.active_squad,
            Some(2),
            "active re-anchors to a survivor"
        );

        core.clients.push(client(1, 6, (24, 80), false));
        let flow = core.command(1, Command::RemoveSquad(2));
        assert!(
            matches!(flow, Flow::Shutdown),
            "removing the last squad ends the session (Locked Decision 8)"
        );
        assert!(core.session.squads.is_empty());
    }

    #[test]
    fn remove_squad_unknown_id_is_refused_without_mutation() {
        // AC2-ERR: a RemoveSquad naming a dead id touches nothing.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        core.clients.push(client(1, 5, (24, 80), false));
        let flow = core.command(1, Command::RemoveSquad(999));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(core.session.squads.len(), 1, "no squad removed");
    }

    #[test]
    fn move_squad_reorders_and_edge_bump_is_silent_noop() {
        // AC3-HP + Boundaries: reorder clamps to the list, and an at-edge move
        // is a silent no-op (holding a reorder key at the top must not churn).
        let mut core = empty_core();
        for (sid, tid, pid) in [(1u64, 5u64, 1u64), (2, 6, 2), (3, 7, 3)] {
            core.session
                .add_squad(sid, vec![format!("/{sid}")], None, leaf_tab(tid, pid));
        }
        let order = |c: &Core| c.session.squads.iter().map(|s| s.id).collect::<Vec<_>>();

        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::MoveSquad {
                squad: 3,
                delta: -1,
            },
        );
        assert_eq!(order(&core), vec![1, 3, 2], "squad 3 moved up one");

        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::MoveSquad {
                squad: 1,
                delta: -1,
            },
        );
        assert_eq!(
            order(&core),
            vec![1, 3, 2],
            "an at-edge bump changes nothing"
        );
    }

    #[test]
    fn reorder_tab_moves_within_its_squad_and_keeps_the_same_tab_active() {
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        core.session
            .squad_mut(1)
            .unwrap()
            .tabs
            .extend([leaf_tab(6, 2), leaf_tab(7, 3)]);
        core.session.squad_mut(1).unwrap().active_tab = 1;
        let (client, mut rx) = client_with_rx(1);
        core.clients.push(client);

        core.command(
            1,
            Command::ReorderTab {
                squad: 1,
                tab: 6,
                delta: 1,
            },
        );

        let squad = core.session.squad(1).unwrap();
        assert_eq!(
            squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![5, 7, 6]
        );
        assert_eq!(squad.tabs[squad.active_tab].id, 6);
        assert!(rx.try_recv().is_ok(), "a successful reorder pushes Layout");
    }

    #[test]
    fn reorder_tab_at_an_edge_is_a_silent_noop() {
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        core.session.squad_mut(1).unwrap().tabs.push(leaf_tab(6, 2));
        let (client, mut rx) = client_with_rx(1);
        core.clients.push(client);

        core.command(
            1,
            Command::ReorderTab {
                squad: 1,
                tab: 5,
                delta: -1,
            },
        );
        core.command(
            1,
            Command::ReorderTab {
                squad: 1,
                tab: 6,
                delta: 1,
            },
        );

        let squad = core.session.squad(1).unwrap();
        assert_eq!(
            squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert!(
            rx.try_recv().is_err(),
            "edge bumps push neither Layout nor Notice"
        );
    }

    #[test]
    fn reorder_tab_recovers_from_an_invalid_active_index() {
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        let squad = core.session.squad_mut(1).unwrap();
        squad.tabs.push(leaf_tab(6, 2));
        squad.active_tab = usize::MAX;
        core.clients.push(client(1, 5, (24, 80), false));

        core.command(
            1,
            Command::ReorderTab {
                squad: 1,
                tab: 5,
                delta: 1,
            },
        );

        let squad = core.session.squad(1).unwrap();
        assert_eq!(
            squad.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![6, 5]
        );
        assert_eq!(squad.active_tab, 1);
    }

    #[test]
    fn reorder_tab_refuses_a_stale_tab_id() {
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        let (client, mut rx) = client_with_rx(1);
        core.clients.push(client);

        core.command(
            1,
            Command::ReorderTab {
                squad: 1,
                tab: 999,
                delta: 1,
            },
        );

        assert_eq!(core.session.find_tab(5), Some((1, 0)));
        assert_eq!(drain_notice(&mut rx).as_deref(), Some("no such tab"));
    }

    #[test]
    fn reorder_tab_refuses_when_the_tab_moved_to_another_squad() {
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        core.session.squad_mut(1).unwrap().tabs.push(leaf_tab(6, 2));
        core.session
            .add_squad(2, vec!["/b".into()], None, leaf_tab(7, 3));
        core.session.squad_mut(2).unwrap().tabs.push(leaf_tab(8, 4));
        let (client, mut rx) = client_with_rx(1);
        core.clients.push(client);

        core.command(1, Command::MoveTab { tab: 6, squad: 2 });
        while rx.try_recv().is_ok() {}
        core.command(
            1,
            Command::ReorderTab {
                squad: 1,
                tab: 6,
                delta: -1,
            },
        );

        assert_eq!(
            core.session
                .squad(2)
                .unwrap()
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            vec![7, 8, 6],
            "a stale reorder must not mutate the destination squad"
        );
        assert!(drain_notice(&mut rx).unwrap().contains("moved"));
    }

    #[test]
    fn move_tab_follows_the_viewing_client_into_dst() {
        // Invariant (view validity): a viewer of the moved tab follows it into
        // the destination squad - content continuity beats spatial position.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        core.session
            .add_squad(2, vec!["/b".into()], None, leaf_tab(6, 2));
        // A live receiver so push_layout does not reap the client before we
        // read its post-move view.
        let (tx, _rx) = mpsc::channel(8);
        core.clients.push(Client {
            id: 1,
            reliable_tx: tx,
            dirty: Arc::default(),
            notify: Arc::new(Notify::new()),
            synced_modes: Modes::default(),
            view: (1, 5),
            visible: HashSet::new(),
            dims: (24, 80),
            passive: false,
        });

        core.command(1, Command::MoveTab { tab: 5, squad: 2 });
        assert_eq!(
            core.session.find_tab(5),
            Some((2, 1)),
            "tab 5 re-homed into squad 2"
        );
        assert_eq!(
            core.clients[0].view,
            (2, 5),
            "the viewer follows the moved tab into its new squad"
        );
    }

    #[test]
    fn attach_agent_refuses_unknown_or_malformed_jobid() {
        // The jobId lands in `claude attach <id>`'s argv, so an out-of-shape id
        // is refused before any pane spawns (argv defense in depth). A
        // well-formed id that names no surfaced watch-only row is refused too
        // (catalog membership, like the sibling FocusPane/SelectTab commands) -
        // here `empty_core` has no agents, so even valid-shape "deadbeef" fails.
        for bad in [
            "short",
            "toolongxx",
            "ZZZZZZZZ",
            "; rm -rf /",
            "c19cd2c",
            "deadbeef",
        ] {
            let mut core = empty_core();
            let flow = core.command(1, Command::attach_agent(bad));
            assert!(matches!(flow, Flow::Continue));
            assert!(
                core.panes.is_empty(),
                "un-surfaced/malformed jobId {bad:?} must not spawn a pane"
            );
        }
    }

    // -- x-76ea agent-row lifecycle (server-side validation) ------------

    fn client_with_rx(id: u64) -> (Client, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel::<ServerMsg>(8);
        let mut c = client(id, 5, (24, 80), false);
        c.reliable_tx = tx;
        (c, rx)
    }

    fn drain_notice(rx: &mut mpsc::Receiver<ServerMsg>) -> Option<String> {
        let mut out = None;
        while let Ok(ServerMsg::Notice { text }) = rx.try_recv() {
            out = Some(text);
        }
        out
    }

    #[test]
    fn stop_agent_unknown_name_refused() {
        // US5 / AC3-ERR: a StopAgent naming a row absent from the catalog is
        // refused fail-closed with a notice. A plain #[test] has no tokio
        // runtime, so reaching agent_action's spawn would panic - the clean
        // refusal is also proof the happy path is never taken here.
        let mut core = empty_core();
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        let flow = core.command(
            1,
            Command::StopAgent {
                name: "ghost".into(),
            },
        );
        assert!(matches!(flow, Flow::Continue));
        assert!(drain_notice(&mut rx).unwrap().contains("no such agent"));
    }

    #[test]
    fn remove_agent_live_row_refused_stop_first() {
        // US2 (stop-then-rm ordering): RemoveAgent on a still-live registry row
        // is refused with the stop-first reason (mirrors the CLI's own refusal).
        let mut core = empty_core();
        core.agents = vec![bg_row("live-worker", "/tmp", None)]; // exited: false
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::RemoveAgent {
                name: "live-worker".into(),
            },
        );
        assert!(drain_notice(&mut rx).unwrap().contains("still live"));
    }

    /// A helper for the respawn refusal tests: an EXITED registry row with an
    /// optional recorded claude session uuid.
    fn exited_claude_row(name: &str, uuid: Option<&str>) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            cwd: "/w".into(),
            exited: true,
            badge: None,
            reason: None,
            mux: None,
            answerable: None,
            attach_id: None,
            external: false,
            account: None,
            claude_session_uuid: uuid.map(str::to_owned),
            updated_at: None,
        }
    }

    #[test]
    fn respawn_agent_live_row_refused() {
        // AC2-ERR: RespawnAgent on a still-live row is refused (a plain #[test]
        // has no tokio runtime, so the clean refusal is also proof the spawn arm
        // is never reached).
        let mut core = empty_core();
        core.agents = vec![bg_row("live-worker", "/w", None)]; // exited: false
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::RespawnAgent {
                name: "live-worker".into(),
            },
        );
        assert!(drain_notice(&mut rx).unwrap().contains("still live"));
    }

    #[test]
    fn respawn_agent_no_uuid_refused() {
        // AC2-ERR: an exited row with no recorded claude_session_uuid (also the
        // non-claude case, since derive_rows only carries the uuid for claude).
        let mut core = empty_core();
        core.agents = vec![exited_claude_row("dead-worker", None)];
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::RespawnAgent {
                name: "dead-worker".into(),
            },
        );
        assert!(drain_notice(&mut rx)
            .unwrap()
            .contains("no claude session recorded"));
    }

    #[test]
    fn respawn_agent_malformed_uuid_refused_before_argv() {
        // AC2-ERR: a malformed uuid is refused with the SPECIFIC reason (a
        // generic "error" would fail this AC) before it could reach argv.
        let mut core = empty_core();
        core.agents = vec![exited_claude_row("dead-worker", Some("not-a-uuid"))];
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::RespawnAgent {
                name: "dead-worker".into(),
            },
        );
        assert!(drain_notice(&mut rx)
            .unwrap()
            .contains("malformed session id"));
    }

    #[test]
    fn mail_agent_unknown_name_refused() {
        // AC1-ERR: MailAgent naming an absent row is refused fail-closed.
        let mut core = empty_core();
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::MailAgent {
                name: "ghost".into(),
                text: "hi".into(),
            },
        );
        assert!(drain_notice(&mut rx).unwrap().contains("no such agent"));
    }

    #[test]
    fn mail_agent_blank_text_refused_after_resolve() {
        // AC3-ERR: a valid target but blank-after-sanitize text is refused (the
        // resolve succeeds, so the refusal proves the sanitize gate, not the
        // resolver, caught it - and no subprocess is reached in a plain #[test]).
        let mut core = empty_core();
        core.agents = vec![bg_row("worker", "/w", None)];
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::MailAgent {
                name: "worker".into(),
                text: "   ".into(),
            },
        );
        assert!(drain_notice(&mut rx).unwrap().contains("empty"));
    }

    #[test]
    fn sanitize_mail_text_strips_trims_and_bounds() {
        // Control chars stripped, trimmed; blank refused; over-cap refused (never
        // truncated - Locked Decision 7).
        assert_eq!(sanitize_mail_text("  hi \x07there \n").unwrap(), "hi there");
        assert!(sanitize_mail_text("").is_err());
        assert!(sanitize_mail_text("\x07\x08 \t").is_err());
        let ok = "x".repeat(crate::proto::MAX_MAIL_TEXT);
        assert_eq!(
            sanitize_mail_text(&ok).unwrap().len(),
            crate::proto::MAX_MAIL_TEXT
        );
        assert!(sanitize_mail_text(&"x".repeat(crate::proto::MAX_MAIL_TEXT + 1)).is_err());
    }

    #[test]
    fn valid_session_uuid_accepts_only_lowercase_8_4_4_4_12_hex() {
        assert!(valid_session_uuid("12345678-1234-1234-1234-1234567890ab"));
        assert!(!valid_session_uuid("not-a-uuid"));
        assert!(!valid_session_uuid("12345678-1234-1234-1234-1234567890AB")); // uppercase
        assert!(!valid_session_uuid("12345678123412341234567890ab")); // no dashes
        assert!(!valid_session_uuid("12345678-1234-1234-1234-1234567890a")); // short group
    }

    #[test]
    fn agent_rows_join_pr_from_holder_map() {
        // US8: a watch-only worker whose name holds a live claim on a node with a
        // pr_number gets AgentRow.pr; a non-holder row gets None. updated_at
        // passes through from the registry row.
        let mut core = empty_core();
        core.session_name = "main".into();
        let mut worker = bg_row("target-x-9c5f", "/w", None);
        worker.updated_at = Some(42);
        core.agents = vec![worker, bg_row("other-worker", "/x", None)];
        core.backlog_holders = HashMap::from([("x-9c5f".to_string(), "target-x-9c5f".to_string())]);
        core.backlog_pr = HashMap::from([("x-9c5f".to_string(), 385)]);
        let rows = core.agent_rows();
        let joined = rows.iter().find(|r| r.name == "target-x-9c5f").unwrap();
        assert_eq!(joined.pr, Some(385));
        assert_eq!(joined.updated_at, Some(42));
        let other = rows.iter().find(|r| r.name == "other-worker").unwrap();
        assert_eq!(other.pr, None);
    }

    #[test]
    fn external_row_stop_and_remove_refused() {
        // US4: an external roster row belongs to the claude daemon, not the fno
        // registry, so BOTH verbs refuse with a notice rather than fire a doomed
        // `fno-agents` call. The external arm is checked before the live/exited
        // arms, so a dead external row still refuses on provenance.
        let ext_live = RegistryAgent {
            external: true,
            ..bg_row("ext-a", "/tmp", Some("deadbee1"))
        };
        let ext_dead = RegistryAgent {
            external: true,
            exited: true,
            ..bg_row("ext-b", "/tmp", Some("deadbee2"))
        };
        for (row, cmd) in [
            (
                ext_live,
                Command::StopAgent {
                    name: "ext-a".into(),
                },
            ),
            (
                ext_dead,
                Command::RemoveAgent {
                    name: "ext-b".into(),
                },
            ),
        ] {
            let mut core = empty_core();
            core.agents = vec![row];
            let (c, mut rx) = client_with_rx(1);
            core.clients.push(c);
            core.command(1, cmd);
            assert!(drain_notice(&mut rx).unwrap().contains("external"));
        }
    }

    fn ext_record(
        id: &str,
        state: crate::squad_store::ExternalState,
    ) -> crate::squad_store::ExternalLifecycle {
        crate::squad_store::ExternalLifecycle {
            attach_id: id.into(),
            name: format!("ext-{id}"),
            cwd: "/tmp".into(),
            state,
            generation: 1,
            updated_at: String::new(),
            reason: None,
        }
    }

    #[test]
    fn stop_external_stale_id_refused_without_spawn() {
        // AC1-ERR: a StopExternal whose attach id names neither a live external
        // row nor a retry-eligible tombstone is refused fail-closed - no
        // subprocess. A plain #[test] has no tokio runtime, so reaching the
        // spawn would panic; the clean refusal proves it never does.
        let mut core = empty_core();
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(
            1,
            Command::StopExternal {
                attach_id: "deadbeef".into(),
                name: "ext".into(),
            },
        );
        assert!(drain_notice(&mut rx)
            .unwrap()
            .contains("no longer a live external row"));
    }

    #[test]
    fn external_lifecycle_invalid_id_refused_before_spawn() {
        // codex P2: a non-8-hex attach id from the client is rejected before it
        // is persisted or reaches a `claude` argv (a dash-prefixed id could be
        // read as a CLI option). Both verbs guard; the refusal precedes any
        // resolve/CAS, so a #[test] with no tokio runtime never panics.
        for cmd in [
            Command::StopExternal {
                attach_id: "--oops".into(),
                name: "x".into(),
            },
            Command::RemoveExternal {
                attach_id: "nothex!".into(),
                name: "x".into(),
            },
        ] {
            let mut core = empty_core();
            let (c, mut rx) = client_with_rx(1);
            core.clients.push(c);
            core.command(1, cmd);
            assert!(drain_notice(&mut rx)
                .unwrap()
                .contains("invalid external id"));
        }
    }

    #[test]
    fn remove_external_without_stopped_record_refused() {
        // AC2-ERR: rm is reachable only from a persisted `stopped` tombstone. An
        // absent record refuses; a `stopping` record refuses "stop it first".
        // Both stay off the spawn path (no tokio runtime in a #[test]).
        let _s = StoreScratch::new("rm-external-refused");
        let mut core = empty_core();
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        // Absent record.
        core.command(
            1,
            Command::RemoveExternal {
                attach_id: "deadbeef".into(),
                name: "ext".into(),
            },
        );
        assert!(drain_notice(&mut rx)
            .unwrap()
            .contains("no such stopped row"));
        // A stopping record refuses with the stop-first ordering.
        crate::squad_store::begin_external_stop("deadbeef", "ext", "/tmp").unwrap();
        core.command(
            1,
            Command::RemoveExternal {
                attach_id: "deadbeef".into(),
                name: "ext".into(),
            },
        );
        assert!(drain_notice(&mut rx).unwrap().contains("stop it first"));
    }

    #[test]
    fn agent_rows_render_external_tombstones_by_state() {
        // A stopped record renders an EXITED external row carrying its attach_id
        // (so `x` sends RemoveExternal); a failed record renders `!exited` (so
        // `x` retries the stop). Both are external.
        use crate::squad_store::ExternalState as S;
        let mut core = empty_core();
        core.external_lifecycle = vec![
            ext_record("deadbeef", S::Stopped),
            ext_record("cafef00d", S::Failed),
        ];
        let rows = core.agent_rows();
        let stopped = rows
            .iter()
            .find(|r| r.attach_id.as_deref() == Some("deadbeef"))
            .expect("a stopped tombstone row");
        assert!(
            stopped.external && stopped.exited,
            "stopped -> exited external"
        );
        let failed = rows
            .iter()
            .find(|r| r.attach_id.as_deref() == Some("cafef00d"))
            .expect("a failed tombstone row");
        assert!(
            failed.external && !failed.exited,
            "failed -> live-ish external"
        );
    }

    #[test]
    fn agent_rows_dedup_external_tombstone_against_live_row() {
        // A record whose attach_id is ALSO a live external roster row is skipped
        // (the live row wins) so a stop mid-flight never double-renders.
        use crate::squad_store::ExternalState as S;
        let mut core = empty_core();
        core.agents = vec![RegistryAgent {
            external: true,
            ..bg_row("ext-live", "/tmp", Some("deadbeef"))
        }];
        core.external_lifecycle = vec![ext_record("deadbeef", S::Stopping)];
        let n = core
            .agent_rows()
            .iter()
            .filter(|r| r.attach_id.as_deref() == Some("deadbeef"))
            .count();
        assert_eq!(n, 1, "the live row wins; the record row is deduped away");
    }

    #[test]
    fn lifecycle_name_collision_refused_fail_closed() {
        // codex review: `name` is not a unique catalog key (dedup is by
        // attach_id). When an external roster row shares a name with a registry
        // row, the verb must refuse on provenance and NEVER act on the registry
        // agent the external shadows; two same-named registry rows are ambiguous.
        // Both are fail-closed refusals, so no unrelated agent is ever stopped.
        let shared = |external, exited, attach: &str| RegistryAgent {
            external,
            exited,
            ..bg_row("dup", "/tmp", Some(attach))
        };

        // External shadows a registry row -> refuse as external, act on neither.
        let mut core = empty_core();
        core.agents = vec![
            shared(false, false, "reg00001"),
            shared(true, false, "ext00001"),
        ];
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(1, Command::StopAgent { name: "dup".into() });
        assert!(drain_notice(&mut rx).unwrap().contains("external"));

        // Two non-external rows with one name -> ambiguous refusal.
        let mut core = empty_core();
        core.agents = vec![
            shared(false, true, "reg00001"),
            shared(false, true, "reg00002"),
        ];
        let (c, mut rx) = client_with_rx(1);
        core.clients.push(c);
        core.command(1, Command::RemoveAgent { name: "dup".into() });
        assert!(drain_notice(&mut rx).unwrap().contains("ambiguous"));
    }

    #[test]
    fn attach_reconcile_focuses_mapped_pane_no_second_tab() {
        // x-0090 AC2-HP / AC2-FR: an attach_id already mapped to a live pane
        // focuses it, never mints a second tab. The seeded (pane + map entry)
        // stands in for a successful first attach (the real spawn needs a live
        // `claude`), so the live command under test is attach #2 - and a third
        // is still a focus, covering the double-action guard.
        let (mut core, client_id, _p1, p2, _rx) = seen_test_core();
        core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
        core.attached.insert("deadbee1".into(), p2);
        let panes_before = core.panes.len();

        core.command(client_id, Command::attach_agent("deadbee1"));
        assert_eq!(
            core.panes.len(),
            panes_before,
            "reconcile-focus spawns no new pane"
        );
        // The view jumped to the mapped pane's tab (p2 is tab id 2).
        assert_eq!(core.client_view(client_id), Some((1, 2)), "view follows p2");

        core.command(client_id, Command::attach_agent("deadbee1"));
        assert_eq!(
            core.panes.len(),
            panes_before,
            "a second action stays a focus - exactly one pane"
        );
    }

    #[test]
    fn repeated_attach_focuses_existing_pane_with_notice() {
        // x-3e38 AC3-HP: a second attach of a mapped agent focuses the live
        // pane and says so, never minting a second pane. The notice makes the
        // idempotent focus visible to the operator.
        let (mut core, client_id, _p1, p2, mut rx) = seen_test_core();
        core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
        core.attached.insert("deadbee1".into(), p2);
        let panes_before = core.panes.len();

        core.command(client_id, Command::attach_agent("deadbee1"));
        assert_eq!(core.panes.len(), panes_before, "reconcile spawns no pane");
        let mut saw_notice = false;
        while let Ok(msg) = rx.try_recv() {
            if let ServerMsg::Notice { text } = msg {
                saw_notice |= text.contains("already attached");
            }
        }
        assert!(saw_notice, "a repeated attach reports the idempotent focus");
    }

    #[test]
    fn fresh_attach_unknown_target_fails_closed_before_spawn() {
        // x-3e38 AC4: an explicit target that names no live squad refuses BEFORE
        // any PTY spawn - no pane, no attach mapping, a visible reason.
        let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
        core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
        let panes_before = core.panes.len();

        core.command(
            client_id,
            Command::AttachAgent {
                id: "deadbee1".into(),
                placement: PanePlacement {
                    target: PaneTarget::SquadName("ghost".into()),
                    split: None,
                    here: false,
                },
            },
        );
        assert_eq!(core.panes.len(), panes_before, "no PTY spawned");
        assert!(
            !core.attached.contains_key("deadbee1"),
            "no attach mapping recorded on a refused target"
        );
        let mut saw = false;
        while let Ok(msg) = rx.try_recv() {
            if let ServerMsg::Notice { text } = msg {
                saw |= text.contains("no such squad");
            }
        }
        assert!(saw, "the refusal names the missing squad");
    }

    // -- x-9f75 open-here (PanePlacement.here) ---------------------------

    /// Collect every notice text still queued on `rx`.
    fn drain_notices(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ServerMsg::Notice { text } = msg {
                out.push(text);
            }
        }
        out
    }

    #[test]
    fn open_here_swaps_focused_viewer_and_detaches_displaced() {
        // AC1-HP: the focused viewer of session A is repointed at B - the tab's tree slot now hosts B's
        // viewer, focus is the new pane, A's viewer is reaped (A resurfaces watch-only), and B is mapped.
        set_attach_program(&["/bin/cat"]); // stand in for `claude attach`
        let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
        let view = core.client_view(client_id).unwrap();
        let focus = core.viewed_tab(view).unwrap().focus;
        // A occupies the focused pane; B is a watch-only row to open here.
        core.attached.insert("deadbee1".into(), focus);
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let new_pid = core.next_pane_id;

        core.command(client_id, Command::attach_agent_here("deadbee2"));

        let tab = core.viewed_tab(view).unwrap();
        assert_eq!(tab.root, Node::Leaf(new_pid), "slot repointed at B");
        assert_eq!(tab.focus, new_pid, "focus follows the swap");
        assert!(!core.panes.contains_key(&focus), "A's viewer pane reaped");
        assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "B mapped");
        assert!(
            !core.attached.contains_key("deadbee1"),
            "A's mapping swept - it resurfaces watch-only"
        );
        assert!(
            drain_notices(&mut rx)
                .iter()
                .any(|t| t.contains("opened here")),
            "notice names the displaced session"
        );
        core.reap_pane(new_pid); // don't leak the stand-in child
    }

    #[test]
    fn open_here_refuses_non_viewer_focus_before_spawn() {
        // AC1-ERR: the focused pane is a direct/shell pane (not in `attached`), so open-here refuses
        // fail-closed with no spawn - killing its PTY child would kill real work.
        let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let panes_before = core.panes.len();

        core.command(client_id, Command::attach_agent_here("deadbee2"));

        assert_eq!(core.panes.len(), panes_before, "no pane spawned");
        assert!(!core.attached.contains_key("deadbee2"), "nothing mapped");
        assert!(drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("not a detachable viewer")));
    }

    #[test]
    fn open_here_refuses_conflicting_placement_before_spawn() {
        // AC2-ERR: `here` with a split or a non-CurrentRoute target is a
        // contradiction - refused with no spawn.
        for placement in [
            PanePlacement {
                here: true,
                split: Some(Dir::Right),
                ..Default::default()
            },
            PanePlacement {
                here: true,
                target: PaneTarget::SquadName("review".into()),
                ..Default::default()
            },
        ] {
            let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
            core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
            let panes_before = core.panes.len();
            core.command(
                client_id,
                Command::AttachAgent {
                    id: "deadbee2".into(),
                    placement,
                },
            );
            assert_eq!(core.panes.len(), panes_before, "no pane spawned");
            assert!(drain_notices(&mut rx)
                .iter()
                .any(|t| t.contains("open-here takes no split or target")));
        }
    }

    #[test]
    fn open_here_spawn_failure_leaves_layout_untouched() {
        // AC3-ERR: the attach spawn fails - no pane is displaced, `attached` is
        // unchanged, and the sender gets `attach failed`.
        set_attach_program(&["/nonexistent/definitely-not-a-real-binary-xyz"]);
        let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
        let view = core.client_view(client_id).unwrap();
        let focus = core.viewed_tab(view).unwrap().focus;
        core.attached.insert("deadbee1".into(), focus);
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let root_before = core.viewed_tab(view).unwrap().root.clone();

        core.command(client_id, Command::attach_agent_here("deadbee2"));

        assert_eq!(
            core.viewed_tab(view).unwrap().root,
            root_before,
            "nothing displaced on spawn failure"
        );
        assert!(core.panes.contains_key(&focus), "A's viewer still live");
        assert_eq!(
            core.attached.get("deadbee1"),
            Some(&focus),
            "A's mapping untouched"
        );
        assert!(!core.attached.contains_key("deadbee2"), "B never mapped");
        assert!(drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("attach failed")));
    }

    #[test]
    fn open_here_reconcile_focuses_existing_no_displacement() {
        // AC1-EDGE: B already has a live pane. Reconcile focuses it (no spawn,
        // no displacement of the current focus) - reconcile beats open-here.
        let (mut core, client_id, p1, p2, mut rx) = seen_test_core();
        let view = core.client_view(client_id).unwrap();
        let focus = core.viewed_tab(view).unwrap().focus;
        // The focused pane is A's viewer; B is already paned at the OTHER pane.
        let other = if focus == p1 { p2 } else { p1 };
        core.attached.insert("deadbee1".into(), focus);
        core.attached.insert("deadbee2".into(), other);
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let panes_before = core.panes.len();

        core.command(client_id, Command::attach_agent_here("deadbee2"));

        assert_eq!(core.panes.len(), panes_before, "reconcile spawns nothing");
        assert!(core.panes.contains_key(&focus), "A's viewer not displaced");
        assert_eq!(
            core.attached.get("deadbee2"),
            Some(&other),
            "B still at its pane"
        );
        assert!(drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("already attached")));
    }

    #[test]
    fn open_here_double_click_focuses_no_second_displacement() {
        // AC1-FR: open-here on B twice quickly - the first swaps, the second hits reconcile (B now paned)
        // and focuses it. Exactly one viewer of B, no second displacement.
        set_attach_program(&["/bin/cat"]);
        let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
        let view = core.client_view(client_id).unwrap();
        let focus = core.viewed_tab(view).unwrap().focus;
        core.attached.insert("deadbee1".into(), focus);
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let new_pid = core.next_pane_id;

        core.command(client_id, Command::attach_agent_here("deadbee2"));
        let panes_after_first = core.panes.len();
        core.command(client_id, Command::attach_agent_here("deadbee2"));

        assert_eq!(
            core.panes.len(),
            panes_after_first,
            "the second open-here mints no pane"
        );
        assert_eq!(
            core.attached.values().filter(|&&p| p == new_pid).count(),
            1,
            "exactly one viewer of B"
        );
        assert_eq!(core.attached.get("deadbee2"), Some(&new_pid));
        core.reap_pane(new_pid);
    }

    #[test]
    fn open_here_guard_reads_current_focus_not_client_identity() {
        // AC2-FR: the displacement guard evaluates whatever pane is focused NOW (re-resolved server-side), so
        // if focus moved to a non-viewer (e.g. the original viewer exited under the click) open-here refuses
        // rather than blindly displacing an unintended pane. Here the focus is a non-viewer, a viewer elsewhere.
        let (mut core, client_id, p1, p2, mut rx) = seen_test_core();
        let view = core.client_view(client_id).unwrap();
        let focus = core.viewed_tab(view).unwrap().focus;
        // A viewer exists, but on the OTHER (unfocused) pane; focus is a plain
        // pane not in `attached`.
        let other = if focus == p1 { p2 } else { p1 };
        core.attached.insert("deadbee1".into(), other);
        core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
        let panes_before = core.panes.len();

        core.command(client_id, Command::attach_agent_here("deadbee2"));

        assert_eq!(core.panes.len(), panes_before, "no pane spawned");
        assert!(
            core.panes.contains_key(&other),
            "the elsewhere viewer is never touched"
        );
        assert!(drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("not a detachable viewer")));
    }

    #[test]
    fn attach_split_fallback_lands_new_tab_with_notice() {
        // AC3-FR (x-9f75): a same-workspace row click sends AttachAgent with a Right split; at min-size the
        // split is refused and the pane lands as a NEW TAB with a `tab full` notice - never a reap+dead-end.
        set_attach_program(&["/bin/cat"]);
        let (mut core, client_id, _p1, _p2, mut rx) = seen_test_core();
        let view = core.client_view(client_id).unwrap();
        // Force a horizontal-split refusal on the split target tab: the viewing client's dims drive its area
        // (tab_area prefers the client clamp), so shrink them below a two-pane horizontal minimum.
        for c in core.clients.iter_mut().filter(|c| c.id == client_id) {
            c.dims = (24, 12);
        }
        // Align the split target (squad.active_tab) with the tab the client
        // views, so the shrunk-client clamp governs that tab's area.
        let sq = core.session.squad_mut(view.0).unwrap();
        sq.active_tab = sq.tabs.iter().position(|t| t.id == view.1).unwrap();
        core.agents = vec![bg_row("sib", "/tmp/seen", Some("deadbee2"))];
        let squad_tabs_before = core.session.squad(view.0).unwrap().tabs.len();
        let new_pid = core.next_pane_id;

        core.command(
            client_id,
            Command::AttachAgent {
                id: "deadbee2".into(),
                placement: PanePlacement {
                    target: PaneTarget::CurrentRoute,
                    split: Some(Dir::Right),
                    here: false,
                },
            },
        );

        assert_eq!(
            core.session.squad(view.0).unwrap().tabs.len(),
            squad_tabs_before + 1,
            "the pane landed as a new tab"
        );
        assert_eq!(core.attached.get("deadbee2"), Some(&new_pid), "B mapped");
        assert!(drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("tab full - opened as tab")));
        core.reap_pane(new_pid);
    }

    #[test]
    fn agent_rows_presents_mapped_watch_only_pane_hosted() {
        // x-0090 AC1-HP (presentation): a watch-only row whose attach maps to a
        // live pane renders pane-hosted under the pane's squad, with attach_id
        // dropped - so agent_hit sends FocusPane, not a duplicate AttachAgent.
        let (mut core, _client_id, _p1, p2, _rx) = seen_test_core();
        core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
        core.attached.insert("deadbee1".into(), p2);

        let rows = core.agent_rows();
        let row = rows.iter().find(|r| r.name == "spawn-fix-c3d4").unwrap();
        assert_eq!(row.pane_id, Some(p2), "presents pane-hosted");
        assert_eq!(row.squad, Some(1), "under the pane's squad");
        assert_eq!(row.attach_id, None, "attach target dropped on a pane row");
    }

    #[test]
    fn reap_sweeps_attach_map_and_row_reverts_to_watch_only() {
        // x-0090 AC1-FR: killing the mapped pane sweeps the map (eager) AND the
        // lazy `panes` liveness check reverts the row to watch-only attachable,
        // so a next click re-attaches into a fresh pane rather than a corpse.
        let (mut core, _client_id, _p1, p2, _rx) = seen_test_core();
        core.agents = vec![bg_row("spawn-fix-c3d4", "/tmp/seen", Some("deadbee1"))];
        core.attached.insert("deadbee1".into(), p2);

        core.reap_pane(p2);
        assert!(
            !core.attached.values().any(|&p| p == p2),
            "eager sweep drops the dead pane's mapping"
        );
        let rows = core.agent_rows();
        let row = rows.iter().find(|r| r.name == "spawn-fix-c3d4").unwrap();
        assert_eq!(row.pane_id, None, "reverts to watch-only");
        assert_eq!(
            row.attach_id.as_deref(),
            Some("deadbee1"),
            "re-attachable after the pane dies"
        );
    }

    #[test]
    fn agent_rows_union_covers_merged_bare_and_watch_only() {
        // x-0090 US2: agent_rows() is a pane union. seen_test_core gives squad 1
        // with p1 in tab 1 and p2 in tab 2 (both bare `/bin/cat` panes). Enrich
        // p1 from the registry, leave p2 bare, add one paneless watch-only row.
        let (mut core, _c, p1, p2, _rx) = seen_test_core();
        core.agents = vec![
            agent_in("test", p1, Some(AgentBadge::Working), false),
            bg_row("spawn-fix-c3d4", "/elsewhere", Some("deadbee1")),
        ];
        let rows = core.agent_rows();
        // Pane rows first in (tab, pane) order, watch-only appended last.
        assert_eq!(rows.len(), 3, "two pane rows + one watch-only");

        // Merged row: named + badged from the registry, carries its tab ref.
        assert_eq!(rows[0].pane_id, Some(p1));
        assert_eq!(rows[0].name, "w", "registry name wins on a merged row");
        assert_eq!(rows[0].badge, Some(AgentBadge::Working));
        assert_eq!(rows[0].tab, Some(1));
        assert_eq!(rows[0].attach_id, None, "a pane row never carries attach");

        // Bare pane: labelled from its own entry (cwd basename here), no badge.
        assert_eq!(rows[1].pane_id, Some(p2));
        assert_eq!(rows[1].name, "seen", "bare pane labelled from PaneEntry");
        assert_eq!(rows[1].badge, None);
        assert_eq!(rows[1].tab, Some(2));

        // Watch-only appended last: paneless, orphan squad, still attachable.
        assert_eq!(rows[2].pane_id, None, "watch-only has no pane");
        assert_eq!(rows[2].name, "spawn-fix-c3d4");
        assert_eq!(rows[2].squad, None, "cwd matches no squad -> orphan");
        assert_eq!(rows[2].attach_id.as_deref(), Some("deadbee1"));
        assert_eq!(rows[2].tab, None);
    }

    #[test]
    fn agent_rows_one_row_per_entity_no_watch_only_double() {
        // x-0090 Invariant: a registry row merged onto a pane never ALSO renders
        // watch-only. A bg row that IS pane-hosted this session appears once.
        let (mut core, _c, p1, _p2, _rx) = seen_test_core();
        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
        let rows = core.agent_rows();
        let hits = rows.iter().filter(|r| r.name == "w").count();
        assert_eq!(hits, 1, "the merged agent renders exactly once");
    }

    #[test]
    fn card_ready_gate_only_passes_ready_cards() {
        // x-a496 (codex peer review): a targeted dispatch only proceeds for a
        // READY card named by id or slug; blocked / in-flight / unknown ids are
        // refused, so a click can't start work leader+g would skip.
        let card = |id: &str, slug: &str, state| BacklogCard {
            id: id.into(),
            slug: slug.into(),
            priority: "p2".into(),
            state,
            pane_id: None,
            attach_id: None,
            where_hint: None,
        };
        let backlog = [
            card("x-rdy", "ready-slug", CardState::Ready),
            card("x-blk", "blk-slug", CardState::Blocked),
            card("x-fly", "fly-slug", CardState::InFlight),
        ];
        assert!(card_ready_to_dispatch(&backlog, "x-rdy"), "ready by id");
        assert!(
            card_ready_to_dispatch(&backlog, "ready-slug"),
            "ready by slug"
        );
        assert!(
            !card_ready_to_dispatch(&backlog, "x-blk"),
            "blocked refused"
        );
        assert!(
            !card_ready_to_dispatch(&backlog, "x-fly"),
            "in-flight refused"
        );
        assert!(
            !card_ready_to_dispatch(&backlog, "x-nope"),
            "unknown refused"
        );
        assert!(!card_ready_to_dispatch(&backlog, ""), "empty refused");
        assert!(
            !card_ready_to_dispatch(&[], "x-rdy"),
            "empty backlog refused"
        );
    }

    #[test]
    fn node_token_matches_whole_ids_only() {
        // Locked 6: exact node-id token, non-alphanumeric boundaries. `-` is
        // part of the id shape, so it cannot be the boundary test.
        assert!(name_has_node_token("x-54fa", "x-54fa"));
        assert!(name_has_node_token("tgt-x-54fa", "x-54fa"));
        assert!(name_has_node_token("run x-54fa now", "x-54fa"));
        assert!(name_has_node_token("x-54fa.retry", "x-54fa"));
        // Prefix/suffix of a longer token never matches.
        assert!(!name_has_node_token("x-54fab", "x-54fa"));
        assert!(!name_has_node_token("ax-54fa", "x-54fa"));
        assert!(!name_has_node_token("x-54fa", "x-54f"));
        // Second occurrence with clean boundaries still matches.
        assert!(name_has_node_token("x-54fab x-54fa", "x-54fa"));
        assert!(!name_has_node_token("anything", ""));
        // A node whose FIRST char is multi-byte (a non-ASCII `id_prefix` is
        // legal config) must not panic when a rejected match forces the scan
        // to advance - `start + 1` would land inside the char (gemini review
        // of PR #211). Both the advance-then-match and the pure-reject walk.
        assert!(name_has_node_token(
            "a\u{3093}-54fa \u{3093}-54fa",
            "\u{3093}-54fa"
        ));
        assert!(!name_has_node_token("a\u{3093}-54fa", "\u{3093}-54fa"));
    }

    /// A paneless registry row for the routing tests: `name`/`cwd`/`attach_id`
    /// are the join surfaces; everything else is the quiet default.
    fn bg_row(name: &str, cwd: &str, attach: Option<&str>) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            cwd: cwd.into(),
            exited: false,
            badge: None,
            reason: None,
            mux: None,
            answerable: None,
            attach_id: attach.map(str::to_owned),
            external: false,
            account: None,
            claude_session_uuid: None,
            updated_at: None,
        }
    }

    #[test]
    fn subline_from_joins_branch_and_tail_and_degrades() {
        // Both present -> "branch · tail".
        assert_eq!(
            subline_from(Some("main"), "/code/footnote"),
            Some("main · footnote".into())
        );
        // Branch unresolved -> tail alone (AC1-ERR degradation).
        assert_eq!(
            subline_from(None, "/code/footnote"),
            Some("footnote".into())
        );
        // Trailing slash is trimmed before taking the tail.
        assert_eq!(
            subline_from(None, "/code/footnote/"),
            Some("footnote".into())
        );
        // No cwd -> no subline (AC1-EDGE: no sub-row emitted).
        assert_eq!(subline_from(None, ""), None);
        assert_eq!(subline_from(Some("main"), ""), Some("main".into()));
    }

    #[test]
    fn agent_rows_composes_subline_from_branch_map() {
        // A paneless orphan row joins the off-loop branch map on its cwd; a cwd
        // absent from the map degrades to the tail alone (US4 wire composition).
        let mut core = empty_core();
        core.agents = vec![
            bg_row("worker", "/tmp/repos/footnote", Some("j1")),
            bg_row("other", "/tmp/repos/regready", Some("j2")),
        ];
        core.branch_by_cwd = [("/tmp/repos/footnote".to_string(), "main".to_string())]
            .into_iter()
            .collect();
        let rows = core.agent_rows();
        let footnote = rows.iter().find(|r| r.name == "worker").unwrap();
        assert_eq!(footnote.subline.as_deref(), Some("main · footnote"));
        let regready = rows.iter().find(|r| r.name == "other").unwrap();
        assert_eq!(
            regready.subline.as_deref(),
            Some("regready"),
            "no branch in map -> tail only"
        );
    }

    #[test]
    fn routed_backlog_joins_attach_then_hint_and_leaves_ready_alone() {
        // x-54fa Phase B publish-time join, minus the pane arm (a live pane
        // needs a real PTY; the pane join key - FNO_NODE provenance equality -
        // is covered by the extract_fno_node tests + node_pane's trivial scan).
        let card = |id: &str, state| BacklogCard {
            id: id.into(),
            slug: format!("{id}-slug"),
            priority: "p2".into(),
            state,
            pane_id: None,
            attach_id: None,
            where_hint: None,
        };
        let mut core = empty_core();
        core.backlog = vec![
            card("x-aaa", CardState::InFlight), // attach via name token
            card("x-bbb", CardState::InFlight), // hint via matched row, no jobId
            card("x-ccc", CardState::InFlight), // hint via claim holder
            card("x-ddd", CardState::InFlight), // unroutable, nothing known
            card("x-eee", CardState::Ready),    // never joined
        ];
        core.agents = vec![
            bg_row("tgt-x-aaa", "/w/other", Some("deadbee1")),
            // cwd-basename match (worktree-per-node convention), no jobId.
            bg_row("worker", "/w/x-bbb", None),
            // Rows that must NOT route: exited, pane-hosted, ready-card match.
            RegistryAgent {
                exited: true,
                ..bg_row("tgt-x-ddd", "/w", Some("deadbee2"))
            },
            RegistryAgent {
                mux: Some(("test".into(), 5)),
                ..bg_row("tgt-x-ddd", "/w", Some("deadbee3"))
            },
            bg_row("tgt-x-eee", "/w", Some("deadbee4")),
        ];
        core.backlog_holders
            .insert("x-ccc".into(), "target-session:abc".into());
        let cards = core.routed_backlog();
        assert_eq!(cards[0].attach_id.as_deref(), Some("deadbee1"));
        assert_eq!(cards[0].where_hint, None, "attach route wins over hint");
        assert_eq!(
            cards[1].where_hint.as_deref(),
            Some("in flight - session worker")
        );
        assert_eq!(
            cards[2].where_hint.as_deref(),
            Some("in flight - worked by target-session:abc")
        );
        let bare = &cards[3];
        assert!(
            bare.pane_id.is_none() && bare.attach_id.is_none() && bare.where_hint.is_none(),
            "exited/pane-hosted rows never route"
        );
        let ready = &cards[4];
        assert!(
            ready.attach_id.is_none() && ready.where_hint.is_none(),
            "a ready card is never joined"
        );
    }

    #[test]
    fn inflight_route_resolves_by_id_or_slug_and_fails_closed() {
        // The stale-client DispatchNode re-check (AC2-ERR): an in-flight card
        // with an attach target routes; ready/unknown/unroutable stay None so
        // the handler falls through to dispatch or the refusal notice.
        let mut core = empty_core();
        core.backlog = vec![
            BacklogCard {
                id: "x-aaa".into(),
                slug: "aaa-slug".into(),
                priority: "p2".into(),
                state: CardState::InFlight,
                pane_id: None,
                attach_id: None,
                where_hint: None,
            },
            BacklogCard {
                id: "x-rdy".into(),
                slug: "rdy-slug".into(),
                priority: "p2".into(),
                state: CardState::Ready,
                pane_id: None,
                attach_id: None,
                where_hint: None,
            },
        ];
        core.agents = vec![bg_row("tgt-x-aaa", "/w", Some("deadbee1"))];
        assert_eq!(
            core.inflight_route("x-aaa"),
            Some(Command::attach_agent("deadbee1"))
        );
        assert_eq!(
            core.inflight_route("aaa-slug"),
            Some(Command::attach_agent("deadbee1")),
            "slug names the same card"
        );
        assert_eq!(core.inflight_route("x-rdy"), None, "ready is not routed");
        assert_eq!(core.inflight_route("x-nope"), None, "unknown fails closed");
        core.agents.clear();
        assert_eq!(
            core.inflight_route("x-aaa"),
            None,
            "unroutable in-flight falls through to the refusal notice"
        );
    }

    #[test]
    fn inflight_hint_names_session_then_holder_then_default() {
        // Codex peer review: a stale-client DispatchNode on an in-flight card
        // with NO route must get the situated hint, not the bare not-ready
        // refusal. Hint precedence: matched registry row's session name >
        // claim holder > the client's default copy.
        let mut core = empty_core();
        core.backlog = vec![BacklogCard {
            id: "x-aaa".into(),
            slug: "aaa-slug".into(),
            priority: "p2".into(),
            state: CardState::InFlight,
            pane_id: None,
            attach_id: None,
            where_hint: None,
        }];
        // Nothing known at all: the default copy.
        assert_eq!(
            core.inflight_hint("x-aaa").as_deref(),
            Some("card in flight - no session visible here")
        );
        // A claim holder is known: name it.
        core.backlog_holders
            .insert("x-aaa".into(), "target-session:abc".into());
        assert_eq!(
            core.inflight_hint("aaa-slug").as_deref(),
            Some("in flight - worked by target-session:abc"),
            "slug names the same card"
        );
        // A matched (unattachable) registry row outranks the holder.
        core.agents = vec![bg_row("tgt-x-aaa", "/w", None)];
        assert_eq!(
            core.inflight_hint("x-aaa").as_deref(),
            Some("in flight - session tgt-x-aaa")
        );
        // Not in flight / unknown: None (caller falls through to not-ready).
        assert_eq!(core.inflight_hint("x-nope"), None);
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

    /// A scratch store dir for write-through tests, installed via the per-thread
    /// path override so no test mutates the shared environment (no env race).
    struct StoreScratch {
        dir: std::path::PathBuf,
    }
    impl StoreScratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fno-srv-store-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            crate::squad_store::set_test_path(&dir);
            StoreScratch { dir }
        }
    }
    impl Drop for StoreScratch {
        fn drop(&mut self) {
            crate::squad_store::clear_test_path();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn named_member_squad(core: &mut Core, sid: u64, name: &str, pid: u64, attach: &str) {
        core.session.add_squad(
            sid,
            vec!["/repo".into()],
            Some(name.into()),
            Tab {
                name: None,
                id: sid,
                root: Node::Leaf(pid),
                focus: pid,
            },
        );
        core.squad_members.insert(
            sid,
            vec![crate::squad_store::StoredMember {
                attach_id: attach.into(),
                tombstone: false,
            }],
        );
        core.attached.insert(attach.into(), pid);
    }

    fn stored_member(id: &str, tombstone: bool) -> crate::squad_store::StoredMember {
        crate::squad_store::StoredMember {
            attach_id: id.into(),
            tombstone,
        }
    }

    #[test]
    fn live_ids_from_marks_live_registry_and_roster_rows() {
        // AC1-HP hinges on a FRESH liveness read at first attach (self.agents is
        // still empty then). Pure over the raw file contents: an exited registry
        // row is dead, a live one and a roster worker are live.
        let reg = r#"{"agents":[
            {"name":"w","cwd":"/x","status":"live","provider":"claude","short_id":"c19cd2c3"},
            {"name":"d","cwd":"/x","status":"exited","provider":"claude","short_id":"deadbeef"}
        ]}"#;
        let roster = r#"{"workers":{"k":{"sessionId":"aa11bb22-xyz","cwd":"/y"}}}"#;
        let live = live_ids_from(Some(reg), Some(roster), 0);
        assert!(live.contains("c19cd2c3"), "a live registry row is live");
        assert!(!live.contains("deadbeef"), "an exited row is not live");
        assert!(
            live.contains("aa11bb22"),
            "a roster worker's short_id is live"
        );
        // Missing files (None) yield an empty live set.
        assert!(live_ids_from(None, None, 0).is_empty());
    }

    #[test]
    fn restore_zero_live_squad_gets_a_shell_and_tombstones_dead_members() {
        // AC1-EDGE: a persisted workspace whose members are all dead
        // materializes with one shell pane, each dead member a tombstone; the
        // reconciled tombstone is written back to the store.
        let _s = StoreScratch::new("restore-dead");
        crate::squad_store::upsert(
            "dead-ws",
            &["/tmp".into()],
            &[stored_member("deadbeef", false)],
        )
        .unwrap();
        let mut core = empty_core();
        core.shells = shell_candidates(std::env::var_os("SHELL").as_deref());
        // No live set (no registry/roster under the scratch home).
        core.restore_squads(24, 80, 999);
        assert_eq!(core.session.squads.len(), 1);
        let sq = &core.session.squads[0];
        assert_eq!(sq.name.as_deref(), Some("dead-ws"));
        assert_eq!(sq.tabs.len(), 1, "zero live members -> one shell tab");
        let sid = sq.id;
        assert!(
            core.squad_members[&sid][0].tombstone,
            "the dead member is tombstoned at restore"
        );
        let loaded = crate::squad_store::load();
        assert!(
            loaded.squads[0].members[0].tombstone,
            "the tombstone is persisted"
        );
        // Reap the spawned shell so the test leaks no process.
        let pids: Vec<u64> = core.panes.keys().copied().collect();
        for pid in pids {
            core.reap_pane(pid);
        }
    }

    #[test]
    fn restore_is_a_noop_on_an_empty_store() {
        let _s = StoreScratch::new("restore-empty");
        let mut core = empty_core();
        core.restore_squads(24, 80, 999);
        assert!(
            core.session.squads.is_empty(),
            "nothing persisted -> nothing restored"
        );
    }

    #[test]
    fn recruit_refuses_blank_name_and_empty_ids() {
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RecruitAgents {
                squad: "  ".into(),
                ids: vec!["c19cd2c3".into()],
            },
        );
        core.command(
            1,
            Command::RecruitAgents {
                squad: "team".into(),
                ids: vec![],
            },
        );
        assert_eq!(
            core.session.squads.len(),
            1,
            "no workspace created on refusal"
        );
        assert!(core.squad_members.is_empty());
    }

    #[test]
    fn recruit_skips_bad_unattachable_and_deduped_ids() {
        // All ids fail a gate before any spawn: a bad-shape id, a not-attachable
        // id, and one already recruited (in self.attached). No squad, no panes.
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        core.attached.insert("aaaaaaaa".into(), 1); // already paned -> dedup skip
        core.command(
            1,
            Command::RecruitAgents {
                squad: "team".into(),
                ids: vec![
                    "nothex!!".into(), // bad shape
                    "deadbeef".into(), // not in the catalog -> not attachable
                    "aaaaaaaa".into(), // already recruited
                ],
            },
        );
        assert_eq!(core.session.squads.len(), 1, "no new workspace on all-skip");
        assert!(!core.squad_members.contains_key(&2), "no squad 2 minted");
    }

    #[test]
    fn recruit_refuses_a_name_persisted_but_not_live() {
        // codex P2: recruiting into a name that exists only in the store (another
        // server created it, or restore skipped it) must NOT create a new live
        // squad - that would upsert by name and drop the persisted members.
        let _s = StoreScratch::new("recruit-persisted");
        crate::squad_store::upsert(
            "ghost",
            &["/repo".into()],
            &[stored_member("c19cd2c3", false)],
        )
        .unwrap();
        let mut core = empty_core();
        core.session.add_squad(
            1,
            vec!["/x".into()],
            None,
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.clients.push(client(1, 5, (24, 80), false));
        core.command(
            1,
            Command::RecruitAgents {
                squad: "ghost".into(),
                ids: vec!["deadbeef".into()],
            },
        );
        // The persisted entry is untouched; no live squad was minted for it.
        let loaded = crate::squad_store::load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(
            loaded.squads[0].members,
            vec![stored_member("c19cd2c3", false)],
            "the persisted members are not clobbered"
        );
        assert!(
            !core
                .session
                .squads
                .iter()
                .any(|s| s.name.as_deref() == Some("ghost")),
            "no live squad created for the persisted name"
        );
    }

    #[test]
    fn dismiss_member_removes_a_tombstone_and_refuses_unknown() {
        let _s = StoreScratch::new("dismiss");
        let mut core = empty_core();
        core.session.add_squad(
            7,
            vec!["/repo".into()],
            Some("harden".into()),
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.squad_members.insert(
            7,
            vec![
                stored_member("c19cd2c3", true),
                stored_member("deadbeef", false),
            ],
        );
        core.persist_squad(7);
        core.clients.push(client(1, 5, (24, 80), false));
        // Dismiss the live (non-tombstone) member: refused, nothing removed.
        core.command(
            1,
            Command::DismissMember {
                squad: 7,
                attach_id: "deadbeef".into(),
            },
        );
        assert_eq!(
            core.squad_members[&7].len(),
            2,
            "a live member is not dismissable"
        );
        // Dismiss the tombstone: removed + persisted.
        core.command(
            1,
            Command::DismissMember {
                squad: 7,
                attach_id: "c19cd2c3".into(),
            },
        );
        assert_eq!(core.squad_members[&7].len(), 1);
        let loaded = crate::squad_store::load();
        assert_eq!(
            loaded.squads[0].members,
            vec![stored_member("deadbeef", false)]
        );
        // An unknown workspace is refused.
        core.command(
            1,
            Command::DismissMember {
                squad: 999,
                attach_id: "c19cd2c3".into(),
            },
        );
    }

    #[test]
    fn agent_rows_synthesizes_tombstone_rows_for_dead_members() {
        // AC4-EDGE: a tombstoned member renders dimmed under its squad, carrying
        // its attach_id for DismissMember and exited (so it fails the attach
        // gate). A re-paned id is skipped (rendered pane-hosted instead).
        let mut core = empty_core();
        core.session.add_squad(
            7,
            vec!["/repo".into()],
            Some("harden".into()),
            Tab {
                name: None,
                id: 5,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.squad_members.insert(
            7,
            vec![
                stored_member("c19cd2c3", true),
                stored_member("deadbeef", true),
            ],
        );
        // "deadbeef" is re-paned this session -> skipped in the synthesis.
        core.attached.insert("deadbeef".into(), 99);
        let rows = core.agent_rows();
        let tomb: Vec<_> = rows.iter().filter(|r| r.tombstone).collect();
        assert_eq!(tomb.len(), 1, "only the un-repaned tombstone renders");
        let t = tomb[0];
        assert_eq!(t.squad, Some(7));
        assert!(t.exited, "a tombstone is dimmed/exited");
        assert_eq!(t.attach_id.as_deref(), Some("c19cd2c3"));
        assert_eq!(t.name, "cc-c19cd2c3");
    }

    #[test]
    fn persist_squad_writes_named_workspace_and_dupe_is_taken() {
        let _s = StoreScratch::new("persist-named");
        let mut core = empty_core();
        named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
        core.persist_squad(7);
        let loaded = crate::squad_store::load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(loaded.squads[0].name, "harden");
        assert_eq!(loaded.squads[0].origins, vec!["/repo".to_string()]);
        // A live named squad is taken; a persisted-but-not-live one is too.
        assert!(core.named_squad_taken("harden"), "live name is taken");
        core.session.squads.clear();
        assert!(core.named_squad_taken("harden"), "persisted name is taken");
        assert!(!core.named_squad_taken("nope-zzz"));
    }

    #[test]
    fn churn_tombstones_and_user_close_de_recruits() {
        // AC4-EDGE: a worker dying on its own tombstones its member (survives as
        // a persisted, tombstoned entry). AC3-EDGE: the user closing the pane
        // de-recruits it (gone from the store).
        let _s = StoreScratch::new("churn-vs-user");
        let mut core = empty_core();
        named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
        core.persist_squad(7);

        // Churn: the member is tombstoned, not removed.
        let ctx = core.member_ctx(100);
        assert!(ctx.is_some(), "pane 100 resolves to a persisted member");
        core.reconcile_member_close(ctx, true);
        let after_churn = crate::squad_store::load();
        assert_eq!(after_churn.squads[0].members.len(), 1);
        assert!(
            after_churn.squads[0].members[0].tombstone,
            "churn tombstones"
        );

        // User close of the still-live squad de-recruits the member.
        let ctx = core.member_ctx(100);
        core.reconcile_member_close(ctx, false);
        let after_user = crate::squad_store::load();
        assert_eq!(after_user.squads.len(), 1, "workspace survives");
        assert!(
            after_user.squads[0].members.is_empty(),
            "member de-recruited"
        );
    }

    #[test]
    fn user_close_of_last_member_pane_de_persists_the_workspace() {
        // AC3-EDGE corner: closing the workspace's last pane (squad removed from
        // the session) drops the whole entry, so it never returns at restart.
        let _s = StoreScratch::new("last-pane");
        let mut core = empty_core();
        named_member_squad(&mut core, 7, "harden", 100, "c19cd2c3");
        core.persist_squad(7);
        let ctx = core.member_ctx(100);
        // Simulate the squad already gone (its last tab was removed by close).
        core.session.squads.clear();
        core.reconcile_member_close(ctx, false);
        let loaded = crate::squad_store::load();
        assert!(loaded.squads.is_empty(), "de-persisted with its last pane");
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

    // -- W4 touch telemetry (human_touch emitters) -------------------------

    #[test]
    fn touch_coalesce_window() {
        let mut last = HashMap::new();
        let t0 = Instant::now();
        assert!(
            touch_coalesce(&mut last, 7, t0),
            "first keystroke of a burst emits"
        );
        assert!(
            !touch_coalesce(&mut last, 7, t0 + Duration::from_secs(3)),
            "a keystroke inside the window coalesces into the burst"
        );
        assert!(
            touch_coalesce(&mut last, 7, t0 + Duration::from_secs(6)),
            "past the window a new burst emits"
        );
    }

    #[test]
    fn touch_coalesce_per_pane() {
        let mut last = HashMap::new();
        let t0 = Instant::now();
        assert!(touch_coalesce(&mut last, 1, t0));
        assert!(
            touch_coalesce(&mut last, 2, t0),
            "panes coalesce independently"
        );
    }

    // -- x-9454 wheel-passthrough rate gate --------------------------------

    // AC1-HP / AC2-HP: a 30-tick same-direction flood inside one window
    // forwards at most WHEEL_GATE_BUDGET; ticks spaced past the window all pass.
    #[test]
    fn wheel_gate_bounds_flood_and_passes_notch_rate() {
        let mut g = HashMap::new();
        let t0 = Instant::now();
        let allowed = (0..30)
            .filter(|_| wheel_gate(&mut g, 7, MouseKind::WheelDown, t0))
            .count();
        assert_eq!(
            allowed, WHEEL_GATE_BUDGET as usize,
            "a same-window flood forwards exactly the budget, drops the rest"
        );

        // Notch rate: one tick per window, none dropped.
        let mut g2 = HashMap::new();
        let passed = (0..30)
            .filter(|i| {
                wheel_gate(
                    &mut g2,
                    7,
                    MouseKind::WheelDown,
                    t0 + WHEEL_GATE_WINDOW * (*i as u32),
                )
            })
            .count();
        assert_eq!(passed, 30, "notch-rate input is forwarded 1:1");
    }

    // AC1-EDGE: exactly budget ticks pass, the (budget+1)th in the window drops.
    #[test]
    fn wheel_gate_exact_budget_boundary() {
        let mut g = HashMap::new();
        let t0 = Instant::now();
        for i in 0..WHEEL_GATE_BUDGET {
            assert!(
                wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
                "tick {i} within budget forwards"
            );
        }
        assert!(
            !wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
            "the tick past budget drops"
        );
    }

    // AC1-UI: a reversal mid-flood forwards immediately and resets the budget.
    #[test]
    fn wheel_gate_reversal_passes_immediately() {
        let mut g = HashMap::new();
        let t0 = Instant::now();
        // Exhaust the down budget so drops are occurring.
        for _ in 0..WHEEL_GATE_BUDGET {
            wheel_gate(&mut g, 1, MouseKind::WheelDown, t0);
        }
        assert!(
            !wheel_gate(&mut g, 1, MouseKind::WheelDown, t0),
            "same-direction is dropping"
        );
        assert!(
            wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
            "the opposite tick forwards immediately (reversal is fresh intent)"
        );
        // Reversal reset the window: a fresh up budget is available.
        for _ in 1..WHEEL_GATE_BUDGET {
            assert!(wheel_gate(&mut g, 1, MouseKind::WheelUp, t0));
        }
        assert!(
            !wheel_gate(&mut g, 1, MouseKind::WheelUp, t0),
            "the reset up budget then exhausts"
        );
    }

    // AC1-FR: a tick at or after exactly window_start + window re-admits
    // (no permanent mute), testing the boundary instant itself.
    #[test]
    fn wheel_gate_readmits_at_window_boundary() {
        let mut g = HashMap::new();
        let t0 = Instant::now();
        for _ in 0..WHEEL_GATE_BUDGET {
            wheel_gate(&mut g, 1, MouseKind::WheelDown, t0);
        }
        assert!(
            !wheel_gate(&mut g, 1, MouseKind::WheelDown, t0),
            "budget exhausted inside the window"
        );
        assert!(
            wheel_gate(&mut g, 1, MouseKind::WheelDown, t0 + WHEEL_GATE_WINDOW),
            "the exact boundary instant counts as a fresh window and forwards"
        );
    }

    // AC2-ERR: a now behind window_start (virtualized clock) saturates instead
    // of panicking, and treats the tick as inside the window.
    #[test]
    fn wheel_gate_clock_skew_saturates() {
        let mut g = HashMap::new();
        let t0 = Instant::now() + WHEEL_GATE_WINDOW * 10;
        assert!(wheel_gate(&mut g, 1, MouseKind::WheelDown, t0));
        // A now BEFORE the stored window_start: saturating_duration_since is 0,
        // so the tick is inside the window and consumes budget (never panics).
        let earlier = t0 - WHEEL_GATE_WINDOW * 5;
        for _ in 1..WHEEL_GATE_BUDGET {
            assert!(wheel_gate(&mut g, 1, MouseKind::WheelDown, earlier));
        }
        assert!(
            !wheel_gate(&mut g, 1, MouseKind::WheelDown, earlier),
            "skewed-early ticks stay inside the window and hit the budget"
        );
    }

    // Per-pane independence: one pane's flood never spends another's budget.
    #[test]
    fn wheel_gate_per_pane() {
        let mut g = HashMap::new();
        let t0 = Instant::now();
        for _ in 0..WHEEL_GATE_BUDGET {
            wheel_gate(&mut g, 1, MouseKind::WheelDown, t0);
        }
        assert!(!wheel_gate(&mut g, 1, MouseKind::WheelDown, t0));
        assert!(
            wheel_gate(&mut g, 2, MouseKind::WheelDown, t0),
            "a different pane draws from its own budget"
        );
    }

    // AC1-ERR: a wheel tick routed to a pane absent from the panes map is a
    // no-op through mouse() - the top-of-fn early return fires, so no PTY write
    // and no gate-state entry for the dead id.
    #[test]
    fn mouse_wheel_dead_pane_is_noop() {
        let mut core = empty_core();
        core.mouse(
            1,
            999,
            MouseEvent {
                row: 0,
                col: 0,
                kind: MouseKind::WheelDown,
            },
        );
        assert!(
            core.wheel_gate.is_empty(),
            "no gate state is created for a dead pane"
        );
    }

    // AC2-EDGE: reaping a pane drops its gate entry (touch_last_emit pattern).
    #[test]
    fn reap_pane_clears_wheel_gate() {
        let mut core = empty_core();
        core.wheel_gate.insert(
            42,
            WheelGateState {
                window_start: Instant::now(),
                count: 3,
                dir: MouseKind::WheelDown,
            },
        );
        core.reap_pane(42);
        assert!(
            !core.wheel_gate.contains_key(&42),
            "the closed pane's gate entry is removed"
        );
    }

    #[test]
    fn pane_touch_provenance_cwd_fallback_and_none() {
        let mut core = empty_core();
        // No PaneEntry exists for either pane (no FNO_NODE provenance), so
        // the squad-cwd-basename fallback decides the node id.
        core.session.add_squad(
            1,
            vec!["/tmp/worktrees/x-aff6".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(7),
                focus: 7,
            },
        );
        core.session.add_squad(
            2,
            vec!["/tmp/worktrees/footnote".into()],
            None,
            Tab {
                name: None,
                id: 2,
                root: Node::Leaf(8),
                focus: 8,
            },
        );
        let (node, cwd) = core.pane_touch_provenance(7);
        assert_eq!(node.as_deref(), Some("x-aff6"));
        assert_eq!(cwd.as_deref(), Some("/tmp/worktrees/x-aff6"));
        // Unshaped basename: no node (the emit carries resolution=failed,
        // never a drop - AC4-FR), but the squad cwd still routes the event.
        let (node, cwd) = core.pane_touch_provenance(8);
        assert!(node.is_none());
        assert_eq!(cwd.as_deref(), Some("/tmp/worktrees/footnote"));
        // Unknown pane: (None, None).
        assert_eq!(core.pane_touch_provenance(99), (None, None));
    }

    #[test]
    fn node_id_shape_check() {
        assert!(node_id_shaped("x-aff6"));
        assert!(node_id_shaped("ab-1234abcd"));
        assert!(
            !node_id_shaped("footnote"),
            "a plain squad basename is never a node id"
        );
        assert!(!node_id_shaped("feature-x-aff6"));
        assert!(!node_id_shaped("x-123"), "hex run too short");
        assert!(!node_id_shaped("x-AFF6"), "node ids are lowercase hex");
        assert!(!node_id_shaped("x-aff6789012"), "hex run too long");
        assert!(!node_id_shaped("-aff6"), "empty prefix");
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
            branch_by_cwd: HashMap::new(),
            backlog: Vec::new(),
            backlog_holders: HashMap::new(),
            backlog_pr: HashMap::new(),
            claim_eligible: HashSet::new(),
            claims: HashMap::new(),
            touch_last_emit: HashMap::new(),
            wheel_gate: HashMap::new(),
            touch_emit_failures: Arc::new(AtomicU64::new(0)),
            client_count: watch::channel(0).0,
            seen: HashSet::new(),
            attached: HashMap::new(),
            squad_members: HashMap::new(),
            external_lifecycle: Vec::new(),
            persist_degraded_notified: false,
            restored: false,
        }
    }

    fn placement_core() -> Core {
        let mut core = empty_core();
        core.session.add_squad(
            7,
            vec!["/repo/default".into()],
            Some("review".into()),
            Tab {
                name: None,
                id: 11,
                root: Node::Leaf(1),
                focus: 1,
            },
        );
        core.next_pane_id = 2;
        core.next_squad_id = 8;
        core
    }

    #[test]
    fn pane_placement_resolves_named_and_stale_targets_before_spawn() {
        let core = placement_core();
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::SquadName("review".into()), None),
            Ok(Some(7))
        );
        assert_eq!(
            core.resolve_placement_target(&PaneTarget::SquadId(7), None),
            Ok(Some(7))
        );
        assert!(core
            .resolve_placement_target(&PaneTarget::SquadName("missing".into()), None)
            .is_err());
        assert!(core
            .resolve_placement_target(&PaneTarget::SquadId(99), None)
            .is_err());
    }

    #[test]
    fn pane_placement_splits_on_requested_side_and_focuses_new_pane() {
        let mut core = placement_core();
        core.tab_areas.insert(11, (24, 80));
        let landed = core
            .place_spawned_pane(Some(7), "/repo/child", 2, Some(Dir::Left))
            .unwrap();
        assert_eq!(landed, (7, 11, false));
        let tab = &core.session.squad(7).unwrap().tabs[0];
        assert_eq!(tree::leaves(&tab.root), vec![2, 1]);
        assert_eq!(tab.focus, 2);
    }

    #[test]
    fn pane_placement_split_refusal_falls_back_to_new_tab() {
        // AC3-FR (x-9f75): a split refused at min-size no longer reaps and dead-ends - the pane lands as a
        // NEW TAB in the same squad, the original tab is untouched, and the caller is told to notice.
        let mut core = placement_core();
        core.tab_areas.insert(11, (24, 16));
        core.claim_eligible.insert(2);
        let before = core.session.squad(7).unwrap().tabs[0].clone();
        let (sid, tid, fell_back) = core
            .place_spawned_pane(Some(7), "/repo/child", 2, Some(Dir::Right))
            .unwrap();
        assert!(
            fell_back,
            "the caller must know to emit the tab-full notice"
        );
        assert_eq!(sid, 7);
        let squad = core.session.squad(7).unwrap();
        assert_eq!(squad.tabs.len(), 2, "a new tab was added");
        assert_eq!(squad.tabs[0], before, "the crowded tab is untouched");
        let landed = squad.tabs.iter().find(|t| t.id == tid).unwrap();
        assert_eq!(
            landed.root,
            Node::Leaf(2),
            "pane landed as the new tab's leaf"
        );
        assert!(
            core.claim_eligible.contains(&2),
            "the pane is not reaped, so its claim eligibility survives"
        );
    }

    #[test]
    fn pane_placement_split_without_existing_route_creates_first_tab() {
        let mut core = empty_core();
        let landed = core
            .place_spawned_pane(None, "/repo/new", 1, Some(Dir::Down))
            .unwrap();
        let squad = core.session.squad(landed.0).unwrap();
        assert_eq!(squad.canonical_cwd(), "/repo/new");
        assert_eq!(squad.tabs.len(), 1);
        assert_eq!(squad.tabs[0].root, Node::Leaf(1));
        assert_eq!(squad.tabs[0].focus, 1);
    }

    #[test]
    fn pane_placement_target_does_not_replace_child_cwd() {
        let mut core = placement_core();
        let root = std::env::temp_dir().join(format!("fno-placement-cwd-{}", std::process::id()));
        let child_cwd = root.join("child");
        std::fs::create_dir_all(&child_cwd).unwrap();
        let marker = child_cwd.join("cwd.txt");
        let pid = core
            .run_pane(
                "/repo/default".into(),
                child_cwd.to_string_lossy().into_owned(),
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "pwd > cwd.txt; sleep 30".into(),
                ],
                24,
                80,
                false,
                PanePlacement {
                    target: PaneTarget::SquadName("review".into()),
                    split: None,
                    here: false,
                },
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let reported = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            std::fs::canonicalize(reported.trim()).unwrap(),
            std::fs::canonicalize(&child_cwd).unwrap()
        );
        let (sid, _) = core.session.find_pane(pid).unwrap();
        assert_eq!(sid, 7);

        core.reap_pane(pid);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_pane_create_if_absent_mints_persisted_named_squad() {
        // AC2-HP (x-9f75): a `pane run --squad <name>` naming no existing squad mints a persisted named squad
        // (origins = the spawn's repo root) and lands the pane as its first tab. A second run with the same
        // name joins it - no duplicate mint.
        let _s = StoreScratch::new("run-create-if-absent");
        let mut core = empty_core();
        let run = |core: &mut Core| {
            core.run_pane(
                "/repo/proj".into(),
                "/repo/proj".into(),
                vec!["/bin/cat".into()],
                24,
                80,
                false,
                PanePlacement {
                    target: PaneTarget::SquadName("readyrule".into()),
                    split: None,
                    here: false,
                },
            )
            .unwrap()
        };
        let pid = run(&mut core);
        let (sid, _) = core.session.find_pane(pid).unwrap();
        let sq = core.session.squad(sid).unwrap();
        assert_eq!(sq.name.as_deref(), Some("readyrule"));
        assert_eq!(sq.origins, vec!["/repo/proj".to_string()]);
        assert_eq!(tree::leaves(&sq.tabs[0].root), vec![pid]);
        assert!(
            crate::squad_store::load()
                .squads
                .iter()
                .any(|s| s.name == "readyrule"),
            "the named squad is persisted (write-through)"
        );

        let pid2 = run(&mut core);
        let (sid2, _) = core.session.find_pane(pid2).unwrap();
        assert_eq!(sid2, sid, "the second run joins the existing named squad");
        assert_eq!(
            core.session
                .squads
                .iter()
                .filter(|s| s.name.as_deref() == Some("readyrule"))
                .count(),
            1,
            "no duplicate squad minted"
        );

        core.reap_pane(pid);
        core.reap_pane(pid2);
    }

    #[test]
    fn run_pane_create_if_absent_rejects_blank_name_before_spawn() {
        // A blank/whitespace SquadName is still refused (never a minted squad),
        // and no pane is spawned - fail-closed, mirroring resolve_placement.
        let _s = StoreScratch::new("run-create-blank");
        let mut core = empty_core();
        let before = core.panes.len();
        let err = core
            .run_pane(
                "/repo/proj".into(),
                "/repo/proj".into(),
                vec!["/bin/cat".into()],
                24,
                80,
                false,
                PanePlacement {
                    target: PaneTarget::SquadName("   ".into()),
                    split: None,
                    here: false,
                },
            )
            .unwrap_err();
        assert!(err.contains("blank"), "{err}");
        assert_eq!(core.panes.len(), before, "no pane spawned on a blank name");
        assert!(core.session.squads.is_empty(), "no squad minted");
    }

    #[test]
    fn attach_new_tab_anchors_pane_in_row_cwd() {
        // US5 contract (x-9f75): attaching a watch-only row spawns the pane in the ROW's own cwd, not the
        // viewer's squad cwd. Asserting the existing behavior so it becomes contract, not accident (the
        // interactive cwd chooser is a deferred follow-up; these defaults are the floor).
        let root = std::env::temp_dir().join(format!("fno-row-cwd-{}", std::process::id()));
        let row_cwd = root.join("agent-home");
        std::fs::create_dir_all(&row_cwd).unwrap();
        let marker = row_cwd.join("cwd.txt");
        // The attach spawn writes its pwd then idles, standing in for the real
        // `claude attach <id>` (the id rides as $0 for `sh -c`, harmless).
        set_attach_program(&["/bin/sh", "-c", "pwd > cwd.txt; sleep 30"]);
        let (mut core, client_id, _p1, _p2, _rx) = seen_test_core();
        core.agents = vec![bg_row(
            "home-agent",
            &row_cwd.to_string_lossy(),
            Some("deadbee2"),
        )];

        core.command(client_id, Command::attach_agent("deadbee2"));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let reported = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            std::fs::canonicalize(reported.trim()).unwrap(),
            std::fs::canonicalize(&row_cwd).unwrap(),
            "the attach pane is anchored in the row's own cwd"
        );
        if let Some(&pid) = core.attached.get("deadbee2") {
            core.reap_pane(pid);
        }
        let _ = std::fs::remove_dir_all(root);
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
    fn cold_attach_delivers_every_pane_frame_on_the_reliable_channel() {
        // AC1-FR (x-0296): the cold-attach snapshot must NOT depend on the
        // droppable dirty map or later PTY output. Deterministic mechanism
        // guard: after attach, the client's reliable queue holds the Layout
        // followed by a Frame for EVERY pane in it. Pre-fix this fails 100%
        // (seeds sat only in the dirty map); no timing involved.
        let mut core = empty_core();
        core.shells = vec!["/bin/cat".into()];
        let p1 = core.spawn_pane(24, 40, "/tmp").expect("pane 1");
        let p2 = core.spawn_pane(24, 40, "/tmp").expect("pane 2");
        core.session.add_squad(
            1,
            vec!["/tmp/x0296".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Branch {
                    axis: Axis::Horizontal,
                    children: vec![(0.5, Node::Leaf(p1)), (0.5, Node::Leaf(p2))],
                },
                focus: p1,
            },
        );
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
        core.attach(
            9,
            24,
            80,
            "/tmp/x0296".into(),
            "/tmp/x0296".into(),
            tx,
            Arc::default(),
            Arc::new(Notify::new()),
        );
        let mut msgs = Vec::new();
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        let layout_at = msgs
            .iter()
            .position(|m| matches!(m, ServerMsg::Layout { .. }))
            .expect("attach queues a Layout reliably");
        let layout_panes: HashSet<u64> = match &msgs[layout_at] {
            ServerMsg::Layout { panes, .. } => panes.iter().map(|(pid, _)| *pid).collect(),
            _ => unreachable!(),
        };
        assert_eq!(
            layout_panes,
            HashSet::from([p1, p2]),
            "both panes are in the attach Layout"
        );
        let framed: HashSet<u64> = msgs[layout_at + 1..]
            .iter()
            .filter_map(|m| match m {
                ServerMsg::Frame { pane_id, .. } => Some(*pane_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            framed, layout_panes,
            "every Layout pane's initial frame must ride the reliable \
             channel AFTER the Layout - a dirty-map-only seed is droppable \
             and a passive reattach never recovers it (x-0296)"
        );
    }

    #[test]
    fn focus_only_push_layout_preserves_pending_pane_frames() {
        // AC1-FR (x-0296, the CI root cause): a pane's output-driven frame
        // sits in the client's droppable dirty map until the writer drains
        // it, and it is the ONLY copy - a quiet pane produces no further
        // output to regenerate it (broadcast_pane fires on output only). A
        // focus-only push_layout(reemit=false) landing in that window must
        // not flush it: rects are unchanged, so the frame is still valid.
        // Pre-fix, the unconditional clear() destroyed it 100% here; on the
        // loaded CI runner the shell's "$ " prompt frame died in exactly
        // this window and the pane stayed blank forever.
        let mut core = empty_core();
        core.shells = vec!["/bin/cat".into()];
        let p1 = core.spawn_pane(24, 40, "/tmp").expect("pane 1");
        core.session.add_squad(
            1,
            vec!["/tmp/x0296".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(p1),
                focus: p1,
            },
        );
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
        let dirty: DirtyMap = Arc::default();
        core.attach(
            9,
            24,
            80,
            "/tmp/x0296".into(),
            "/tmp/x0296".into(),
            tx,
            dirty.clone(),
            Arc::new(Notify::new()),
        );
        // Drain the attach traffic (not under test), then land the shell's
        // prompt: the output broadcast seeds the dirty map.
        while rx.try_recv().is_ok() {}
        core.panes.get_mut(&p1).unwrap().vt.feed(b"$ ");
        core.broadcast_pane(p1);
        assert!(
            dirty.lock().unwrap().contains_key(&p1),
            "output must seed the dirty map"
        );
        // A focus-only push races in before the writer drains the frame.
        core.push_layout(false);
        assert!(
            dirty.lock().unwrap().contains_key(&p1),
            "a reemit=false push_layout must preserve a pending frame: it is \
             the only copy of a quiet pane's latest output (x-0296)"
        );
    }

    /// A one-squad, two-tab (one pane each) `Core` with a client attached and
    /// viewing it - the shared rig for the x-4328 seen-bit tests below. The
    /// returned receiver must stay alive for the test's duration: dropping it
    /// closes the client's channel, which `push_layout` reads as "gone" and
    /// prunes the client, breaking every later `Command::FocusPane` (no
    /// client view to act on).
    fn seen_test_core() -> (Core, u64, u64, u64, mpsc::Receiver<ServerMsg>) {
        // attach() below runs a once-per-server restore_squads() ->
        // squad_store::load(), which defaults to the real $HOME/.fno/squads.json.
        // A dev box with a live store imports its squads here (an extra $HOME
        // pane, a squad-id collision), making squad/row-count asserts pass on a
        // fresh-home CI runner but fail locally. Point the store at a per-thread
        // path that does not exist, so load() reads it as an empty store and
        // restore is a no-op. We deliberately do NOT create the dir: a missing
        // file already reads empty, and the store's own writer create_dir_all's
        // its parent, so leaving nothing on disk means nothing to clean up.
        // TEST_PATH is thread-local and one test == one thread, so it never
        // leaks across tests and needs no teardown.
        let scratch = std::env::temp_dir().join(format!(
            "fno-seen-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch); // sweep any stale same-pid dir
        crate::squad_store::set_test_path(&scratch);

        let mut core = empty_core();
        core.shells = vec!["/bin/cat".into()];
        let p1 = core.spawn_pane(24, 40, "/tmp/seen").expect("pane 1");
        let p2 = core.spawn_pane(24, 40, "/tmp/seen").expect("pane 2");
        core.session.add_squad(
            1,
            vec!["/tmp/seen".into()],
            None,
            Tab {
                name: None,
                id: 1,
                root: Node::Leaf(p1),
                focus: p1,
            },
        );
        core.session.squad_mut(1).unwrap().tabs.push(Tab {
            name: None,
            id: 2,
            root: Node::Leaf(p2),
            focus: p2,
        });
        let (tx, rx) = mpsc::channel::<ServerMsg>(32);
        let client_id = 9;
        core.attach(
            client_id,
            24,
            80,
            "/tmp/seen".into(),
            "/tmp/seen".into(),
            tx,
            Arc::default(),
            Arc::new(Notify::new()),
        );
        (core, client_id, p1, p2, rx)
    }

    #[test]
    fn scroll_delta_classifies_only_interpreted_wheel_ticks() {
        // The fold is only correct if it folds real scrolls and STOPS at anything
        // else. A plain pane's wheel is an interpreted scroll (foldable, +/-
        // MOUSE_WHEEL_LINES); a left press is a selection (not foldable) so it must
        // break the run and preserve order.
        let (core, _cid, p1, _p2, _rx) = seen_test_core();
        let ev = |kind| MouseEvent {
            row: 1,
            col: 1,
            kind,
        };
        assert_eq!(
            core.scroll_delta(p1, &ev(MouseKind::WheelUp)),
            Some(MOUSE_WHEEL_LINES)
        );
        assert_eq!(
            core.scroll_delta(p1, &ev(MouseKind::WheelDown)),
            Some(-MOUSE_WHEEL_LINES)
        );
        assert_eq!(
            core.scroll_delta(p1, &ev(MouseKind::Press(MouseButton::Left))),
            None,
            "a select must stop the fold, not coalesce into it"
        );
        assert_eq!(
            core.scroll_delta(999, &ev(MouseKind::WheelUp)),
            None,
            "an unknown pane never folds"
        );
    }

    #[test]
    fn folded_scroll_ticks_preserve_per_tick_clamp_at_a_boundary() {
        // The fold applies ticks IN ORDER via scroll_tick, never by algebraic net:
        // at the live bottom a WheelDown clamps to 0, so a following WheelUp must
        // still move the view up. Netting (-3 + 3 = 0) would wrongly lose the
        // reversal - the exact boundary bug this guards against.
        let (mut core, _cid, p1, _p2, _rx) = seen_test_core();
        // Push content past the 24-row grid so there is scrollback to reveal.
        core.panes
            .get_mut(&p1)
            .unwrap()
            .vt
            .feed("row\r\n".repeat(60).as_bytes());
        assert_eq!(core.scroll_offset(p1), 0, "starts at the live bottom");

        // WheelDown at the bottom clamps (no-op); the reversal WheelUp reveals
        // history. Ordered application lands above the bottom, not back at it.
        core.scroll_tick(p1, -MOUSE_WHEEL_LINES);
        assert_eq!(core.scroll_offset(p1), 0, "down at the bottom clamps");
        core.scroll_tick(p1, MOUSE_WHEEL_LINES);
        assert_eq!(
            core.scroll_offset(p1),
            MOUSE_WHEEL_LINES as usize,
            "the reversal still scrolls up (netting to 0 would strand it)"
        );

        // A mid-history reversal that truly cancels returns to where it started,
        // so the drain's before==after guard skips the redundant broadcast.
        let mid = core.scroll_offset(p1);
        core.scroll_tick(p1, MOUSE_WHEEL_LINES);
        core.scroll_tick(p1, -MOUSE_WHEEL_LINES);
        assert_eq!(core.scroll_offset(p1), mid, "a real cancel nets to no move");
    }

    #[test]
    fn bounded_scroll_target_caps_a_burst_but_spares_a_single_notch() {
        // A big same-direction fold is capped to one viewport (24) either way...
        assert_eq!(bounded_scroll_target(0, 300, 24), 24, "up burst capped");
        assert_eq!(bounded_scroll_target(300, 0, 24), 276, "down burst capped");
        // ...a move already within a screen (a lone wheel notch) is untouched...
        assert_eq!(
            bounded_scroll_target(0, MOUSE_WHEEL_LINES, 24),
            MOUSE_WHEEL_LINES
        );
        // ...a boundary reversal the in-order fold landed at +3 survives the cap
        // (it never re-introduces the netting bug)...
        assert_eq!(bounded_scroll_target(0, 3, 24), 3);
        // ...and a true no-op fold stays put.
        assert_eq!(bounded_scroll_target(10, 10, 24), 10);
    }

    #[test]
    fn focus_pane_marks_a_done_pane_seen() {
        // AC1-HP: focusing a `Done` pane inserts it into `Core.seen`.
        let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
        core.command(client_id, Command::FocusPane(p1));
        assert!(
            core.seen.contains(&p1),
            "focusing a Done pane marks it seen"
        );
    }

    #[test]
    fn a_re_run_evicts_and_does_not_self_reinsert() {
        // AC1-EDGE: Done(focused) -> seen; the pane re-runs to Working (the
        // level-triggered evict in push_layout drops it); it finishes to
        // Done again WITHOUT a fresh focus action - it must stay unseen
        // until re-focused, not re-arm itself just because focus never left.
        let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
        core.command(client_id, Command::FocusPane(p1));
        assert!(core.seen.contains(&p1), "precondition: seen after focus");

        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Working), false)];
        core.push_layout(true);
        assert!(!core.seen.contains(&p1), "a Working tick evicts it");

        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
        core.push_layout(true);
        assert!(
            !core.seen.contains(&p1),
            "the second Done must stay unseen until re-focused - insert is \
             a one-shot side effect of FocusPane, never a per-pass level \
             check on \"is this still the focused pane\""
        );

        core.command(client_id, Command::FocusPane(p1));
        assert!(core.seen.contains(&p1), "re-focusing re-arms seen");
    }

    #[test]
    fn focusing_while_working_never_seeds_a_later_done() {
        // AC2-EDGE: a focus action that lands while the badge is `Working`
        // must not mark the pane seen once it later finishes unattended.
        let (mut core, client_id, p1, _p2, _rx) = seen_test_core();
        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Working), false)];
        core.command(client_id, Command::FocusPane(p1));
        assert!(
            !core.seen.contains(&p1),
            "a Working-time focus never sets the done-seen bit"
        );

        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
        core.push_layout(true);
        assert!(
            !core.seen.contains(&p1),
            "finishing without a fresh focus action stays unseen"
        );
    }

    #[test]
    fn evict_is_per_pane_not_per_focus() {
        // A non-focused Done pane's seen bit (set earlier) is untouched by a
        // push_layout pass that focuses a DIFFERENT pane; eviction keys on
        // the pane's OWN current badge, never on which pane is focused.
        let (mut core, client_id, p1, p2, _rx) = seen_test_core();
        core.agents = vec![agent_in("test", p1, Some(AgentBadge::Done), false)];
        core.command(client_id, Command::FocusPane(p1));
        assert!(core.seen.contains(&p1));

        core.agents = vec![
            agent_in("test", p1, Some(AgentBadge::Done), false),
            agent_in("test", p2, Some(AgentBadge::Working), false),
        ];
        core.command(client_id, Command::FocusPane(p2));
        assert!(
            core.seen.contains(&p1),
            "p1 stays seen: it is still Done, just no longer focused"
        );
        assert!(!core.seen.contains(&p2), "p2 never reached Done");
    }

    #[test]
    fn gone_decrements_the_published_client_count() {
        // AC4-EDGE (x-4e30): detach via CoreMsg::Gone must drop the published
        // count - the regression the original 4-site enumeration would have
        // shipped (a count stuck high means the idle gate and the FNO_E2E
        // reaper never fire).
        let (tx, rx) = watch::channel(0usize);
        let mut core = empty_core();
        core.client_count = tx;
        core.clients.push(client(1, 5, (24, 80), false));
        core.publish_client_count();
        assert_eq!(*rx.borrow(), 1);
        core.handle(CoreMsg::Gone(1));
        assert_eq!(
            *rx.borrow(),
            0,
            "Gone must publish the decremented count via the handle-tail choke point"
        );
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

    #[test]
    fn reap_notice_maps_reaped_count() {
        // AC1-HP: the reaped array length is the visible count.
        assert_eq!(
            reap_notice(r#"{"reaped":["a","b","c"],"kept_dirty":[]}"#),
            "reaped 3"
        );
        // AC1-EDGE: zero candidates is a successful visible `reaped 0`, not an
        // error and not silence.
        assert_eq!(reap_notice(r#"{"reaped":[],"kept_dirty":[]}"#), "reaped 0");
        // A missing `reaped` key (schema drift) reads as zero reaped.
        assert_eq!(reap_notice(r#"{"kept_dirty":[]}"#), "reaped 0");
        // The verb exited zero, so unparseable stdout still reports success (the
        // row-vanish is authoritative), never a false failure.
        assert_eq!(reap_notice("not json"), "reap: done");
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
