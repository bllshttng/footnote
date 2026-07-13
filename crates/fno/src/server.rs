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
    /// re-pushes layouts (rects unchanged, so no frame re-emit).
    AgentRows(Vec<RegistryAgent>),
    /// (x-6f77) A fresh board-ordered work-queue card set from the off-loop graph
    /// reader, claim-overlaid (x-54fa). Sent only when the set changed; the core
    /// stores it and re-pushes layouts so the sideline backlog lane tracks
    /// claims/closes. `holders` is the sweep's node-id -> claim-holder map,
    /// consumed at publish time for the `where_hint` of unroutable cards.
    BacklogCards {
        cards: Vec<BacklogCard>,
        holders: HashMap<String, String>,
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

/// The spawned command's basename for the tab-label chain (x-c150): the first
/// argv token past an optional leading `env` + its `NAME=VALUE` run (the same
/// scan shape as [`node_from_argv`]). `None` when the scan finds no command -
/// spawn never fails on labeling.
fn cmd_from_argv(argv: &[String]) -> Option<String> {
    let cmd = if argv.first().map(String::as_str) == Some("env") {
        argv.iter().skip(1).find(|a| !a.contains('='))?
    } else {
        argv.first()?
    };
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    (!base.is_empty()).then(|| base.to_string())
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
    /// Latest board-ordered work-queue cards (x-6f77), from the off-loop graph
    /// reader; packed into every `Layout` for the sideline backlog lane.
    backlog: Vec<BacklogCard>,
    /// Claim holder per in-flight node id (x-54fa), from the reader's sweep;
    /// joined at publish time into card routes / `where_hint`.
    backlog_holders: HashMap<String, String>,
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
async fn run_dispatch_one(session: &str, node: Option<&str>) -> String {
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
        self.register_pane(id, pty, rows, cols, None, cwd.to_string(), None);
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
        self.register_pane(id, pty, rows, cols, node, cwd.to_string(), cmd);
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

    /// `pane run`: spawn `argv` as a new tab-pane in the squad `squad_key`
    /// names (created if absent). PTY-first ordering (Locked 7): a spawn
    /// failure mutates no model. Each call is its own pane, so three runs into
    /// one cwd land three panes in one squad (no mux-layer dedup - that lives
    /// in the spawn front half).
    /// Resolve a placement target to a concrete squad id, BEFORE any PTY spawn
    /// (Locked 4 / AC4 fail-closed). `CurrentRoute` yields `current` (the
    /// caller's cwd/owner default, `None` when no squad exists yet and one must
    /// be born). An explicit name/id that is missing, ambiguous, or stale is
    /// refused - it never spawns a PTY and never creates a named squad.
    fn resolve_placement_target(
        &self,
        target: &PaneTarget,
        current: Option<u64>,
    ) -> Result<Option<u64>, String> {
        match target {
            PaneTarget::CurrentRoute => Ok(current),
            PaneTarget::SquadName(name) => {
                let n = name.trim();
                let mut hits = self
                    .session
                    .squads
                    .iter()
                    .filter(|s| s.name.as_deref() == Some(n));
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

    /// Commit an already-spawned pane `pid` per `split`, the single atomic
    /// placement helper shared by pane-run and fresh attach (Locked 1). `dest`
    /// is `None` only for a CurrentRoute miss, where a squad is born from
    /// `squad_key` with `pid` as its first tab (split collapses to first-tab -
    /// AC6-EDGE, a lone pane has no sibling to split against). Otherwise a
    /// `None` split mints a new tab; a `Some` split inserts beside the
    /// destination's active-tab focus. On a split refusal the pane is reaped and
    /// the prior tree is left byte-for-byte unchanged (AC7). Returns the
    /// `(squad, tab)` the pane landed in and leaves it focused.
    fn place_spawned_pane(
        &mut self,
        dest: Option<u64>,
        squad_key: &str,
        pid: u64,
        split: Option<Dir>,
    ) -> Result<(u64, TabId), String> {
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
                return Ok((sid, tid));
            }
        };
        let squad = self
            .session
            .squad(sid)
            .ok_or_else(|| "target squad vanished".to_string())?;
        // First-pane-of-an-empty-squad and split-omitted both mint a fresh tab.
        // A squad always carries >=1 tab in practice; the empty guard keeps the
        // active-tab index safe (AC6-EDGE first-tab collapse) rather than
        // underflowing.
        if split.is_none() || squad.tabs.is_empty() {
            let tid = self.session.mint_tab_id();
            let tab = Tab {
                name: None,
                id: tid,
                root: Node::Leaf(pid),
                focus: pid,
            };
            self.session
                .squad_mut(sid)
                .expect("squad present")
                .tabs
                .push(tab);
            return Ok((sid, tid));
        }
        let dir = split.expect("split present");
        let ti = squad.active_tab.min(squad.tabs.len() - 1);
        let tid = squad.tabs[ti].id;
        let vp = self.tab_rect(tid);
        let tab = &mut self
            .session
            .squad_mut(sid)
            .expect("squad present")
            .tabs[ti];
        match tree::split_directional(tab, vp, dir, pid) {
            Ok(()) => Ok((sid, tid)),
            Err(e) => {
                // Refused split: reap the pre-spawned pane; the tree, claim
                // eligibility, and mappings are all cleared/untouched (AC7).
                self.reap_pane(pid);
                Err(e.to_string())
            }
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
        // Resolve an explicit target before spawning a PTY - a missing squad
        // must fail closed with no process wasted (AC4).
        let current = self.session.find_by_cwd(&squad_key);
        let dest = self.resolve_placement_target(&placement.target, current)?;
        let pid = self.spawn_pane_cmd(&argv, rows, cols, &cwd)?;
        if claim {
            // Writer-claim ELIGIBILITY, set only at agent spawn (Locked 5).
            // The claim itself is acquired per-burst via PaneClaim. A later
            // placement refusal reaps the pane, which clears this again.
            self.claim_eligible.insert(pid);
        }
        // place_spawned_pane reaps `pid` (clearing claim eligibility) on refusal.
        self.place_spawned_pane(dest, &squad_key, pid, placement.split)?;
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
        live_ids_from(reg.as_deref(), roster.as_deref(), now)
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
        if loaded.squads.is_empty() {
            return;
        }
        let live = self.live_attach_ids_now();
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
                // Live: re-attach it into a fresh pane.
                let argv = vec![
                    "claude".to_string(),
                    "attach".to_string(),
                    m.attach_id.clone(),
                ];
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
    fn dispatch_next(&self, id: u64, node: Option<String>) {
        let session = self.session_name.clone();
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_dispatch_one(&session, node.as_deref()).await;
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

    /// Resolve a sideline lifecycle target (x-76ea `StopAgent`/`RemoveAgent`) by
    /// name against the current catalog, returning the exited flag of the single
    /// resolved registry row. `name` is NOT a unique key (codex review): the
    /// catalog dedups by `attach_id`, so an external roster row and a registry
    /// row can carry the same name. Fail-closed on every ambiguity - absent, any
    /// external row sharing the name (never act on a registry agent an external
    /// shadows), or a >1 non-external collision - so a keypress can only ever act
    /// on exactly one unambiguous registry agent, never a guessed match.
    fn resolve_lifecycle_target(&self, name: &str) -> Result<bool, String> {
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
            [one] => Ok(one.exited),
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
    fn agent_rows(&self) -> Vec<AgentRow> {
        let mut out = Vec::new();
        // Which registry agents a pane row already claimed (so they don't
        // double-render as watch-only). Indexed like `self.agents`.
        let mut consumed = vec![false; self.agents.len()];

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
                });
            }
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
            Command::AttachAgent { id, placement: _ } => {
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
                            self.push_layout(true);
                            return Flow::Continue;
                        }
                    }
                    self.attached.remove(&id);
                }
                // Resolve the OWNING squad (Locked 2): the first squad whose
                // `owns_path` matches the watch-only row's registry cwd, so the
                // attach lands where the agent lives, not the viewer's squad;
                // fall back to the viewed squad for an orphan (AC1-EDGE).
                let owner = self
                    .agents
                    .iter()
                    .find(|a| a.mux.is_none() && !a.exited && a.attach_id.as_deref() == Some(&id))
                    .map(|a| a.cwd.clone())
                    .and_then(|cwd| {
                        self.session
                            .squads
                            .iter()
                            .find(|s| s.owns_path(&cwd))
                            .map(|s| s.id)
                    })
                    .unwrap_or(view.0);
                // Spawn `claude attach <id>` as a new tab in the owning squad: the
                // claude supervisor PTYs the detached bg session into this pane.
                // cwd is the squad's, like a new tab - attach connects by id, so
                // the dir is cosmetic.
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                let squad_cwd = self
                    .session
                    .squad(owner)
                    .map(|s| s.canonical_cwd().to_string())
                    .unwrap_or_default();
                let argv = vec!["claude".to_string(), "attach".to_string(), id.clone()];
                let pid = match self.spawn_pane_cmd(&argv, rows, cols, &squad_cwd) {
                    Ok(p) => p,
                    Err(e) => {
                        self.notice(client_id, format!("attach failed: {e}"));
                        return Flow::Continue;
                    }
                };
                let tid = self.session.mint_tab_id();
                let Some(squad) = self.session.squad_mut(owner) else {
                    return Flow::Continue;
                };
                squad.tabs.push(Tab {
                    name: None,
                    id: tid,
                    root: Node::Leaf(pid),
                    focus: pid,
                });
                // Record the reconcile mapping AFTER the spawn+model mutation
                // succeed (PTY-first: a failed spawn returned above, writing no
                // entry - AC1-ERR leaves the row watch-only).
                self.attached.insert(id, pid);
                self.set_view(client_id, owner, tid);
                self.push_layout(true);
                Flow::Continue
            }
            Command::DispatchNode(node) => {
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
                    self.dispatch_next(client_id, Some(node));
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
                    let argv = vec!["claude".to_string(), "attach".to_string(), id.clone()];
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
                    Ok(_exited) => self.agent_action(client_id, "stop", name),
                }
                Flow::Continue
            }
            Command::RemoveAgent { name } => {
                // Remove an exited sideline row (x-76ea). Same resolution as
                // StopAgent, plus the stop-then-rm ordering: a still-live row is
                // refused with the stop-first reason (the CLI enforces this too,
                // but refusing here keeps the notice specific and skips a doomed
                // subprocess).
                match self.resolve_lifecycle_target(&name) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(exited) if !exited => {
                        self.notice(client_id, format!("{name} is still live - stop it first"))
                    }
                    Ok(_) => self.agent_action(client_id, "rm", name),
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
            CoreMsg::DispatchNext { id } => {
                self.dispatch_next(id, None);
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
            CoreMsg::AgentRows(rows) => {
                self.agents = rows;
                // Rects are unchanged; only the Layout's agent rows moved -
                // push without re-emitting frames (AC1-UI: visible within one
                // layout push; AC2-UI: the read happened off-loop).
                self.push_layout(false);
                Flow::Continue
            }
            CoreMsg::BacklogCards { cards, holders } => {
                // Same as AgentRows: only sideline data moved, so push the
                // Layout without a frame re-emit (x-6f77).
                self.backlog = cards;
                self.backlog_holders = holders;
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
        backlog: Vec::new(),
        backlog_holders: HashMap::new(),
        claim_eligible: HashSet::new(),
        claims: HashMap::new(),
        touch_last_emit: HashMap::new(),
        touch_emit_failures: Arc::new(AtomicU64::new(0)),
        client_count: client_count_tx,
        seen: HashSet::new(),
        attached: HashMap::new(),
        squad_members: HashMap::new(),
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
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Some(rows) = state.tick(
                    reg_stamp,
                    move || reg_raw,
                    roster_stamp,
                    move || roster_raw,
                    now,
                ) {
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
                if let Some(cards) = state.tick(stamp, move || raw, last_live.as_ref()) {
                    let holders = last_live.clone().unwrap_or_default();
                    if core_tx
                        .send(CoreMsg::BacklogCards { cards, holders })
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

        let (sid, _tid) = core.place_spawned_pane(Some(1), "/a", 2, None).unwrap();
        assert_eq!(sid, 1);
        assert_eq!(
            core.session.squad(1).unwrap().tabs.len(),
            2,
            "omitted split pushes a new tab"
        );

        let tabs_before = core.session.squad(1).unwrap().tabs.len();
        let (_sid, tid) = core
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
        let (sid, tid) = core
            .place_spawned_pane(None, "/fresh", 9, Some(Dir::Left))
            .unwrap();
        let sq = core.session.squad(sid).unwrap();
        assert_eq!(sq.tabs.len(), 1);
        assert_eq!(sq.tabs[0].id, tid);
        assert_eq!(tree::leaves(&sq.tabs[0].root), vec![9]);
        assert_eq!(sq.origins, vec!["/fresh".to_string()]);
    }

    #[test]
    fn place_spawned_pane_reaps_on_min_size_refusal() {
        // AC7: a split that would violate minimum size reaps the pane and leaves
        // the prior tree byte-for-byte unchanged.
        let mut core = empty_core();
        core.session
            .add_squad(1, vec!["/a".into()], None, leaf_tab(5, 1));
        // 8 cols cannot hold two MIN_COLS(8)-wide halves -> horizontal refusal.
        core.tab_areas.insert(5, (40, 8));
        let before = core.session.squad(1).unwrap().tabs[0].root.clone();

        let err = core
            .place_spawned_pane(Some(1), "/a", 3, Some(Dir::Right))
            .unwrap_err();
        assert!(!err.is_empty(), "the refusal names a reason");
        assert_eq!(
            core.session.squad(1).unwrap().tabs[0].root,
            before,
            "a refused split leaves the tree untouched"
        );
        assert!(
            !core.panes.contains_key(&3),
            "the pre-spawned pane is reaped"
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
        }
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
            {"name":"w","cwd":"/x","status":"live","claude_short_id":"c19cd2c3"},
            {"name":"d","cwd":"/x","status":"exited","claude_short_id":"deadbeef"}
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
            backlog: Vec::new(),
            backlog_holders: HashMap::new(),
            claim_eligible: HashSet::new(),
            claims: HashMap::new(),
            touch_last_emit: HashMap::new(),
            touch_emit_failures: Arc::new(AtomicU64::new(0)),
            client_count: watch::channel(0).0,
            seen: HashSet::new(),
            attached: HashMap::new(),
            squad_members: HashMap::new(),
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
        assert_eq!(landed, (7, 11));
        let tab = &core.session.squad(7).unwrap().tabs[0];
        assert_eq!(tree::leaves(&tab.root), vec![2, 1]);
        assert_eq!(tab.focus, 2);
    }

    #[test]
    fn pane_placement_refusal_preserves_tree_and_cleans_spawn_state() {
        let mut core = placement_core();
        core.tab_areas.insert(11, (24, 16));
        core.claim_eligible.insert(2);
        let before = core.session.squad(7).unwrap().tabs[0].clone();
        let error = core
            .place_spawned_pane(Some(7), "/repo/child", 2, Some(Dir::Right))
            .unwrap_err();
        assert!(error.contains("smaller"), "{error}");
        assert_eq!(core.session.squad(7).unwrap().tabs[0], before);
        assert!(!core.claim_eligible.contains(&2));
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
