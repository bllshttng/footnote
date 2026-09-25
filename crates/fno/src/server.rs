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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use crate::agents_view::{self, RegistryAgent};
use crate::backlog_view;
use crate::proto::{
    bind_or_probe, check_attach_version, err_code, read_msg, write_msg, AgentBadge,
    AgentNoPaneReason, AnchoredLayoutSpec, BacklogCard, BindOutcome, BlockDir, BlockSel, CardState,
    ClientMsg, Command, ControlVerb, Frame, LayoutBinding, LayoutScope, LayoutSlot, LayoutSpec,
    LayoutTreeChild, LayoutTreeSpec, MouseButton, MouseEvent, MouseKind, PaneInfo, PanePlacement,
    PaneTarget, PlacementFallback, PortalSlot, ProtoError, Reach, ResolvedPlacement, RestoreRow,
    ServerMsg, SlotBinding, SlotOutcome, SlotResult, SquadLayout, SquadMeta, TabInfo, TabLayout,
    TabMeta, TabPaneOccupant, TabSel, WaitOutcome, MAX_SQUAD_NAME, MAX_TAB_NAME,
};
use crate::pty::{shell_candidates, PaneChunk, PtyShell};
use crate::restore_liveness::{
    classify_member, no_resume_form_reason, restore_worker_refusal_reason, worker_registry_match,
    MemberVerdict,
};
#[cfg(test)]
use crate::spawn_journal::parse_spawn_receipts;
use crate::spawn_journal::{
    receipt_for_member, scan_spawn_journal, worker_binding_key, BatchReplay, DetachedPane,
    HeldWorker, ReentrySpawnRequest, ReentryVerdict, SpawnJournal,
};
use crate::squad::{self, MoveTabOutcome, RemoveOutcome, Resolver, Session, Squad};
use crate::squad_store::{SquadSnapshot, StoredTabTree};
use crate::thread_viewer::Portal;
use crate::tree::{self, Axis, Dir, Node, Rect, Tab, TabId};
use crate::vt::BlockJumpOutcome;
use crate::vt::{self, frame_text, Modes};

mod agent_actions;
pub(crate) mod agent_launch;
mod agent_rows_join;
mod drift_retire;
mod human_input;
mod keeper_adopt;
pub(crate) mod lifecycle_target;
mod pane_close;
mod pane_identity;
mod pane_release;
mod pane_reseat;
pub(crate) mod placement_fit;
mod portal_reach;
mod restore_route_gate;
mod resume_argv;
mod retire_session;
mod revival_gate;
mod row_set;
mod session_guard;
mod shutdown_capture;
mod squad_persistence;
mod squad_sync;
mod truth_probe;
mod workspace_restore;
use self::session_guard::{ConnAlive, SocketGuard};

use self::agent_actions::{run_mail_send, run_reap, run_reentry_plan};
use self::keeper_adopt::{keeper_worker_bin, AdoptedKeeper};
use self::resume_argv::{
    declared_resume_form, resume_argv_for, resume_target_from_argv, ResumeReplay,
};
#[cfg(test)]
use self::resume_argv::{
    set_declared_resume_form, set_resume_program, DeclaredResumeFormsGuard, ResumeProgramGuard,
};
use self::truth_probe::TruthReading;
use self::truth_probe::{probe_truth_map, TruthProbeLatch, TRUTH_PROBE_EVERY};

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

/// Give a responsive core time to flush topology and run normal cleanup after
/// the signal waiter submits its shutdown message before taking the emergency
/// path for a wedged core.
const SIGNAL_CORE_GRACE: Duration = Duration::from_secs(2);

/// The droppable outbound path: newest unsent frame per pane, per client.
type DirtyMap = Arc<Mutex<HashMap<u64, Frame>>>;

/// Cadence of the `mux_pane_counters` snapshot emit. 30s is far finer than
/// any per-pane pricing window and keeps the global journal at ~2.9k rows/day.
const PANE_STATS_CADENCE: Duration = Duration::from_secs(30);

/// Per-pane monotonic counters, the mux's share of fleet cost attribution.
/// All five are totals since pane birth, NEVER rates: every rate read during
/// the measurement session that motivated them was wrong or inside the noise
/// floor, and the correct answers all came from differencing two snapshots.
/// `frames_composited` counts PER-VIEWING-CLIENT frame enqueues (a shared
/// `Frame` fanned out to two clients is 2), increments only PAST the
/// `broadcast_pane` visible-gate so a fed-but-unviewed pane stays flat, and
/// pairs one-for-one with `frames_emitted`'s per-client wire writes:
/// composited > emitted is the newest-wins dirty map dropping a frame copy
/// before the wire. That pair decides whether feeding or display is the
/// cost. `cpu_ns` is wall-clock around
/// core-loop work (VT feed + frame build/fan-out): every pane shares the one
/// loop thread, so per-thread CPU cannot attribute it, and writer-task
/// serialization stays unattributed by design (visible only in the process
/// total). Relaxed ordering throughout - nothing synchronizes on a counter.
#[derive(Default)]
struct PaneCounters {
    bytes_in: AtomicU64,
    grid_updates: AtomicU64,
    frames_composited: AtomicU64,
    frames_emitted: AtomicU64,
    cpu_ns: AtomicU64,
}

/// pane id -> live counters. Shared with the per-client writer tasks, which
/// know only the pane id at write time; the core loop reaches its counters
/// through `PaneEntry::stats` and never takes this lock. Rows live exactly as
/// long as the pane (born in `register_pane`, dropped in `reap_pane`) - a
/// reaped pane's final partial window is lost, which is the accepted cost of
/// not growing a second store.
type PaneStats = Arc<RwLock<HashMap<u64, Arc<PaneCounters>>>>;

/// The only pane state visible to the emergency signal waiter. Keeper-hosted
/// children belong to their keeper and survive this server; plain children
/// belong to this server's shutdown sweep.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PaneChild {
    pid: u32,
    keeper_hosted: bool,
}

type PaneChildRoster = Arc<Mutex<HashSet<PaneChild>>>;

/// Block termination signals and start the one waiter that can end a wedged
/// core loop. Threads created later inherit this blocked mask.
fn install_signal_reaper(
    socket: &Path,
    core_tx: mpsc::Sender<CoreMsg>,
    shutdown_complete: Arc<AtomicBool>,
) -> Result<PaneChildRoster, String> {
    let mut signal_set = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let empty = unsafe { libc::sigemptyset(&mut signal_set) };
    if empty != 0 {
        return Err(format!(
            "sigwait setup failed at sigemptyset: {}",
            std::io::Error::last_os_error()
        ));
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let added = unsafe { libc::sigaddset(&mut signal_set, signal) };
        if added != 0 {
            return Err(format!(
                "sigwait setup failed at sigaddset({signal}): {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    let blocked =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set, std::ptr::null_mut()) };
    if blocked != 0 {
        return Err(format!(
            "sigwait setup failed at pthread_sigmask: {}",
            std::io::Error::from_raw_os_error(blocked)
        ));
    }
    let roster = Arc::new(Mutex::new(HashSet::new()));
    let waiter_roster = Arc::clone(&roster);
    let waiter_socket = socket.to_path_buf();
    let waiter_complete = Arc::clone(&shutdown_complete);
    std::thread::Builder::new()
        .name("fno-mux-sigwait".into())
        .spawn(move || {
            signal_waiter(
                signal_set,
                waiter_roster,
                waiter_socket,
                core_tx,
                waiter_complete,
            )
        })
        .map_err(|e| format!("sigwait setup failed to spawn waiter: {e}"))?;
    Ok(roster)
}

fn signal_waiter(
    signal_set: libc::sigset_t,
    roster: PaneChildRoster,
    socket: PathBuf,
    core_tx: mpsc::Sender<CoreMsg>,
    shutdown_complete: Arc<AtomicBool>,
) {
    let mut received = 0;
    let result = unsafe { libc::sigwait(&signal_set, &mut received) };
    if result != 0 {
        eprintln!(
            "fno mux: sigwait failed for SIGTERM/SIGINT: {}",
            std::io::Error::from_raw_os_error(result)
        );
        unsafe { libc::_exit(1) }
    }
    if core_tx.try_send(CoreMsg::Kill).is_ok() {
        let deadline = Instant::now() + SIGNAL_CORE_GRACE;
        while !shutdown_complete.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if shutdown_complete.load(Ordering::Acquire) {
            return;
        }
    }
    let snapshot = match roster.lock() {
        Ok(children) => children.iter().copied().collect::<HashSet<_>>(),
        Err(_) => {
            eprintln!("fno mux: emergency pane roster lock poisoned");
            unsafe { libc::_exit(1) }
        }
    };
    kill_plain_children(&snapshot);
    let _ = crate::proto::remove_session_files(&socket);
    crate::proto::remove_startup_guard(&socket);
    unsafe { libc::_exit(0) }
}

fn kill_plain_children(roster: &HashSet<PaneChild>) {
    let plain = roster
        .iter()
        .filter(|child| !child.keeper_hosted)
        .copied()
        .collect::<Vec<_>>();
    for child in &plain {
        let result = unsafe { libc::kill(child.pid as libc::pid_t, libc::SIGKILL) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                eprintln!(
                    "fno mux: emergency SIGKILL failed for pane child {}: {error}",
                    child.pid
                );
            }
        }
    }
    for child in plain {
        reap_plain_child(child.pid);
    }
}

fn reap_plain_child(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if result == pid as libc::pid_t {
            return;
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ECHILD | libc::ESRCH)) {
                return;
            }
            eprintln!("fno mux: emergency waitpid failed for pane child {pid}: {error}");
            return;
        }
        if Instant::now() >= deadline {
            eprintln!("fno mux: emergency waitpid timed out for pane child {pid}");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Lines a single wheel notch scrolls a mux-interpreted pane. ONE, because the
/// host terminal already did the accumulation and a notch here IS one cell of
/// finger travel - multiplying re-amplifies input that arrives pre-normalized.
///
/// Every precision-scroll host converts trackpad pixel deltas into whole cells
/// by accumulating a float and keeping the sub-cell remainder, then emits one
/// wheel notch per cell crossed: alacritty `input/mod.rs::scroll_terminal`
/// (`accumulated_scroll.y %= height`, and it forces its user multiplier to 1
/// while an app owns the mouse - which is us), ghostty `Surface.zig`
/// (`pending_scroll_y`, `mouse-scroll-multiplier.precision` default 1).
///
/// Three is the right step for a DISCRETE wheel detent, where the host emits
/// one notch per physical click and a 1:1 step feels sluggish. It is 3x too
/// fast for a trackpad, and SGR reports both as button 64/65 with nothing to
/// tell them apart - so this favors the trackpad, the common case. A wheel-mouse
/// operator wanting a coarser step is the config knob this deliberately does not
/// have yet; add it when someone asks, not on speculation.
const MOUSE_WHEEL_LINES: i32 = 1;

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
/// sideline highlight) and never forwards it, so a pane app's own
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
/// (`ESC [ < b; x; y {M|m}`, brief Locked 12 / Domain: SGR 1006 only). `b` is
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
/// (v8). Jump/select carry a walk direction; rerun re-sends the
/// selected block's command line under the idle guard.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BlockNavOp {
    Jump(BlockDir),
    Select(BlockDir),
    Rerun,
}

/// One in-scrollback search operation the core applies to a pane (v12).
/// Open carries the query (owned - the wire string); step carries the walk
/// direction (reusing [`BlockDir`]); clear drops the search. Each mutates the
/// shared pane and, on open/step, replies a `SearchResult` to the initiator.
#[derive(Debug, Clone)]
pub(crate) enum SearchOp {
    Open(String),
    Step(BlockDir),
    Clear,
}

/// The rerun idle guard, pure over the registry rows so the routing
/// (the safety-critical bit) is unit-testable without a Core. A pane with no
/// agent row is a plain shell - always safe. An agent pane must prove idle: a
/// `Done`/exited badge allows; a `Working`/`Blocked` badge refuses (busy); an
/// unknown badge (liveness-only, no fresh hook report / manifest verdict)
/// refuses fail-closed, and false-ready is the forbidden direction.
///
/// Refusing mid-turn is still the conservative call HERE, but not for the
/// reason this comment used to give: node measured, from a live claude
/// session's own transcript, that a busy recipient does not corrupt its
/// composer on an injected turn - it enqueues the paste and processes it at
/// the next turn boundary (`queue-operation`/`enqueue`, confirmed via
/// `crates/fno-agents/src/mail_inject.rs`). That measurement is about a
/// bracketed-paste MAIL turn landing through the harness's own input-queue
/// feature; it says nothing about `BlockNavOp::Rerun`'s write below, which
/// puts a raw re-typed command line directly onto the pane's live input state
/// (`pty.write_input`), not a message the composer is designed to buffer. No
/// measurement establishes that write is safe mid-turn, so the guard stays.
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

/// The guard's verdict on one raw registry read. Pure, so every arm of the
/// fail-closed matrix is a unit test.
///
/// An unattributable row is an agent whose PANE is unknown. `rerun_allowed`
/// reasons from absence ("no row for this pane means shell"), and that
/// inference is only sound over a lossless read. So a row-level malformation
/// earns the same verdict a document-level one already gets, for the same
/// reason: absent liveness must never read as idle.
fn classify_guard_registry(raw: &str, now: u64) -> Result<Vec<RegistryAgent>, &'static str> {
    match agents_view::derive_rows_counted(raw, now) {
        None => Err("agents registry malformed - target agent state unknown"),
        Some((_, unattributable)) if unattributable > 0 => Err(
            "agents registry carries a row with no readable pane binding - \
             target agent state unknown",
        ),
        Some((rows, _)) => Ok(rows),
    }
}

/// True only when the current session/pane join names a LIVE row that
/// explicitly declared the bus-only delivery policy. DND is presence, never
/// liveness, but an exited row is skipped (matching [`rerun_allowed`]): a hold
/// stamped on a reaped agent must not veto the shell or successor that
/// inherited the pane and can never lift it.
fn pane_is_dnd(agents: &[RegistryAgent], session: &str, pane: u64) -> bool {
    agents.iter().any(|a| {
        !a.exited
            && a.dnd
            && a.mux
                .as_ref()
                .is_some_and(|(s, p)| s == session && *p == pane)
    })
}

/// Whether a focused NON-viewer leaf may be taken over by `.`=here.
/// Pure over the three inputs so the reap gate (the safety-critical bit) is
/// unit-testable without a Core, mirroring [`rerun_allowed`]. Take-over is
/// allowed iff the leaf is the tab's ONLY pane, a plain shell (`cmd == None`,
/// not a `pane run` / agent pane), and a pristine idle shell (has drawn a
/// prompt but run nothing - see [`vt::Pane::is_pristine_idle_shell`]). Any other
/// shape - a split, an agent pane, or a shell that has run/started anything -
/// refuses, so `.` never kills live work.
fn idle_shell_takeover(leaf_count: usize, cmd: Option<&str>, pristine_idle: bool) -> bool {
    leaf_count == 1 && cmd.is_none() && pristine_idle
}

/// The last `n` non-empty lines of `text`, joined by `\n` - the mux-server twin
/// of the daemon `Region::BottomNonEmptyLines` extraction; byte-
/// identical to the daemon so a region fingerprint hashes the same.
fn bottom_non_empty_lines(text: &str, n: usize) -> String {
    let nonblank: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = nonblank.len().saturating_sub(n);
    nonblank[start..].join("\n")
}

/// What connected clients register with the core loop.
pub(crate) enum CoreMsg {
    /// (v78) A stats request; the Core owns the counter.
    ServerStats {
        reply: oneshot::Sender<ServerMsg>,
    },
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
    /// (v56, hover affordance) One link-span lookup for the requester's hover
    /// underline. Read-only and initiator-only: the core resolves the pane's
    /// link match and answers THIS client with coordinates alone, so it is not
    /// in the passive-observer mutation gate (a passive viewer already sees
    /// the pane's frames; which cells form a link adds nothing they lack).
    LinkHover {
        id: u64,
        pane: u64,
        row: u16,
        col: u16,
        seq: u64,
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
    /// (v12) In-scrollback search: open/step/clear a free-text find over
    /// the pane's server-side history. The scroll + highlight are shared (every
    /// co-viewer sees the jump); `id` is the initiator, who alone gets the
    /// `SearchResult` counter and any no-op/pane-not-found notice.
    Search {
        id: u64,
        pane: u64,
        op: SearchOp,
    },
    /// (v9) Answer a blocked prompt: re-verify the region fingerprint
    /// against the live grid, then inject the daemon-pinned `keystroke`. `id`
    /// is the requesting client (for the stale/busy/closed notice).
    PaneAnswer {
        id: u64,
        pane: u64,
        fingerprint: [u8; 32],
        region_lines: u16,
        keystroke: Vec<u8>,
    },
    /// (v11) "Grab work" (prefix+g): dispatch the next ready node into a
    /// new pane. `id` is the requesting client (for the outcome notice). The
    /// launch runs OFF the core loop in a detached task (it shells the door,
    /// `fno agents spawn`); the pane appears via the existing registry
    /// reader, and only the no-work / refusal / failure outcomes come back as
    /// `DispatchResult`.
    DispatchNext {
        id: u64,
        /// The requesting client's session-local active account, so
        /// prefix+g routes the spawn to it just like a targeted card click.
        account: Option<String>,
    },
    /// The off-loop dispatch task's outcome, routed back so the notice is sent
    /// from the core loop (which owns `clients`). `notice` phrases carry over
    /// from the retired porcelain (see `dispatch_launch::dispatch_notice`).
    DispatchResult {
        id: u64,
        notice: String,
    },
    /// (v83, ) One sideline launcher request from client `id`. The
    /// handler validates pre-birth, dedups by request id, and runs exactly
    /// one canonical spawn off-loop; progress returns as
    /// [`CoreMsg::AgentLaunchUpdate`]. Gated on passive observers like
    /// DispatchNext.
    AgentLaunch {
        id: u64,
        request: crate::proto::AgentLaunchRequest,
    },
    /// The off-loop launcher task's terminal update, routed back so it is
    /// sent from the core loop (which owns `clients`). Trusted origin (a
    /// server task, not a client), so it is NOT in the passive gate - the
    /// same shape as DispatchResult/PeekResult. `retry` bounds the
    /// redelivery when the target client's channel is momentarily full.
    AgentLaunchUpdate {
        id: u64,
        update: crate::proto::AgentLaunchUpdate,
        retry: u8,
    },
    /// (v29) The off-loop peek task's transcript, routed back so the
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
    /// A refreshed external-lifecycle record set from an off-loop
    /// external action (`claude stop|rm`) or the startup reconcile, routed back
    /// so the render snapshot update + layout push run on the core loop. `to`
    /// targets one client (an action outcome) or every client (`None`, the
    /// reconcile broadcast); `notices` are the bounded per-record messages.
    ExternalLifecycleSync {
        to: Option<u64>,
        records: Vec<crate::squad_store::ExternalLifecycle>,
        notices: Vec<String>,
    },
    /// The canonical re-entry plan for one gesture, resolved OFF the
    /// core loop (`fno-agents reentry-plan`), routed back so the pane spawn +
    /// placement run on the core loop as before. `Err` is the visible refusal:
    /// a timeout, malformed verdict, missing binary, `resolved != true`, or the
    /// resolver's own named evidence gap - the handler starts NO pane on it.
    ReentryPlanReady {
        id: u64,
        request: Box<ReentrySpawnRequest>,
        verdict: Result<ReentryVerdict, String>,
    },
    /// One non-claude row's resume argv, resolved OFF the core loop
    /// (`fno-agents resume-argv`), routed back so the pane spawn runs on the
    /// core loop as before. `Ok((argv, degraded))`: the argv to stage and
    /// whether the verb failed (the fallback render, so the operator learns
    /// the worker resumes without its writable-roots grant). `Err` is the
    /// visible refusal: a timeout, a missing binary, an unknown harness - the
    /// handler starts NO pane on it.
    ResumeArgvReady {
        id: u64,
        argv: Result<(Vec<String>, bool), String>,
        replay: Box<ResumeReplay>,
    },
    /// One revival gate answer (the `fno-agents spawn-gate` ask the
    /// resume gesture or the held-pane focus fired off the core loop).
    /// `Ok` stages an admission for the row and re-dispatches the same
    /// command; `Err` is the visible refusal - the verdict line plus the
    /// one-run CLI escape - and starts no pane.
    RevivalGateAnswered {
        id: u64,
        name: String,
        verdict: Result<(), String>,
        replay: Box<ResumeReplay>,
    },
    /// A batch's pre-resolved attach plans (restore's members or a
    /// picker recruit's selected ids, keyed by attach id), routed back so the
    /// existing loop re-enters on the core loop with the verdicts in hand.
    /// An `Err` value is that member's visible refusal: its row is kept and
    /// no pane starts for it.
    BatchPlansReady {
        id: u64,
        plans: HashMap<String, Result<ReentryVerdict, String>>,
        replay: Box<BatchReplay>,
    },
    /// `fno mux workspace restore`: collect the candidate members on
    /// the core loop, resolve claude re-entry plans OFF it, and re-enter
    /// through [`CoreMsg::WorkspaceRestoreApply`]. A dry run (or a run with
    /// no claude members) applies inline - plans are never resolved for a
    /// classification-only pass.
    WorkspaceRestore {
        dry_run: bool,
        harness: Option<String>,
        reply: ControlReply,
    },
    /// The bulk apply half of a workspace restore: the plans are in
    /// hand (keyed by member worker name, `Err` being that member's visible
    /// refusal), and the revival gate's probed headroom is in hand (`Err`
    /// refusing every member), so every gate runs on the core loop through
    /// [`Core::resume_one`].
    WorkspaceRestoreApply {
        dry_run: bool,
        harness: Option<String>,
        plans: HashMap<String, Result<ReentryVerdict, String>>,
        headroom: Result<revival_gate::ProbeHeadroom, String>,
        reply: ControlReply,
    },
    /// (v71) `ControlVerb::SquadReload`: re-read `squads.json` into
    /// `squad_members` on the core loop, so an external prune survives the
    /// next `persist_squad`.
    SquadReload {
        reply: ControlReply,
    },
    /// (v75) `ControlVerb::RetireSession`: tombstone one (harness,
    /// full session id) identity across every held squad and close only its
    /// attached panes. Idempotent; a repeat retires nothing.
    RetireSession {
        harness: String,
        session_id: String,
        reply: ControlReply,
    },
    Gone(u64),
    /// A pre-Attach `Query` (mux ls): reply with the whole `Info` message.
    Query(tokio::sync::oneshot::Sender<ServerMsg>),
    /// A pre-Attach `KillServer`: Bye clients, then the shared choke point
    /// captures before killing non-keeper children and exits 0.
    Kill,
    // -- v4 control verbs (one-shot: reply on the oneshot, then the
    // connection task closes). Snapshot reads and the spawn/kill mutations
    // reply inline on the core loop; `PaneWait` hands its reply to an
    // off-loop watcher so nothing blocking ever lands on the loop.
    PaneLs {
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    PaneRead {
        pane: u64,
        lines: Option<u16>,
        /// (v6) Select an OSC 133 command block instead of a plain read.
        block: Option<BlockSel>,
        /// Fresh registry snapshot used to label the read with the registry
        /// identity joined to this pane.
        agents: Option<Vec<RegistryAgent>>,
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
        worker: Option<String>,
        reply: ControlReply,
    },
    PaneSend {
        pane: u64,
        bytes: Vec<u8>,
        guarded: bool,
        expected_identity: Option<String>,
        /// Fresh registry snapshot for a guarded send, read off-loop in
        /// `handle_control`. `Err` carries the refusal reason: either the read
        /// failed or the registry carries a row whose pane cannot be read
        /// (guarded -> fail closed); an unguarded send leaves it unused.
        /// `Ok(rows)` is the idle authority the guard checks, `rows` empty =>
        /// no agents => proceed.
        agents: Result<Vec<RegistryAgent>, &'static str>,
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
        hand_off_to: Option<String>,
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
    // -- v41 layout script verbs (all snapshot reads / inline
    //    mutations, replying on the oneshot). --
    PaneSplit {
        pane: u64,
        direction: Dir,
        no_focus: bool,
        reply: ControlReply,
    },
    TabLs {
        squad: PaneTarget,
        reply: ControlReply,
    },
    TabCreate {
        squad: PaneTarget,
        name: Option<String>,
        reply: ControlReply,
    },
    TabRename {
        squad: PaneTarget,
        tab: TabSel,
        name: String,
        reply: ControlReply,
    },
    /// (v65) The `fno mux tab move` door onto the reorder trunk.
    TabReorder {
        squad: PaneTarget,
        tab: TabSel,
        to: TabSel,
        reply: ControlReply,
    },
    TabClose {
        squad: PaneTarget,
        tab: TabSel,
        force: bool,
        /// Fresh registry rows for the unforced occupancy guard. `None` is
        /// either an unreadable registry or the deliberate forced bypass.
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    LayoutGet {
        scope: LayoutScope,
        /// Whether the reply carries the per-pane worker join the
        /// human layout rendering needs. `false` keeps the machine JSON
        /// byte-shape unchanged, whatever the registry read says.
        workers: bool,
        /// Fresh registry rows for that join; `None` is a read
        /// failure and, with `workers: true`, its own refusal.
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    ///  The `fno mux rows` receipt: the row set exactly as the
    /// server last derived it, decorated with the paint verdicts. The
    /// registry rows ride in from the router's off-loop read (the LayoutGet
    /// pattern); `None` is a read failure, never zero rows.
    AgentRowsGet {
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    PaneWhere {
        fno_id: String,
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    PaneBreak {
        pane: u64,
        name: Option<String>,
        reply: ControlReply,
    },
    PaneFocus {
        pane: u64,
        reply: ControlReply,
    },
    /// `fno agents attach` with a live mux: reach `name` through
    /// portal `portal` (the TUI reach's twin; see
    /// [`ControlVerb::ThreadPane`]). `placement` is what a fresh
    /// open honors; the verb's `portal` index wins over any portal field
    /// inside it.
    ThreadPane {
        name: String,
        portal: u8,
        placement: PanePlacement,
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    /// (v72) Re-seat a live pane-hosted worker into a portal seat, keeping the
    /// PTY (see [`ControlVerb::ThreadReseat`]). The server moves the topology;
    /// the registry `mux` flip is the caller's half, on this receipt.
    ReseatPane {
        pane: u64,
        portal: Option<u8>,
        reply: ControlReply,
    },
    TabJoin {
        src_tab: TabSel,
        anchor_pane: u64,
        direction: Dir,
        reply: ControlReply,
    },
    LayoutApply {
        squad: PaneTarget,
        tab: TabSel,
        spec: LayoutSpec,
        focus: bool,
        reply: ControlReply,
    },
    /// (v44) Local anchored layout graft: realize `spec` at `anchor`.
    LayoutGraft {
        squad: PaneTarget,
        anchor: u64,
        spec: AnchoredLayoutSpec,
        focus: bool,
        reply: ControlReply,
    },
    /// (v51) Reverse location lookup: resolve a location selector to
    /// the tab and its occupants.
    TabWhere {
        squad: PaneTarget,
        sel: String,
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    },
    /// A fresh registry-derived agent row set from the off-loop reader task
    /// (4a-G2). Sent only when the set changed; the core stores it and
    /// re-pushes layouts (rects unchanged, so no frame re-emit). `branches`,
    /// `tails` and `ctx` are the reader's off-loop maps, joined into each
    /// row's subline/tail and each pane's `PaneMeta` at layout time; an
    /// absent reading degrades to an absent cell, never a fabrication.
    AgentRows {
        rows: Vec<RegistryAgent>,
        branches: HashMap<String, String>,
        tails: HashMap<String, String>,
        ctx: HashMap<String, String>,
        /// The reader's registry+roster read succeeded (parsed bytes,
        /// last-good, or a confirmed-vanished file; a present-but-unreadable
        /// file reads false). Gates the daemon-side registry-absence death
        /// rule, which must stay inert while the read state is unknown.
        read_ok: bool,
    },
    /// Tails (and ctx, v91) moved with no row change behind them:
    /// transcripts grow independently of the registry, so when only this
    /// pass moved, it pushes alone rather than forcing a row set through.
    AgentTails {
        tails: HashMap<String, String>,
        ctx: HashMap<String, String>,
    },
    /// (v48) A fresh name -> reachability-evidence map from the off-loop truth
    /// probe (`fno agents list --json`, one process for the whole fleet).
    /// Replaces the map wholesale; a failed probe sends nothing so the last
    /// good map stands until the next success. `seq` is the probe's launch
    /// order (review finding: a probe can outlive the next tick's probe under
    /// load, and without a sequence guard the LATE result would win and
    /// silently revert the map to a stale reading until the next success).
    AgentTruth {
        map: HashMap<String, TruthReading>,
        seq: u64,
    },
    /// A fresh board-ordered work-queue card set from the off-loop graph
    /// reader, claim-overlaid. Sent only when the set changed; the core
    /// stores it and re-pushes layouts so the sideline backlog lane tracks
    /// claims/closes. `holders` is the sweep's node-id -> claim-holder map,
    /// consumed at publish time for the `where_hint` of unroutable cards.
    BacklogCards {
        cards: Vec<BacklogCard>,
        /// The UNCAPPED per-lane card counts `cards` was cut from.
        lanes: Vec<(String, usize)>,
        /// These cards are last-known, not current: the graph read has
        /// been failing.
        stale: bool,
        holders: HashMap<String, String>,
        /// node id -> pr_number, from the same graph read as `cards`.
        prs: HashMap<String, u64>,
        /// node id -> driving session short id, from the same graph read.
        drivers: HashMap<String, String>,
    },
    /// The per-pane counter snapshot cadence fired: snapshot every live pane's
    /// monotonic totals and emit one event onto the machine-global events
    /// journal. Sent by a 30s interval task; a no-op with no live panes (an
    /// idle mux writes nothing).
    PaneStatsTick,
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
    /// any pane without an upstream message (read-only attach). Its
    /// `Resize` is ignored and it never spawns a squad.
    passive: bool,
    /// Where this client's left button last went DOWN, as
    /// `(pane, row, col)`. Opening a clicked URL needs it because the client
    /// hit-tests every mouse report independently (client.rs `hit_test`) and
    /// forwards each to whatever pane the pointer is over: a drag begun in pane
    /// A and released over pane B delivers the release to B, which has no
    /// selection of its own and would otherwise read as a plain click there.
    /// A release only opens a link when it completes an UNMOVED press in the
    /// SAME pane, which also rules out a within-pane drag that happened to
    /// select nothing. `None` before the first press (codex P2, PR 702).
    last_press: Option<(u64, u16, u16)>,
}

struct PaneEntry {
    pty: PtyShell,
    vt: vt::Pane,
    /// The pane's `FNO_NODE` provenance, parsed from the `env(1)`
    /// wrapper prefix in the pane-run argv at spawn. `None` for a shell pane or
    /// an ad-hoc `pane run` with no `FNO_NODE=` token. Surfaced to the client
    /// status row via `Layout::focus_node`.
    node: Option<String>,
    /// The pane's `FNO_AGENT_SELF` registered worker name, parsed once
    /// at spawn. `None` for a shell or ad-hoc pane. The top derived source for
    /// the tab/pane title (decision b) - distinct from `node` (the backlog node
    /// id) and `cmd` (what the pane is running): three facts, not conflation.
    name: Option<String>,
    /// The spawn cwd, captured once so the tab-label derivation never
    /// touches the filesystem on the render path. Empty when the spawn fell
    /// back to the server cwd.
    cwd: String,
    /// Basename of the spawned command ("claude", "htop"), parsed from the
    /// pane-run argv like [`node_from_argv`]. `None` for a shell pane.
    cmd: Option<String>,
    /// The pane's birth claude account (`FNO_ACCOUNT`), parsed once at
    /// spawn. `None` = the default account. Drives the sideline account glyph
    /// for a mux-spawned pane; a durable pane fact (survives reattach), never
    /// the registry schema (Locked Decision 5).
    account: Option<String>,
    /// The session id this pane's run argv resumes (parsed at spawn,
    /// see [`resume_target_from_argv`]). The row-to-pane join for an UNBOUND
    /// resume pane: the registry row needs no written `fno_id` for the
    /// disposition to see that its session is already running here.
    /// `None` for a shell pane or any non-resume run.
    resume_target: Option<String>,
    /// A restore placeholder created because a worker could not be resumed.
    /// This positive refusal marker is sweepable; it is not inferred from an
    /// absent registry row.
    refused_worker: Option<String>,
    /// The held portal row this placeholder seat stands in for
    /// (`FNO_PORTAL_HELD`), parsed once at spawn and re-parsed at keeper
    /// re-adoption. A placeholder is held even though adoption gives its
    /// bare shell `cmd: Some`; the portal doors read
    /// [`Core::portal_seat_is_viewer`], never `cmd` alone.
    portal_hold: Option<String>,
    /// True when this pane was adopted at a fresh id because the pane key its
    /// keeper socket carries could not be reused (zero, or already live). Set
    /// only at keeper re-adoption; a send to an unreconciled pane is refused
    /// rather than delivered to whatever the number now names.
    unreconciled: bool,
    /// True when the keeper could not host this pane and the inline pty
    /// fell back instead: the pane is LIVE but will die with the server.
    /// Set only at spawn (never at re-adoption, whose panes are keeper-hosted
    /// by construction); `kill-server` refuses while an unkept pane is live.
    unkept: bool,
    /// When this pane last produced PTY output, stamped on the drain
    /// path itself so a pane with no `pane wait` watcher still records activity
    /// (`note_pane_output` returns early with zero subscribers, which is why
    /// bare-pane rows used to read `last_activity_age_s: None`). Initialized at
    /// registration so a never-spoken pane has an honest birth time.
    last_output: Instant,
    /// This pane's monotonic counters. Same `Arc` as the registry row, so the
    /// core loop increments without touching the registry lock.
    stats: Arc<PaneCounters>,
    /// A repaint request due to fire after the resize dust settles.
    /// Set by the geometry pass when the grid changed; fired by the 1s core
    /// tick. Deferral is the point: an immediate re-signal lands in the same
    /// signal burst the resize itself raised and coalesces into it, so a
    /// renderer that repaints exactly once on the burst never sees it.
    nudge_due: Option<Instant>,
    /// The most recently REQUESTED pane size, set by the geometry pass
    /// whether or not `vt` has caught up yet. A keeper-hosted pane's resize
    /// is a round trip (`PtyShell::resize` queues a frame; `entry.vt.resize`
    /// applies only once the keeper's ack arrives via `PaneChunk::Resized`
    /// on the pane's own ordered output channel) - so this, not `vt.size()`,
    /// is the one source of "what size did we last ask for", used by the
    /// geometry pass itself (to dedupe a repeat request) and the repaint
    /// nudge (to never nudge back to a stale pre-resize size while the ack
    /// is still in flight).
    requested_size: (u16, u16),
}

mod argv_facts;

use argv_facts::*;

/// A tab's display label, from spawn-time facts only - no I/O, no
/// subprocess on the layout path (squad.rs's origin-freeze discipline).
/// Chain: explicit rename > registered name (`FNO_AGENT_SELF`) >
/// `FNO_NODE` provenance > spawn-cwd basename when it differs from the squad's
/// > command basename > the bare 1-based index (so a plain shell tab renders
/// unchanged). `pane` is the focused pane's `(name, node, cwd, cmd)`; `None`
/// (a reaped pane racing tree cleanup) falls through to the index - the
/// derivation never panics on a missing pane.
#[allow(clippy::type_complexity)]
fn tab_label(
    rename: Option<&str>,
    pane: Option<(Option<&str>, Option<&str>, &str, Option<&str>)>,
    squad_cwd: &str,
    i: usize,
) -> String {
    if let Some(name) = rename {
        return name.to_string();
    }
    if let Some((name, node, cwd, cmd)) = pane {
        // Every derived candidate is sanitized like a rename (codex peer
        // review): FNO_NODE values, dir names, and argv all admit control
        // bytes, and these strings land in chrome cells. A candidate that
        // sanitizes to empty (e.g. whitespace-only) falls through to the
        // next source instead of rendering a blank label.
        if let Some(name) = name {
            let clean = sanitize_tab_name(name);
            if !clean.is_empty() {
                return clean;
            }
        }
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

/// The label chain and the pure `PaneMeta` builder live in [`pane_meta`]
/// (file-budget ratchet); re-imported so callers and tests resolve.
use pane_meta::pane_label;

mod pane_meta;

/// Is an executable `delta` on `path`? Takes the PATH value rather than reading
/// the environment so a test can probe a scratch dir without mutating
/// process-wide state that its parallel siblings share.
///
/// The execute bit is the point: a non-executable `delta` (a half-finished
/// install, a data file of that name) would be selected as the renderer and
/// then fail to exec, losing the diff into a dead pipe.
fn delta_in_path(path: Option<&std::ffi::OsStr>) -> bool {
    path.is_some_and(|p| std::env::split_paths(p).any(|d| is_executable_file(&d.join("delta"))))
}

fn is_executable_file(f: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Follows symlinks, so a dangling link is not mistaken for a binary.
    std::fs::metadata(f).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The renderer chain for a diff pane: delta when installed, git's own
/// color output through `less` otherwise. Resolved at each open, never cached,
/// so installing or removing delta takes effect on the next toggle.
///
/// `--paging=always` is load-bearing: without it delta dumps and exits on a
/// short diff, which reads as a broken pane rather than a small diff.
fn diff_pager() -> &'static str {
    if delta_in_path(std::env::var_os("PATH").as_deref()) {
        "delta --paging=always"
    } else {
        "less -R"
    }
}

#[cfg(test)]
thread_local! {
    /// Test override for the diff pane's shell program (see [`diff_argv`]):
    /// points a test at a nonexistent binary so the spawn-failure path runs.
    static DIFF_SHELL: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_diff_shell(program: &str) {
    DIFF_SHELL.with(|p| *p.borrow_mut() = Some(program.to_string()));
}

/// The diff pane's argv: the assembled script handed to a shell. The
/// script is built per call, so the renderer chain is re-probed at each open.
fn diff_argv() -> Vec<String> {
    #[cfg(not(test))]
    let sh = "sh".to_string();
    #[cfg(test)]
    let sh = DIFF_SHELL
        .with(|p| p.borrow().clone())
        .unwrap_or_else(|| "sh".to_string());
    vec![sh, "-c".to_string(), diff_script(diff_pager())]
}

/// The diff pane's shell script, rendering the working diff into `pager`. Pure
/// so the behavioral tests can run it for real with `cat`.
///
/// Every branch prints something. A pane that exits with no output is the
/// silent failure this feature is most exposed to: an empty diff, a repo with
/// no commits yet, and a non-git cwd would all produce one, and the operator
/// reads the flash-and-exit as "the feature is broken" rather than "there is
/// nothing to show". So the clean case states itself, an unborn HEAD diffs
/// against the empty tree, and a non-repo lets git's own error text through.
/// Untracked files are invisible to `git diff`, hence the header count.
fn diff_script(pager: &str) -> String {
    format!(
        r#"{{
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git rev-parse --is-inside-work-tree 2>&1
else
  rev=HEAD
  label=HEAD
  if ! git rev-parse --verify -q HEAD >/dev/null 2>&1; then
    rev=$(git hash-object -t tree /dev/null)
    label="the empty tree (no commits yet)"
  fi
  u=$(git ls-files --others --exclude-standard | wc -l | tr -d ' ')
  if [ "$u" -gt 0 ]; then
    echo "$u untracked file(s) not shown"
  fi
  if git diff --quiet "$rev" 2>/dev/null; then
    echo "no changes vs $label"
  else
    git -c color.ui=always diff "$rev"
  fi
fi
}} | {pager}"#
    )
}

/// Sanitize a wire-supplied name: strip control characters (they would corrupt
/// chrome cells), trim, cap at `cap` chars. The cap lives HERE and not only in
/// the overlay: `Command` is a wire surface, and the TUI is not the only
/// client. Empty-after-sanitize means "clear".
fn sanitize_name(raw: &str, cap: usize) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().chars().take(cap).collect()
}

/// Tab-name sanitize, capped at [`MAX_TAB_NAME`].
fn sanitize_tab_name(raw: &str) -> String {
    sanitize_name(raw, MAX_TAB_NAME)
}

/// The layout script verbs' tab-name boundary: sanitize control bytes
/// and cap length exactly like `Command::RenameTab`, then drop an empty result
/// to `None` (a blank name clears / stays unnamed). Applied by `tab_create`,
/// `tab_rename`, and `pane_break` so a scripted name can never emit terminal
/// escapes through `tab ls` or bypass the storage cap.
fn clean_tab_name(raw: Option<String>) -> Option<String> {
    let cleaned = sanitize_tab_name(raw?.as_str());
    (!cleaned.is_empty()).then_some(cleaned)
}

/// The location selector grammar for [`ControlVerb::TabWhere`]: the
/// identifier an operator can read off the screen, in explicit or bare form.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocSel {
    Ordinal(usize),
    Id(TabId),
    Name(String),
    /// A bare number: an ordinal or a stable id. The resolver refuses when
    /// the two readings name different live tabs (Locked Decision 5).
    Bare(u64),
}

fn parse_loc_sel(s: &str) -> Result<LocSel, String> {
    if let Some(n) = s.strip_prefix("ordinal:") {
        return n
            .parse::<usize>()
            .map(LocSel::Ordinal)
            .map_err(|_| format!("ordinal: needs a number, got {n:?}"));
    }
    if let Some(n) = s.strip_prefix("id:") {
        return n
            .parse::<u64>()
            .map(LocSel::Id)
            .map_err(|_| format!("id: needs a number, got {n:?}"));
    }
    if let Some(n) = s.strip_prefix("name:") {
        return Ok(LocSel::Name(n.to_string()));
    }
    match s.parse::<u64>() {
        Ok(n) => Ok(LocSel::Bare(n)),
        Err(_) => Ok(LocSel::Name(s.to_string())),
    }
}

/// Whether an event ended the session.
#[derive(PartialEq)]
enum Flow {
    Continue,
    Shutdown,
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
    match crate::pty::raise_fd_limit() {
        Ok(Some((before, after))) => {
            eprintln!("fno mux: open-file limit raised from {before} to {after}");
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!(
                "fno mux: warn: could not raise the open-file limit: {e}; panes will cap early"
            );
        }
    }
    // Stamp this server's wire version next to its socket so `fno mux
    // ls` can flag a below-floor server after a binary upgrade. Best-effort: a
    // write failure only means `ls` reads no version and treats the server as
    // stale (conservative - a spurious restart, never a missed skew), so it must
    // never abort the server.
    if let Err(e) = std::fs::write(
        crate::proto::version_sidecar_path(&socket),
        crate::proto::PROTO_VERSION.to_string(),
    ) {
        eprintln!("fno mux: warn: could not write version sidecar: {e}");
    }

    // Stamp the server's own pid beside the socket so kill-server
    // can signal a wedged holder without an accepted connection. Format lives
    // in proto's write/read pair; best-effort like `.ver` above.
    if let Err(e) = crate::proto::write_pid_sidecar(&socket) {
        eprintln!("fno mux: warn: could not write pid sidecar: {e}");
    }

    // Install signal ownership before constructing the Tokio runtime. Every
    // runtime thread inherits the blocked mask; only the dedicated waiter can
    // consume SIGTERM/SIGINT.
    let (signal_tx, signal_rx) = mpsc::channel(1);
    let shutdown_complete = Arc::new(AtomicBool::new(false));
    let pane_children =
        match install_signal_reaper(&socket, signal_tx, Arc::clone(&shutdown_complete)) {
            Ok(roster) => roster,
            Err(e) => {
                eprintln!("fno mux: cannot install emergency signal reaper: {e}");
                return 1;
            }
        };

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
    runtime.block_on(serve(
        listener,
        &socket,
        session_name,
        pane_children,
        signal_rx,
        shutdown_complete,
    ))
}

// ---------------------------------------------------------------------------
// Core state + mutations (all on the core loop)
// ---------------------------------------------------------------------------

/// A queued US8 template-tab restore. Re-applied once every fno binding
/// resolves, or after [`MAX_RESTORE_ATTEMPTS`] AgentRows ticks (then a shell for
/// each still-unresolved slot). `fallback_tid` is the zero-live-member
/// placeholder tab to remove once real template tabs land.
struct PendingRestore {
    sid: u64,
    specs: Vec<crate::squad_store::StoredTabSpec>,
    fallback_tid: Option<TabId>,
    attempts: u32,
}

/// AgentRows ticks a pending template restore waits for its bindings before it
/// applies anyway with shells for the unresolved. The reader ticks ~1/s, so this
/// is a generous few-second grace for restored sessions to register.
const MAX_RESTORE_ATTEMPTS: u32 = 30;

pub(crate) struct Core {
    session: Session,
    panes: HashMap<u64, PaneEntry>,
    /// Per-pane output signal for off-loop `PaneWait` watchers. One entry per
    /// live pane, created with the pane, dropped (flipped `exited`) when it is
    /// reaped. Kept in lockstep with `panes` via [`Core::register_pane`] /
    /// [`Core::reap_pane`].
    pane_watch: HashMap<u64, watch::Sender<WaitTick>>,
    /// Live per-pane counters, shared with the writer tasks for the
    /// `frames_emitted` leg. Kept in lockstep with `panes` in
    /// [`Core::register_pane`] / [`Core::reap_pane`], same as `pane_watch`.
    pane_stats: PaneStats,
    /// Minimal live child roster shared with the off-loop signal waiter.
    pane_children: PaneChildRoster,
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
    out_tx: mpsc::Sender<(u64, PaneChunk)>,
    exit_tx: mpsc::Sender<u64>,
    /// A clone of the core channel so an off-loop task (the prefix+g dispatch
    /// shell-out) can route its outcome back as a `CoreMsg::DispatchResult`
    ///, the same off-loop-work-feeds-the-loop shape as the registry
    /// reader.
    self_tx: mpsc::Sender<CoreMsg>,
    /// Latest registry-derived agent rows (4a-G2), stored raw; the pane-exit
    /// fact and squad assignment are joined at layout time, where the live
    /// pane set and the squad catalog live.
    agents: Vec<RegistryAgent>,
    /// The last `AgentRows` reader's registry+roster read succeeded.
    /// Starts false (no read yet = unknown), so the registry-absence death
    /// rule stays inert until a successful read proves it may fire.
    agents_read_ok: bool,
    /// The spawn journal as of the last row change. Refreshed only
    /// when `AgentRows` publishes (row changes are rare; journal appends ride
    /// them), never on the layout path: `dead_sweep_count` feeds every
    /// layout push, and a per-push journal scan would read the whole file
    /// every second.
    journal: crate::spawn_journal::JournalCache,
    /// (v83, ) The sideline launcher's attempt memory: one request id
    /// = one spawn attempt, with bounded replay for duplicate submissions
    /// and reopened popups.
    launch_desk: agent_launch::LaunchDesk,
    /// (US4) Latest cwd -> git-branch map from the off-loop reader,
    /// joined into each agent row's `subline` at layout time. A cwd absent from
    /// the map has no resolvable branch (non-git dir, unreadable HEAD); the
    /// subline then degrades to the cwd tail. Display-only, so staleness across
    /// a git checkout is cosmetic.
    branch_by_cwd: HashMap<String, String>,
    /// Latest session-uuid -> most-recent-assistant-line map from the
    /// off-loop reader, joined into each agent row's `tail` at layout time for
    /// the extended sideline table. A uuid absent from the map has no readable
    /// transcript or no prose in its tail; the cell renders empty. Display-only,
    /// so a stale line between reader ticks is cosmetic.
    tail_by_session: HashMap<String, String>,
    /// (v91) The context reading per transcript key, beside `tails`.
    ctx_by_session: HashMap<String, String>,
    /// (v48) Latest reachability-evidence map from the off-loop truth probe,
    /// joined into each agent row's `basis` / `last_activity_age_s` at layout
    /// time. The key is the row's full harness session id when it
    /// has one, the label otherwise (legacy rows): identity first, the
    /// demoted label as the alias fallback. Kept-whole on a failed probe: the
    /// alternative (clearing on failure) would flap every row to "no reading"
    /// on one miss. A key absent from the map has no probe answer, which the
    /// client reads as absence, never as urgency.
    truth_by_name: HashMap<String, TruthReading>,
    /// (v48) The `seq` of the last-applied `CoreMsg::AgentTruth`. Probes run
    /// concurrently off-loop and can complete out of launch order under load;
    /// a message whose `seq` is not newer is dropped rather than allowed to
    /// overwrite a fresher map with a stale one.
    truth_seq: u64,
    /// Latest board-ordered work-queue cards, from the off-loop graph
    /// reader; packed into every `Layout` for the sideline backlog lane.
    backlog: Vec<BacklogCard>,
    /// The UNCAPPED per-lane card counts `backlog` was cut from, so the
    /// sideline's `+N more` and the kanban's lane headers state true numbers.
    backlog_lanes: Vec<(String, usize)>,
    /// Whether `backlog` is last-known rather than current.
    backlog_stale: bool,
    /// Claim holder per in-flight node id, from the reader's sweep;
    /// joined at publish time into card routes / `where_hint`.
    backlog_holders: HashMap<String, String>,
    /// node id -> pr_number, from the off-loop graph reader; joined at
    /// layout time (holder name -> node -> pr) into `AgentRow.pr` for the peek
    /// header's `PR #N` label.
    backlog_pr: HashMap<String, u64>,
    /// node id -> driving session short id; joined at layout time into
    /// `AgentRow.pr_session_short` (the PR row's attach handle).
    backlog_driver: HashMap<String, String>,
    /// Panes spawned claim-ELIGIBLE (`pane run --claim`, agent panes). A
    /// general pane never appears here and never consults a claim (Locked 5).
    claim_eligible: HashSet<u64>,
    /// Held writer claims: pane -> holder pid. In-memory lookup + a
    /// `kill(pid, 0)` liveness probe (one syscall, never a subprocess); a
    /// dead holder releases lazily on the next contested keystroke (AC3-FR).
    claims: HashMap<u64, u32>,
    /// Per-pane last `human_touch(inject)` emit time: at most one emit per
    /// pane per [`TOUCH_COALESCE_WINDOW`], so a typing burst is one steering
    /// action. Purged with the pane in [`Core::reap_pane`].
    touch_last_emit: HashMap<u64, Instant>,
    /// Per-pane wheel-passthrough rate gate: bounds how many wheel
    /// ticks per window reach a mouse-owning pane PTY; purged with the pane
    /// in [`Core::reap_pane`], the `touch_last_emit` pattern.
    wheel_gate: HashMap<u64, WheelGateState>,
    /// Failed `human_touch` emits (AC4-ERR): counted, never raised to the
    /// steering path; read by the scoreboard stats answer (v78).
    touch_emit_failures: Arc<AtomicU64>,
    /// (v78) Server boot instant: the stats answer measurement window.
    started_at: String,
    /// Failed per-pane counter emits: same discipline as touch_emit_failures.
    pane_stats_emit_failures: Arc<AtomicU64>,
    /// Attached-client count for the periodic readers, published
    /// from choke points only: `clients` mutates in six places and per-site
    /// stores drift. A `watch`: the readers need the `changed()` edge.
    client_count: watch::Sender<usize>,
    /// Pane ids the operator has focused while badged `Done`.
    /// Inserted by an actual focus action (`Command::FocusPane`, via
    /// [`Core::mark_seen_if_done`]) on a `Done` pane; evicted level-triggered
    /// every layout pass the instant a pane's badge leaves `Done`. Reattach-
    /// durable (`Core` survives detach/reattach) but not server-restart (a
    /// cold-scrape non-goal, Locked Decision 7). Orphan ids from reaped
    /// panes are inert (never re-matched); no GC.
    seen: HashSet<u64>,
    /// Live attach panes: `attach_id -> pane`. Lifetime = pane
    /// lifetime, never persisted (server death kills panes; the bg agent
    /// re-surfaces watch-only next session). Lets `AttachAgent` reconcile
    /// row-to-tab identity: a second attach for a mapped id focuses the
    /// existing pane instead of minting a duplicate tab, and `agent_rows()`
    /// presents the mapped watch-only row pane-hosted. Swept eagerly on pane
    /// teardown; `agent_rows()` also checks `panes` liveness lazily so a stale
    /// entry can never present a dead pane.
    attached: HashMap<String, u64>,
    /// Live resumed worker panes: `registry name -> pane`. The
    /// worker twin of [`Self::attached`]: a resume spawn cannot go through
    /// the porcelain (`fno agents spawn --resume` is claude-bg-only), so the
    /// server binds the row to its new pane HERE - a second Resume for a
    /// mapped name focuses the existing pane instead of minting a second
    /// session on the same rollout, and `agent_rows()` presents the row
    /// pane-hosted while it lives. Lifetime = pane lifetime, swept on reap,
    /// never persisted.
    worker_pane: HashMap<String, Vec<u64>>,
    /// Full `(harness, harness session id)` -> resumed pane. This is the durable
    /// join when registry cleanup rewrites the display name after process death.
    worker_session_pane: HashMap<(String, String), u64>,
    /// Restart placeholders keyed by pane. Focusing one consumes the marker
    /// and resumes the persisted harness session into the same tree leaf.
    held_workers: HashMap<u64, HeldWorker>,
    /// Live worker panes intentionally removed from every tab tree. The
    /// keeper owns the PTY for worker panes, so this map is a placement marker,
    /// not the child-lifetime owner.
    detached_panes: HashMap<u64, DetachedPane>,
    /// The one live diff pane, as `(source cwd, pane id)`. One at a
    /// time by construction, so the "at most one pane per source" invariant
    /// needs no per-source map and no GC: a stale id (the pane was closed by
    /// any other path) reads as closed, and the next toggle opens fresh.
    ///
    /// Scope is the whole session, not a view: opening a diff on one tab
    /// closes one open on another. Deliberate for v1 - a diff pane is a
    /// glance, and one operator wanting two at once is the case to hear about
    /// before building per-view state for it.
    diff_pane: Option<(String, u64)>,
    /// The open PORTALS: the panes dedicated to
    /// thread-substrate rows, keyed by the operator-facing index. A portal is
    /// the thing you go through to reach a live harness thread, so several
    /// threads can each hold one and the existing Join actions tile them side
    /// by side.
    ///
    /// This is deliberately NOT the singleton contract `diff_pane` above
    /// keeps. A diff is a glance and one at a time is its design; a portal is
    /// a window onto a running thread and the cap of one was the defect
    /// (lifted it). Substrate semantics are untouched either way: a
    /// thread still hosts no pane until one is created.
    ///
    /// Per index, the mechanics are exactly the old single slot's. Re-reaching
    /// the SAME row is a no-op focus ("show me"), never a close - closing is
    /// the ordinary close-pane gesture. A repoint to a different row reuses
    /// the open-here mechanic (spawn-first, `tree::replace_leaf`, reap-last)
    /// so the geometry never moves, and it touches only its own index. NEVER
    /// persisted and NEVER rebuilt by restore: a pane binds a session to
    /// geometry, a thread binds a session to a row, and persisting a portal
    /// would re-bind a thread to a rectangle across a restart. `BTreeMap`
    /// rather than `HashMap` so iteration is index-ordered and the sideline's
    /// portal column cannot reshuffle between frames.
    ///
    /// After a viewer's child dies, the seat keeps the tab alive as
    /// an idle-shell stand-in and the entry names the STAND-IN, so the next
    /// reach repoints the seat in the same tab instead of minting a second
    /// portal tab. That swap now fires only for the LAST open portal: it
    /// exists so a dying viewer never deletes the only window onto the fleet,
    /// and with another portal open that premise is false. The seat pane is a
    /// live viewer iff `panes[seat].cmd` is `Some` (shells carry no argv
    /// provenance); the same-row focus arm requires it.
    portals: BTreeMap<u8, Portal>,
    /// One-shot latch for the discoverability notice: the first
    /// thread row to appear with no portal open tells the operator the reach
    /// gesture exists. A notice is not state - the latch only mutes
    /// repetition, it never gates behavior.
    portal_noticed: bool,
    /// Durable membership of each PERSISTED named squad: squad id ->
    /// its recruited members (attach-ids + tombstone bits). Populated only by
    /// `NewSquad`, `RecruitAgents`, and restore; presence here is what marks a
    /// squad persistent (an attach-born origin squad is absent and never
    /// written). Written through to `~/.fno/squads.json` on every membership
    /// mutation. Keyed by session-scoped id, so a removed squad's entry is
    /// inert (ids never reused; no GC - ponytail: a dead-sid leak is one small
    /// map entry per closed workspace per session, bounded by session length).
    squad_members: HashMap<u64, Vec<crate::squad_store::StoredMember>>,
    /// The live layout spec of each template-managed tab: tab id ->
    /// the spec last applied. A template tab is agent-managed by contract, so
    /// this is the authority for the reconcile diff and the source captured into
    /// the store on persist (US8 restore re-applies it). Keyed by session-scoped
    /// TabId; a closed tab's entry is inert (ids never reused, no GC needed).
    template_specs: HashMap<crate::tree::TabId, LayoutSpec>,
    /// Template tabs awaiting a re-apply after restore, once their fno
    /// bindings can resolve (the registry populates off-loop). Drained on every
    /// AgentRows tick; empty in steady state.
    pending_template_restores: Vec<PendingRestore>,
    /// Machine-global external-row lifecycle tombstones the sideline
    /// renders (stopped -> exited `x`-removable; failed/unknown/stopping/removing
    /// -> `!exited` with an in-flight reason). Loaded at restore, refreshed after
    /// every external action and the startup reconcile. The durable truth is
    /// `squads.json`'s `external_lifecycle`; this is the render snapshot.
    external_lifecycle: Vec<crate::squad_store::ExternalLifecycle>,
    /// One-shot notice latches: persistence degraded (a full disk never spams
    /// a bell per keystroke), and each stored identity two live squads share.
    persist_degraded_notified: bool,
    shared_identity_notified: HashSet<String>,
    /// First-attach restore fires once per server lifetime; this gates
    /// it so a second client attach does not re-materialize the persisted
    /// squads.
    restored: bool,
    /// True while startup restore waits for off-loop re-entry plans.
    restore_pending: bool,
    /// Per-squad store generations produced or restored by this server.
    store_generations: HashMap<String, u64>,
    /// Squads created before first attach. Their empty bootstrap persist must
    /// not overwrite an older squad waiting for restore.
    pre_restore_squads: HashSet<u64>,
    /// A topology mutation landed (`push_layout(true)`) whose tree
    /// capture has not been written yet. Set on every layout-changing pass,
    /// flushed by [`Core::flush_topology`] when the debounce window is past or
    /// the last client is leaving - a ratio drag emits continuously, and an
    /// undebounced per-event persist would contend the store's flock past its
    /// retry budget and silently drop writes (the exact gesture that generates
    /// the most events). Core-loop-owned like every other `Core` field.
    topology_dirty: bool,
    /// When the last topology flush ran, for the debounce window.
    last_topology_flush: Option<Instant>,
    /// The canonical re-entry verdict for the gesture the
    /// `ReentryPlanReady` continuation just re-dispatched. Consumed exactly
    /// once by the receiving arm (`take()` at its argv construction); empty in
    /// steady state. One-shot by construction: a second gesture arriving
    /// without a verdict resolves fresh.
    reentry_verdict: Option<ReentryVerdict>,
    /// The resolved resume argv for the non-claude gesture the
    /// `ResumeArgvReady` continuation just re-dispatched, same one-shot
    /// contract as `reentry_verdict`: staged by the ready-handler (or the
    /// bulk apply, which keeps its sync declared-form render), consumed
    /// exactly once by the receiving arm. Empty in steady state.
    staged_resume_argv: Option<Vec<String>>,
    /// The staged revival-gate admission for the worker whose `spawn-gate`
    /// ask just admitted: `(name, staged_at)`, consumed exactly once by
    /// [`Core::resume_worker_into`] and stale after 120s. Empty in steady
    /// state.
    revival_admission: Option<(String, std::time::Instant)>,
    /// A batch's pre-resolved attach plans, keyed by attach id:
    /// staged by the `BatchPlansReady` handler, drained per member by the
    /// consuming loop (restore or a picker recruit). Empty outside a batch
    /// (and in every legacy-path test: no entry means the reconstructed
    /// argv stands).
    batch_plans: HashMap<String, Result<ReentryVerdict, String>>,
    /// One parked control-door portal reach: a claude Drive row
    /// whose re-entry plan is resolving off-loop. The observer client stays
    /// registered and the CLI's reply is held until the ReentryPlanReady
    /// replay lands (or refuses) the reach; `finish_pending_thread_reply`
    /// then answers it. `None` in steady state and on every TUI gesture.
    pending_thread_reply: Option<portal_reach::PendingThreadReply>,
    /// Keeper-hosted panes adopted at startup, awaiting their stored-member
    /// binding at restore. Empty once every adoptee is placed.
    keeper_adopted: Vec<AdoptedKeeper>,
    /// Shell-integration rc dirs of KEEPER shells, `pane id -> dir`. A keeper
    /// child outlives this server, so the dir must too: never a dropping
    /// `pty::ShellRc`, which would remove the dir a live shell still
    /// references at server exit. Removed on pane close; re-owned by
    /// re-adoption.
    shell_rc_dirs: HashMap<u64, std::path::PathBuf>,
    /// Per-portal fill guards recorded at capture: `portal index -> FULL
    /// harness session id`. A held seat whose key later resolves to a
    /// different session id is a DIFFERENT thread under a familiar label;
    /// the fill refuses and names both. Cleared when the seat fills live
    /// (ownership is then the reach's, not the store's).
    portal_session_guards: BTreeMap<u8, String>,
}

/// Wheel-passthrough rate gate: forward at most [`WHEEL_GATE_BUDGET`]
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

/// Capture-side pane -> slot naming. Slot names are decided at
/// CAPTURE, never at restore, so two snapshots of one session agree on which
/// pane is which: a pane with an fno id names its slot that id and binds
/// `Fno(id)` (restore re-attaches it); a pane without one names itself
/// `p<ordinal>` and binds `Shell`. A duplicate attach id (the mirroring-ready
/// case `PaneLocation` documents) gets a `#2` suffix rather than colliding.
struct SlotCapture<'a> {
    pane_owner: &'a HashMap<u64, &'a str>,
    /// Each pane's live cwd, read once before the tab loop.
    pane_cwd: &'a HashMap<u64, String>,
    /// Every live portal seat -> (index, row_key), read once before
    /// the tab loop. A seated leaf names its slot after the portal instead of
    /// an ordinal, so the capture keeps what restore needs to hold it again.
    portal_seats: &'a HashMap<u64, (u8, String)>,
    /// The live portals map and the registry snapshot, read once: a seated
    /// leaf's `PortalSlot` carries the row's harness and FULL session id (the
    /// fill guard) resolved through the same join the reach uses.
    portals: &'a BTreeMap<u8, Portal>,
    agents: &'a [crate::agents_view::RegistryAgent],
    slots: Vec<LayoutSlot>,
    by_pane: HashMap<u64, String>,
    ordinal: usize,
}

impl<'a> SlotCapture<'a> {
    fn new(
        pane_owner: &'a HashMap<u64, &str>,
        pane_cwd: &'a HashMap<u64, String>,
        portal_seats: &'a HashMap<u64, (u8, String)>,
        portals: &'a BTreeMap<u8, Portal>,
        agents: &'a [crate::agents_view::RegistryAgent],
    ) -> Self {
        SlotCapture {
            pane_owner,
            pane_cwd,
            portal_seats,
            portals,
            agents,
            slots: Vec::new(),
            by_pane: HashMap::new(),
            ordinal: 0,
        }
    }

    /// The live tree -> the persisted spec. Weights are renormalized on the
    /// way out rather than trusted: `tree::check_invariants` requires branch
    /// ratios summing to 1.0, but a stored document is untrusted input and
    /// geometry divides by the sum.
    fn node_to_spec(&mut self, node: &Node) -> LayoutTreeSpec {
        match node {
            Node::Leaf(p) => LayoutTreeSpec::Slot(self.name_leaf(*p)),
            Node::Branch { axis, children } => {
                let weights: Vec<f32> = children.iter().map(|(w, _)| w.max(0.0)).collect();
                let sum: f32 = weights.iter().sum();
                let even = 1.0 / children.len() as f32;
                let children = children
                    .iter()
                    .zip(weights)
                    .map(|((_, n), w)| LayoutTreeChild {
                        weight: if sum > 0.0 { w / sum } else { even },
                        tree: self.node_to_spec(n),
                    })
                    .collect();
                LayoutTreeSpec::Split {
                    axis: *axis,
                    children,
                }
            }
        }
    }

    fn name_leaf(&mut self, pane: u64) -> String {
        // Order is portal, owner, ordinal. A portal seat that is
        // also an attach pane (a LIVE viewer is: the reach inserts the
        // mapping) captures as the portal slot, never as `Fno(attach_id)` -
        // an attach binding would re-bind the thread to the rectangle at
        // restore, the exact thing never-persist rule exists for.
        // The slot pair is the durable record; the viewer process is not.
        let base = match self.portal_seats.get(&pane) {
            Some((index, _)) => format!("portal{index}"),
            None => match self.pane_owner.get(&pane) {
                Some(id) => id.to_string(),
                None => {
                    self.ordinal += 1;
                    format!("p{}", self.ordinal)
                }
            },
        };
        let mut name = base.clone();
        let mut n = 2;
        while self.slots.iter().any(|s| s.name == name) {
            name = format!("{base}#{n}");
            n += 1;
        }
        let binding = if self.portal_seats.contains_key(&pane) {
            LayoutBinding::Shell
        } else {
            match self.pane_owner.get(&pane) {
                Some(id) => LayoutBinding::Fno(id.to_string()),
                None => LayoutBinding::Shell,
            }
        };
        let portal = self.portal_seats.get(&pane).map(|(index, row)| {
            // The row facts the fill guard reads back after a restart:
            // harness + FULL session id of the row the seat showed at
            // capture, resolved through the same agents snapshot the reach
            // itself used. A row that no longer resolves captures as None
            // and fills unguarded.
            let row_facts = crate::thread_viewer::row_for_pane(self.portals, pane, self.agents)
                .map(|agent| (agent.harness.clone(), agent.harness_session_id.clone()))
                .unwrap_or((None, None));
            PortalSlot {
                index: *index,
                row: row.clone(),
                harness: row_facts.0,
                session_id: row_facts.1,
            }
        });
        self.slots.push(LayoutSlot {
            name: name.clone(),
            binding,
            cwd: self.pane_cwd.get(&pane).cloned(),
            portal,
            // The restart join: the leaf remembers the pane id that lived
            // here, so restore can bind its re-adopted keeper twin.
            pane_id: Some(pane),
        });
        self.by_pane.insert(pane, name.clone());
        name
    }

    /// The slot name capture gave `pane` (for the persisted focus marker).
    fn slot_of(&self, pane: u64) -> Option<String> {
        self.by_pane.get(&pane).cloned()
    }
}

/// Whether a stored layout slot binds a done member. Bindings name
/// workers with the exact string `worker_binding_key` builds, so the slot
/// filter and the member-loop gate can never disagree about who is done.
/// A portal slot names done by its ROW: the same string set the
/// forgotten-tombstone arm fills with attach ids, so a portal onto a done
/// claude row collapses with the done leaves instead of restoring a held
/// ghost.
fn slot_names_done(slot: &LayoutSlot, done: &HashSet<String>) -> bool {
    matches!(&slot.binding, LayoutBinding::Fno(id) if done.contains(id))
        || slot.portal.as_ref().is_some_and(|p| done.contains(&p.row))
}

/// The stored tree minus the leaves of DONE slots (names only, the
/// same strings `LayoutTreeSpec::Slot` carries): done work earns no pane, so
/// its leaf collapses instead of shell-substituting. One-child splits
/// collapse; `None` means the tree is gone and the tab is skipped.
fn prune_done_slots(tree: &LayoutTreeSpec, done: &HashSet<String>) -> Option<LayoutTreeSpec> {
    match tree {
        LayoutTreeSpec::Slot(name) => {
            (!done.contains(name)).then(|| LayoutTreeSpec::Slot(name.clone()))
        }
        LayoutTreeSpec::Split { axis, children } => {
            let kept: Vec<LayoutTreeChild> = children
                .iter()
                .filter_map(|child| {
                    prune_done_slots(&child.tree, done).map(|tree| LayoutTreeChild {
                        weight: child.weight,
                        tree,
                    })
                })
                .collect();
            match kept.len() {
                0 => None,
                1 => Some(kept.into_iter().next().expect("checked").tree),
                _ => Some(LayoutTreeSpec::Split {
                    axis: *axis,
                    children: kept,
                }),
            }
        }
    }
}

/// The persisted spec -> a live tree (restore). `resolve` maps a slot
/// name to a pane id (`None` = the slot's pane is unavailable and the caller
/// substitutes). `None` from THIS function means the document is malformed (a
/// one-child split, or a dangling slot ref) and the tab must fall back, never
/// half-build - the same refusal posture `apply_spec` holds.
fn spec_to_node(tree: &LayoutTreeSpec, resolve: &dyn Fn(&str) -> Option<u64>) -> Option<Node> {
    match tree {
        LayoutTreeSpec::Slot(name) => resolve(name).map(Node::Leaf),
        LayoutTreeSpec::Split { axis, children } => {
            if children.len() < 2 {
                return None;
            }
            let children = children
                .iter()
                .map(|c| Some((c.weight, spec_to_node(&c.tree, resolve)?)))
                .collect::<Option<Vec<_>>>()?;
            Some(Node::Branch {
                axis: *axis,
                children,
            })
        }
    }
}

/// The cwd to spawn a restored member's pane at (case 2/3), and the
/// stored path to name in a fallback notice when it no longer resolves. Pure
/// over an injected `is_dir` so the policy is unit-testable without touching
/// the filesystem, like `live_ids_from`. `stored.is_none()` (a pre-change
/// member) and a stored path that fails `is_dir` both fall back to `cwd0` -
/// the two-path notice (member wants -> where it actually landed) is the
/// caller's job, since only the caller knows the member's identity to name.
pub(crate) fn restore_member_cwd(
    stored: Option<&str>,
    cwd0: &str,
    is_dir: impl Fn(&str) -> bool,
) -> (String, Option<String>) {
    match stored {
        Some(path) if is_dir(path) => (path.to_string(), None),
        Some(path) => (cwd0.to_string(), Some(path.to_string())),
        None => (cwd0.to_string(), None),
    }
}

// The registry rows the restore verb classifies against: the reader and its
// test override live in `restore_gate`, next to the restore refusals.
use crate::restore_gate::restore_registry_rows;

// The restore gate's done set, overridable in tests (a unit test
// cannot populate the real graph). `None` falls through to the live
// `backlog_view::done_session_ids` read.
#[cfg(test)]
thread_local! {
    static RESTORE_DONE_SESSIONS: std::cell::RefCell<Option<HashSet<(String, String)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_done_sessions(done: HashSet<(String, String)>) {
    RESTORE_DONE_SESSIONS.with(|slot| *slot.borrow_mut() = Some(done));
}

#[cfg(test)]
struct DoneSessionsGuard;

#[cfg(test)]
impl Drop for DoneSessionsGuard {
    fn drop(&mut self) {
        RESTORE_DONE_SESSIONS.with(|slot| *slot.borrow_mut() = None);
    }
}

/// The stored member's own structural refusal, when the member
/// itself explains a failure better than the generic gesture notice: a
/// harness the capability table gives no form, or a missing session id.
/// Report-only - the gates in [`Core::resume_one`] already refused the
/// spawn; this names WHY in the member's own terms (AC5-ERR: named, never
/// silently dropped).
fn member_structural_refusal(member: &crate::squad_store::StoredMember) -> Option<String> {
    let harness = member.harness.as_deref()?;
    let session_id = member.harness_session_id.as_deref().unwrap_or("");
    if !Core::resume_form(harness) {
        return Some(no_resume_form_reason(harness, session_id));
    }
    if session_id.is_empty() {
        return Some("session id is missing".into());
    }
    None
}

/// The routed-codex refusal for one restore candidate now lives in
/// [`restore_route_gate::member_routed_codex_refusal`], beside the
/// refused-row constructor.
pub(crate) fn agent_harness_session_id(agent: &RegistryAgent) -> Option<&str> {
    agent
        .harness_session_id
        .as_deref()
        .or(agent.claude_session_uuid.as_deref())
}

/// The set of attach-ids live NOW, from the raw registry + roster contents
///. Pure so restore's liveness read is unit-testable without files or
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

/// The live attach-id set read synchronously from the registry + roster files
/// (the dead-set source for the mux squad prune verb). Free of `Server`
/// so the standalone CLI computes the same liveness restore does, isolated
/// rosters folded in: a worker live in an alt-account roster is live.
/// `None`-ish semantics are the caller's - this returns the set it could read;
/// an unreadable registry/roster simply contributes nothing (fail-safe: the
/// prune predicate then treats unprovable members as unknown and keeps them).
pub(crate) fn live_attach_ids_snapshot() -> HashSet<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let reg = std::fs::read_to_string(agents_view::registry_path()).ok();
    let roster = std::fs::read_to_string(agents_view::roster_path()).ok();
    let mut live = live_ids_from(reg.as_deref(), roster.as_deref(), now);
    // A reboot writes nothing to the registry, so a row can claim a
    // non-terminal status for a worker whose pid died with the machine.
    // Subtract the rows their own recorded pid POSITIVELY falsifies before
    // restore trusts the set: an unverified read here respawns `claude
    // attach` into sessions that no longer exist. Rows with no recorded pid
    // keep their status-field verdict (fail-safe, `row_falsified`).
    if let Some(raw) = reg.as_deref() {
        let stale = agents_view::stale_live_attach_ids(raw);
        live.retain(|id| !stale.contains(id));
    }
    for (_account, path) in agents_view::isolated_roster_paths() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for w in agents_view::parse_roster(&raw).into_iter().flatten() {
                live.insert(w.short_id);
            }
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
    !crate::proto::pid_confirmed_dead(pid as libc::pid_t)
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

/// How long one `fno config get` may take before it is killed and read as
/// absent. Bounded because the sync callers run on a startup path with nothing
/// downstream to rescue a wedged read.
const CONFIG_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Read one config key through `fno config get <key>`, bounded and fail-open:
/// any spawn error, non-zero exit, or overrun reads as `None` (absent), never
/// as a value. `fno config get` prints the bare value on stdout and its
/// provenance on stderr, so stdout is the value.
///
/// Callers are the CLIENT (at server spawn) and `mux doctor`. Never the server:
/// a subprocess on its startup path delayed shutdown past the SIGTERM grace and
/// perturbed multiclient frame ordering, so the server reads only the env the
/// client latched.
///
/// Capture stdout to a FILE, not a pipe. A pipe read blocks until EOF (every
/// write-end closed), so a descendant of `fno config get` that inherits stdout
/// and outlives the direct child would hang the read even after `try_wait`
/// reports the child gone - re-introducing the very freeze the bound exists to
/// prevent. A file read never blocks on EOF; the bounded try_wait/kill still
/// caps the child's own runtime.
pub(crate) fn config_get(key: &str) -> Option<String> {
    let dir = crate::proto::mux_dir();
    crate::proto::ensure_private_dir(&dir).ok()?;
    // 0700 per-user dir (never world-writable /tmp); a pid+key-unique name, so
    // no two processes and no two KEYS share a capture file. Callers read keys
    // one at a time, so the same key twice at once does not arise. Removed on
    // every return path.
    let safe_key: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let out_path = dir.join(format!("config-{}-{safe_key}.out", std::process::id()));
    let out_file = std::fs::File::create(&out_path).ok()?;
    let mut command = crate::process_admission::std_command(fno_bin());
    command
        .args(["config", "get", key])
        .stdin(std::process::Stdio::null())
        .stdout(out_file)
        .stderr(std::process::Stdio::null());
    let mut child = match crate::process_admission::std_spawn(&mut command) {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&out_path);
            return None;
        }
    };
    let deadline = std::time::Instant::now() + CONFIG_READ_TIMEOUT;
    let value = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break status
                    .success()
                    .then(|| std::fs::read_to_string(&out_path).ok())
                    .flatten();
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let _ = std::fs::remove_file(&out_path);
    value
}

/// Sanitize peek-overlay free-text mail: strip control chars, trim,
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

/// First non-empty line of `s` with control chars stripped, else
/// `fallback`. Subprocess stdout/stderr becomes an operator-visible notice, so
/// raw ANSI/C0 must never reach the status line (Domain Pitfall: route stderr
/// through the same strip the peek body uses).
pub(crate) fn first_line_or(s: &str, fallback: &str) -> String {
    s.lines()
        .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// (native since the store port) Drive one reorder verb into the ported
/// graph store. The `fno` shell-out is retired FOR THE WRITE: the server is a
/// store keeper client now ([`crate::store_client`]), so the write runs through
/// the same bounded-lock, version-checked, atomically-published pipeline
/// every other writer uses. The blocking socket work runs off the async core;
/// failure names what went wrong so the footer carries the cause, not just
/// that it did. The derived views (graph.md, board targets) are the keeper
/// render trigger's job: a landed native write bumps the store counter and
/// the trigger replays the views once, like every other write.
async fn run_backlog_verb(node: &str, verb: crate::proto::BacklogVerb) -> String {
    let label = verb.label();
    let graph = backlog_view::graph_path();
    let target = node.to_string();
    let outcome = tokio::task::spawn_blocking(move || match verb {
        crate::proto::BacklogVerb::RankTop => crate::store_client::rank_top(&graph, &target),
        crate::proto::BacklogVerb::Defer => {
            crate::store_client::defer(&graph, &target, crate::proto::DEFER_REASON)
        }
        crate::proto::BacklogVerb::EndMission => crate::store_client::end_mission(&graph, &target),
    })
    .await;
    match outcome {
        Ok(Ok(notice)) => notice,
        Ok(Err(e)) => format!("{label} {node}: {e}"),
        Err(_) => format!("{label} {node}: unavailable"),
    }
}

/// Shell `fno agents peek <name> -n 20` for the sideline peek overlay,
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
    let mut command = crate::process_admission::tokio_command(fno_bin());
    command
        .args(["agents", "peek", name, "-n", PEEK_LINES])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
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

/// Shell `claude <verb> <attach_id>` for an external lifecycle action:
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
    let mut cmd = crate::process_admission::tokio_command("claude");
    cmd.args([verb, attach_id]);
    // Route the lifecycle action at the row's own daemon: an isolated
    // account lives in its own CLAUDE_CONFIG_DIR, so a bare `claude stop|rm`
    // under the default dir would miss it or hit a colliding id (codex P1).
    if let Some(dir) = config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_status(&mut cmd);
    match tokio::time::timeout(CLAUDE_TIMEOUT, fut).await {
        Err(_) => (false, Some(format!("{verb} timed out"))),
        Ok(Err(_)) => (false, Some("claude unavailable".to_string())),
        Ok(Ok(status)) if status.success() => (true, None),
        Ok(Ok(_)) => (false, Some(format!("{verb} failed"))),
    }
}

/// Shell `claude agents --json --all` ONCE for the startup reconcile,
/// bounded + fail-open: parse the tracked-id liveness map, or `None` on missing
/// binary / non-zero exit / timeout / schema drift so the caller holds tracked
/// rows as `unknown` rather than deleting an id it could not observe (AC1-FR).
async fn run_claude_agents_all(
    tracked: &std::collections::HashSet<String>,
) -> Option<HashMap<String, crate::agents_view::ObservedExternal>> {
    const AGENTS_TIMEOUT: Duration = Duration::from_secs(10);
    let mut command = crate::process_admission::tokio_command("claude");
    command
        .args(["agents", "--json", "--all"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(AGENTS_TIMEOUT, fut).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    crate::agents_view::parse_claude_agents(&String::from_utf8_lossy(&output.stdout), tracked)
}

/// CI diagnostics: timestamped breadcrumbs for the e2e server log
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

/// Whether `node` (an id or slug) names a READY card in the server's backlog
/// snapshot (codex peer review). A targeted card dispatch must refuse a
/// blocked / in-flight / unknown node - the same nodes `prefix+g` would never
/// select - so a click cannot start work with unmet deps even if the client's
/// (staler) Layout still showed it ready. Pure so the gate is unit-testable
/// without touching the subprocess spawn.
fn card_ready_to_dispatch(backlog: &[BacklogCard], node: &str) -> bool {
    backlog
        .iter()
        .any(|c| (c.id == node || c.slug == node) && c.state == CardState::Ready)
}

/// Rewrite every `Leaf(from)` in `node` to `Leaf(to)`, leaving branches and
/// other leaves untouched. Used by the home-shell reclaim: the stored shell's
/// pane takes the leaf its slot recorded, the stand-in retires.
fn subtree_swap(node: &mut Node, from: u64, to: u64) {
    match node {
        Node::Leaf(pid) if *pid == from => *pid = to,
        Node::Branch { children, .. } => {
            for (_, child) in children {
                subtree_swap(child, from, to);
            }
        }
        _ => {}
    }
}

/// Whether `name` carries `node` as an exact token (plan Locked 6):
/// the id appears with no alphanumeric neighbor on either side, so
/// `tgt-` and `` match but `x-54f` inside `` (or ``
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowResumeDisposition {
    Resumable,
    NoPane(AgentNoPaneReason),
}

/// The per-member result of one resume attempt, reported by
/// [`Core::resume_one`]. The gesture folds it into notices + a view change;
/// the workspace-restore driver renders it one line per member. The refusal
/// reason strings are exactly the notices the gesture printed before the
/// extraction, so the two surfaces cannot disagree about why a row stayed
/// idle.
#[derive(Debug, Clone, PartialEq)]
enum ResumeOutcome {
    /// A pane now runs the session. Carries the pane id, its workspace and
    /// tab, and the vanished-cwd notice when the stored directory was gone.
    Resumed {
        pane: u64,
        squad: u64,
        tab: TabId,
        notice: Option<String>,
    },
    /// The session already had a live pane and it was focused, never
    /// respawned (AC6-ERR). Carries where the pane lives.
    Focused { pane: u64, squad: u64, tab: TabId },
    /// Refused with the same reason text the gesture would have noticed.
    Refused { reason: String },
    /// The claude re-entry plan fired off-loop and the gesture will replay.
    /// A no-op on the surface it came from; the restore driver never hits it
    /// because it stages every verdict before the apply loop.
    PlanPending,
    /// A dry run stopped exactly where the spawn would happen: nothing was
    /// started and nothing was refused.
    Planned,
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
            // (Locked Decision 4 / AC1-EDGE).
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
    pub(crate) fn spawn_pane(&mut self, rows: u16, cols: u16, cwd: &str) -> Result<u64, String> {
        let id = self.reserve_pane_id()?;
        let dir = Some(std::path::Path::new(cwd)).filter(|_| !cwd.is_empty());
        // Production shells are keeper-hosted like every other pane (the
        // survival contract); the shell candidate loop and the integration
        // prefix live in `spawn_pane_kept`. Unit fixtures keep the inline
        // pty: they spawn short-lived shells that can exit before a keeper
        // answers Identify.
        #[cfg(not(test))]
        if let Some(pid) = self.spawn_pane_kept(rows, cols, cwd, id, dir)? {
            return Ok(pid);
        }
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
        self.register_pane(
            id,
            pty,
            rows,
            cols,
            None,
            None,
            cwd.to_string(),
            None,
            None,
            None,
            None,
            None,
        )?;
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
        // Before admission: the ceiling probe must own the wall. At one free
        // descriptor the admission census EMFILEs first and the operator
        // would read a measurement failure where the truth is the ceiling.
        if let Some(err) = crate::pty::fd_ceiling_refusal() {
            return Err(err.to_string());
        }
        let permit = crate::process_admission::admit_fleet().map_err(|e| e.to_string())?;
        self.spawn_pane_cmd_with_permit(argv, rows, cols, cwd, permit)
    }

    fn spawn_pane_cmd_with_permit(
        &mut self,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
        permit: crate::process_admission::AdmissionPermit,
    ) -> Result<u64, String> {
        // Unit fixtures keep the inline pty (short-lived /bin/cat children
        // can exit before a keeper answers Identify); production panes are
        // keeper-hosted.
        #[cfg(test)]
        let keeper = false;
        #[cfg(not(test))]
        let keeper = true;
        self.spawn_pane_shell_with_permit(argv, rows, cols, cwd, permit, keeper)
    }

    /// The one spawn fork in the road: `keeper = true` routes a pane through
    /// a `fno-agents-worker --pane` process that owns the pty master
    /// out-of-process, so the pane child outlives this server and a fresh
    /// server re-adopts it. EVERY pane takes this road now; the inline pty is
    /// the named fallback for a keeper that cannot start, and the pane entry
    /// is then marked `unkept` (kill-server refuses while one is live). A
    /// deliberate close is unchanged: `reap_pane` sends Kill, the keeper
    /// kills its child, unlinks its socket and exits.
    fn spawn_pane_shell_with_permit(
        &mut self,
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: &str,
        permit: crate::process_admission::AdmissionPermit,
        keeper: bool,
    ) -> Result<u64, String> {
        if argv.is_empty() {
            return Err("pane run needs a command (empty argv)".into());
        }
        let node = node_from_argv(argv);
        let name = agent_self_from_argv(argv);
        let cmd = cmd_from_argv(argv);
        let account = account_from_argv(argv);
        let resume_target = resume_target_from_argv(argv);
        let id = self.reserve_pane_id()?;
        let dir = Some(std::path::Path::new(cwd)).filter(|_| !cwd.is_empty());
        // A keeper that cannot start (missing binary, failed handshake, held
        // seat) must not cost the pane: fall back to the inline pty, say so,
        // and mark the entry `unkept` - the pane is live but will die with
        // the server.
        let mut fell_back: Option<String> = None;
        let (pty, keeper_ring) = if keeper {
            match PtyShell::spawn_cmd_keeper_with_permit(
                &keeper_worker_bin(),
                argv,
                rows,
                cols,
                dir,
                &self.session_name,
                id,
                self.out_tx.clone(),
                self.exit_tx.clone(),
                permit,
            ) {
                Ok(ok) => ok,
                Err(keeper_err) => {
                    let fallback_permit =
                        crate::process_admission::admit_fleet().map_err(|e| e.to_string())?;
                    let shell = PtyShell::spawn_cmd_with_permit(
                        argv,
                        rows,
                        cols,
                        dir,
                        &self.session_name,
                        id,
                        self.out_tx.clone(),
                        self.exit_tx.clone(),
                        fallback_permit,
                    )
                    .map_err(|e| e.to_string())?;
                    fell_back = Some(keeper_err.to_string());
                    (shell, Vec::new())
                }
            }
        } else {
            PtyShell::spawn_cmd_with_permit(
                argv,
                rows,
                cols,
                dir,
                &self.session_name,
                id,
                self.out_tx.clone(),
                self.exit_tx.clone(),
                permit,
            )
            .map(|shell| (shell, Vec::new()))
            .map_err(|e| e.to_string())?
        };
        self.register_pane(
            id,
            pty,
            rows,
            cols,
            node,
            name,
            cwd.to_string(),
            cmd,
            account,
            resume_target,
            refused_worker_from_argv(argv),
            portal_hold_from_argv(argv),
        )?;
        if let Some(keeper_err) = fell_back {
            if let Some(entry) = self.panes.get_mut(&id) {
                entry.unkept = true;
            }
            // `notice_all` only reaches attached clients (Locked 5's own
            // broadcast contract), so a keeper failure with nobody attached
            // yet (the common case at spawn) never reaches the server's own
            // log - the one place a headless caller (a stress script, CI)
            // can see why a pane came up unkept. Say it here too.
            eprintln!(
                "fno mux: keeper unavailable for pane {id} ({keeper_err}); running unkept inline"
            );
            self.notice_all(format!(
                "keeper unavailable for pane {id} ({keeper_err}); running unkept inline"
            ));
        }
        // The keeper's handshake replay carries everything the child printed
        // before the reader thread existed; the VT only now exists, so feed
        // it here (the re-adopt path feeds its ring the same way).
        if !keeper_ring.is_empty() {
            if let Some(entry) = self.panes.get_mut(&id) {
                entry.vt.feed(&keeper_ring);
            }
        }
        Ok(id)
    }

    fn reserve_pane_id(&mut self) -> Result<u64, String> {
        #[cfg(test)]
        {
            let id = self.next_pane_id;
            self.next_pane_id = id.saturating_add(1);
            Ok(id)
        }
        #[cfg(not(test))]
        {
            match crate::squad_store::reserve_next_pane_id(self.next_pane_id) {
                Ok(id) => {
                    self.next_pane_id = id.saturating_add(1);
                    Ok(id)
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        && e.to_string().contains("build-tree binary") =>
                {
                    // A cargo build-tree binary is allowed to run against an
                    // isolated mux without being allowed to write the user's
                    // global squad store. Keep pane run usable with a clear,
                    // process-local fallback; installed binaries still fail
                    // closed on real persistence errors.
                    eprintln!("fno mux: pane id persistence unavailable for build-tree binary; using process-local pane ids (set FNO_AGENTS_HOME for persistence)");
                    let id = self.next_pane_id;
                    self.next_pane_id = id.saturating_add(1);
                    Ok(id)
                }
                Err(e)
                    if e.raw_os_error() == Some(libc::EMFILE)
                        || e.raw_os_error() == Some(libc::ENFILE) =>
                {
                    // The reservation died to the open-file ceiling, not to a
                    // persistence fault: the same wall the pty spawn names,
                    // named here because the reservation runs first.
                    let limit = crate::pty::nofile_limit();
                    let open = usize::try_from(limit).unwrap_or(0);
                    return Err(crate::pty::PtyError::SpawnFdLimit {
                        open,
                        limit,
                        detail: "the pane id reservation hit EMFILE".into(),
                    }
                    .to_string());
                }
                Err(e) => Err(format!("pane id reservation failed: {e}")),
            }
        }
    }

    /// Record a freshly-spawned pane: advance the id floor, insert its VT grid, and
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
        name: Option<String>,
        cwd: String,
        cmd: Option<String>,
        account: Option<String>,
        resume_target: Option<String>,
        refused_worker: Option<String>,
        portal_hold: Option<String>,
    ) -> Result<(), String> {
        let Some(child_pid) = pty.child_pid() else {
            pty.kill();
            return Err(format!("pane {id} has no confirmed child pid"));
        };
        let child = PaneChild {
            pid: child_pid,
            keeper_hosted: pty.is_keeper_hosted(),
        };
        let mut children = match self.pane_children.lock() {
            Ok(children) => children,
            Err(_) => {
                pty.kill();
                return Err("pane child roster lock poisoned".into());
            }
        };
        self.next_pane_id = self.next_pane_id.max(id.saturating_add(1));
        let stats = Arc::new(PaneCounters::default());
        self.panes.insert(
            id,
            PaneEntry {
                pty,
                vt: vt::Pane::new(rows, cols),
                node,
                name,
                cwd,
                cmd,
                account,
                resume_target,
                refused_worker,
                portal_hold,
                unreconciled: false,
                unkept: false,
                last_output: Instant::now(),
                stats: Arc::clone(&stats),
                nudge_due: None,
                requested_size: (rows, cols),
            },
        );
        self.pane_stats.write().unwrap().insert(id, stats);
        let (tx, _rx) = watch::channel(WaitTick::default());
        self.pane_watch.insert(id, tx);
        children.insert(child);
        e2e_log(format_args!("pane {id} registered ({rows}x{cols})"));
        Ok(())
    }

    /// Kill+reap a pane's PTY and retire its watch (flipping `exited` so any
    /// subscribed `PaneWait` returns `PaneExited`). The single place panes
    /// leave `panes`/`pane_watch`, so the two maps never drift. Idempotent.
    /// Snapshot panes whose children and PTY readers have both finished.
    /// Every candidate's final output is already enqueued, so the caller can
    /// drain the shared output channel before closing this exact set.
    fn dead_children_ready_to_reap(&self) -> Vec<u64> {
        self.panes
            .iter()
            .filter_map(|(&pid, entry)| entry.pty.is_reap_ready().then_some(pid))
            .collect()
    }

    /// Defensive backstop for a lost PTY-reader exit notification.
    fn reap_dead_children(&mut self, dead: Vec<u64>) -> Flow {
        for pid in dead {
            let worker = self.worker_member_context(pid);
            let detached = self.detached_panes.get(&pid).cloned();
            let ctx = self.member_ctx(pid);
            if let Some(detached) = detached {
                self.reconcile_worker_member_close(&detached, true);
            } else if let Some(worker) = worker {
                self.reconcile_worker_member_close(&worker, true);
            }
            self.reconcile_member_close(ctx, true);
            if self.close_viewer_died(pid, "child exited") == Flow::Shutdown {
                return Flow::Shutdown;
            }
        }
        Flow::Continue
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
    fn pane_infos_with_agents(&self, agents: &[RegistryAgent]) -> Vec<PaneInfo> {
        // (v71) One evidence read feeds every pane row's orphan verdict.
        let evidence = self.member_evidence();
        let mut out: Vec<PaneInfo> = self
            .panes
            .iter()
            .map(|(&pid, entry)| {
                let (squad_id, squad_name, tab_id, cwd, tab_name, tab_ordinal) =
                    match self.session.find_pane(pid) {
                        Some((sid, ti)) => {
                            let sq = self.session.squad(sid).expect("find_pane live squad");
                            let dict = sq.tab_dict(ti);
                            (
                                sid,
                                sq.name.clone(),
                                sq.tabs[ti].id,
                                sq.canonical_cwd().to_string(),
                                dict.as_ref().and_then(|d| d.name.clone()),
                                dict.map(|d| d.ordinal),
                            )
                        }
                        None => (0, None, 0, String::new(), None, None),
                    };
                // The lineage join: when exactly ONE registry row
                // hosts this pane, its identity fields ride the listing so
                // the stable thread id (fno_id) and the CURRENT harness
                // session are printed beside each other, never conflated.
                // An ambiguous pane (two rows on one mux ref) carries none
                // of them, mirroring the fno_id rule.
                let joined_rows: Vec<&RegistryAgent> = agents
                    .iter()
                    .filter(|a| {
                        a.mux
                            .as_ref()
                            .is_some_and(|(sess, pane)| sess == &self.session_name && *pane == pid)
                    })
                    .collect();
                let joined_row = joined_rows
                    .first()
                    .filter(|_| joined_rows.len() == 1)
                    .copied();
                let orphan = self.orphaned_worker_for_pane(pid, agents, &evidence);
                PaneInfo {
                    pane_id: pid,
                    squad_id,
                    squad_name,
                    tab_id,
                    cwd,
                    child_pid: entry.pty.child_pid(),
                    title: entry.vt.osc_title().map(str::to_string),
                    // A portal seat never reads pristine: the seat is
                    // load-bearing, so no cleanup caller may close it.
                    pristine_idle_shell: entry.cmd.is_none()
                        && entry.vt.is_pristine_idle_shell()
                        && self.portal_of(Some(pid)).is_none(),
                    // (v65) The spent-shell reading: shell integration
                    // measured, nothing running now. `cmd.is_none()` keeps an
                    // agent or `pane run` pane out of the category even when
                    // its shell layer reads idle.
                    shell_idle: entry.cmd.is_none()
                        && matches!(entry.vt.shell_activity(), vt::ShellActivity::Idle),
                    name: entry.name.clone(),
                    tab_name,
                    tab_ordinal,
                    // The fno_id join: the registry row whose mux ref
                    // points at this pane in THIS session carries the durable
                    // identity. Server-owned (self.agents is the cached read).
                    fno_id: self.fno_id_for_pane_with_agents(pid, agents),
                    orphaned_worker: orphan.orphaned,
                    release: orphan.release,
                    harness_session_id: joined_row.and_then(|a| a.harness_session_id.clone()),
                    predecessor_session_ids: joined_row
                        .map(|a| a.predecessor_session_ids.clone())
                        .unwrap_or_default(),
                    forked_from_session_id: joined_row
                        .and_then(|a| a.forked_from_session_id.clone()),
                    // (v90) The seat's portal index, under the same one-row
                    // rule the sideline marker wears.
                    portal: self.portal_marker(Some(pid)),
                }
            })
            .collect();
        out.sort_by_key(|p| p.pane_id);
        out
    }

    fn pane_ls_from_fresh_agents(&self, agents: Option<&[RegistryAgent]>) -> ServerMsg {
        match agents {
            Some(rows) => ServerMsg::PaneList {
                panes: self.pane_infos_with_agents(rows),
            },
            None => ServerMsg::Err {
                code: err_code::REGISTRY_UNAVAILABLE,
                msg: "agent registry unavailable".into(),
            },
        }
    }

    /// The `fno_id` (durable session id) of the registry row hosting `pid` in
    /// this session, if any. The forward half of the identity join (Locked
    /// Decision 6); `PaneWhere` is the reverse.
    #[cfg(test)]
    fn fno_id_for_pane(&self, pid: u64) -> Option<String> {
        self.fno_id_for_pane_with_agents(pid, &self.agents)
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
    /// split was refused at min-size and the pane landed as a new tab instead (AC3-FR; caller notices
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
                // Every squad created remains across a restart, TUI or API: an
                // unnamed lane minted here (a `pane run`, a fresh attach) persists
                // immediately, even before any member attaches, keyed by its
                // durable key.
                self.squad_members.entry(sid).or_default();
                self.pre_restore_squads.insert(sid);
                self.persist_squad(sid);
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

    fn placement_pane_count(&self, dest: Option<u64>, placement: &PanePlacement) -> usize {
        if matches!(placement.tab, Some(TabSel::New))
            || (placement.split.is_none() && placement.at.is_none() && placement.tab.is_none())
        {
            return 0;
        }
        let Some(sid) = dest else {
            return 0;
        };
        let Some(squad) = self.session.squad(sid) else {
            return 0;
        };
        let tab_index = if let Some(anchor) = placement.at {
            self.session
                .find_pane(anchor)
                .and_then(|(found_sid, ti)| (found_sid == sid).then_some(ti))
        } else if let Some(selector) = placement.tab.as_ref() {
            self.resolve_tab_index(sid, selector).ok()
        } else if squad.tabs.is_empty() {
            None
        } else {
            Some(squad.active_tab.min(squad.tabs.len() - 1))
        };
        tab_index
            .and_then(|index| squad.tabs.get(index))
            .map(|tab| tree::leaves(&tab.root).len())
            .unwrap_or(0)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_pane(
        &mut self,
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        claim: bool,
        placement: PanePlacement,
        worker: Option<String>,
    ) -> Result<u64, (u32, String)> {
        let mut placement = placement;
        placement.max_panes = Some(crate::process_admission::configured_pane_group_max(
            placement.max_panes,
        ));
        // The worker name reaches the store and later keys a resume,
        // so the SERVER re-validates it before any pane exists - the CLI gate
        // covers one caller, the control socket is reachable by any client.
        if let Some(name) = &worker {
            if !crate::squad_store::valid_worker_name(name) {
                return Err((
                    err_code::BAD_REQUEST,
                    "worker name must be a registry name ([A-Za-z0-9._-], <=64 chars)".into(),
                ));
            }
        }
        if let Some(refusal) = placement_fit::refuse_fit_with_geometry(&placement) {
            return Err(refusal);
        }
        // Create-if-absent lives ONLY here on the script path (Locked 7): a `pane run --squad
        // <name>` for a not-yet-existing squad mints one so lanes group by project; AttachAgent / UI targets
        // stay fail-closed. Only an UNKNOWN name is creatable (blank / unknown id still error). Resolved
        // pre-spawn so a bad target refuses with no pane.
        let (dest, create_name): (Option<u64>, Option<String>) = match &placement.target {
            PaneTarget::SquadName(name) => {
                let n = name.trim();
                if n.is_empty() {
                    return Err((err_code::BAD_REQUEST, "squad name cannot be blank".into()));
                }
                match self.resolve_placement_target(&placement.target, None) {
                    Ok(d) => (d, None),
                    // Coupled to resolve_placement_target's error text: a name matching NO squad is
                    // creatable; an ambiguous name (2+ matches) still errors - never silently pick one.
                    Err(e) if e.starts_with("no such squad") => (None, Some(n.to_string())),
                    Err(e) => return Err((err_code::BAD_REQUEST, e)),
                }
            }
            _ => {
                // `CurrentRoute` here defaults to the squad owning the spawn's
                // CWD, not the active/gazed-at squad that `resolve_squad`
                // (the tab and layout verbs) passes for the same token. Both
                // defaults are deliberate; the pair is not interchangeable.
                let current = self.session.find_by_cwd(&squad_key);
                let dest = self
                    .resolve_placement_target(&placement.target, current)
                    .map_err(|e| (err_code::BAD_REQUEST, e))?;
                (dest, None)
            }
        };
        let pane_count = self.placement_pane_count(dest, &placement);
        let permit = crate::process_admission::admit_pane(pane_count, placement.max_panes)
            .map_err(|e| (err_code::SPAWN_FAILED, e.to_string()))?;
        // The worker path is the keeper path: a recorded member's pane
        // outlives this server. Everything else spawns inline.
        let mut spawn_argv = argv.clone();
        if let Some(worker) = worker.as_deref() {
            if agent_self_from_argv(&spawn_argv).is_none() {
                let mut wrapped = vec!["env".to_string(), format!("FNO_AGENT_SELF={worker}")];
                wrapped.extend(spawn_argv);
                spawn_argv = wrapped;
            }
        }
        // Every spawned pane takes the keeper road in production, worker or
        // not; unit fixtures keep today's split (short-lived fixtures can
        // exit before a keeper answers Identify).
        #[cfg(test)]
        let keeper = worker.is_some();
        #[cfg(not(test))]
        let keeper = true;
        let pid = self
            .spawn_pane_shell_with_permit(&spawn_argv, rows, cols, &cwd, permit, keeper)
            .map_err(|e| (err_code::SPAWN_FAILED, e))?;
        if claim {
            // Writer-claim ELIGIBILITY, set only at agent spawn (Locked 5).
            // The claim itself is acquired per-burst via PaneClaim.
            self.claim_eligible.insert(pid);
        }
        if let Some(name) = create_name {
            // Origins = the spawn's repo root, so same-project lanes converge here. persist_squad
            // write-through is non-blocking: a failed write degrades restore, not the live session.
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
            self.pre_restore_squads.insert(sid);
            if let Some(worker) = &worker {
                self.record_worker_member(sid, worker, pid, &cwd, None);
            }
            self.persist_squad(sid);
        } else {
            // v41: place_with honors placement.tab / placement.at; it
            // falls through to place_spawned_pane on the pre-v41 no-tab/no-anchor
            // path, and reaps `pid` on any hard error so a bad anchor never
            // orphans a pane.
            let (sid, _tid, _) = self.place_with(dest, &squad_key, pid, &placement)?;
            if let Some(worker) = &worker {
                self.record_worker_member(sid, worker, pid, &cwd, None);
            }
        }
        // Keep any attached client's view consistent; a script-only session
        // has no clients, so this is then a cheap no-op.
        self.push_layout(true);
        Ok(pid)
    }

    // ---- v41 layout script API ---------------------------------

    /// Resolve a [`TabSel`] to a tab INDEX within `sid`, through the squad's
    /// tab dictionary: `Index(n)` is the 1-based ordinal the UI
    /// shows, never a zero-based vector index. `New` is not a selector here
    /// (callers that support creation handle it before calling).
    fn resolve_tab_index(&self, sid: u64, sel: &TabSel) -> Result<usize, String> {
        let sq = self
            .session
            .squad(sid)
            .ok_or_else(|| format!("no such squad id: {sid}"))?;
        if sq.tabs.is_empty() {
            return Err(format!("squad {sid} has no tabs"));
        }
        sq.resolve_tab(sel)
    }

    /// Placement that honors `placement.tab` / `placement.at` (v41), reaping
    /// `pid` on any hard error so a bad anchor never orphans a pane. The
    /// pre-v41 no-tab/no-anchor path delegates to [`Self::place_spawned_pane`]
    /// unchanged.
    fn place_with(
        &mut self,
        dest: Option<u64>,
        squad_key: &str,
        pid: u64,
        placement: &PanePlacement,
    ) -> Result<(u64, TabId, bool), (u32, String)> {
        if placement.fit {
            // Server-chosen tab; the tab/at/split combination is
            // refused in run_pane, so the strict-anchor branch is unreachable.
            return self.place_with_fit(dest, squad_key, pid, placement);
        }
        // Strict origin placement (--at current, fallback=Refuse): locate the
        // anchor pane, infer its squad+tab, refuse a conflicting explicit
        // selector, and never fall back to a new tab. Handled before the
        // selector-resolved path, which assumes the active tab rather than the
        // anchor's real location.
        if placement.at.is_some() && placement.fallback == PlacementFallback::Refuse {
            return self.place_strict_at_anchor(pid, placement);
        }
        if placement.tab.is_none() && placement.at.is_none() {
            return self
                .place_spawned_pane(dest, squad_key, pid, placement.split)
                .map_err(|e| (err_code::SPAWN_FAILED, e));
        }
        let Some(sid) = dest else {
            self.reap_pane(pid);
            return Err((
                err_code::BAD_REQUEST,
                "a --tab/--at placement needs a resolved squad".into(),
            ));
        };
        let Some(si) = self.session.squads.iter().position(|s| s.id == sid) else {
            self.reap_pane(pid);
            return Err((err_code::SPAWN_FAILED, "selected squad vanished".into()));
        };
        // An explicit `New` tab ignores any anchor - it is born with this pane.
        if matches!(placement.tab, Some(TabSel::New)) {
            let tid = self.session.mint_tab_id();
            self.session.squads[si].tabs.push(Tab {
                name: None,
                id: tid,
                root: Node::Leaf(pid),
                focus: pid,
            });
            return Ok((sid, tid, false));
        }
        let ti = match &placement.tab {
            Some(sel) => match self.resolve_tab_index(sid, sel) {
                Ok(ti) => ti,
                Err(e) => {
                    self.reap_pane(pid);
                    return Err((err_code::BAD_REQUEST, e));
                }
            },
            None => {
                let sq = &self.session.squads[si];
                sq.active_tab.min(sq.tabs.len().saturating_sub(1))
            }
        };
        let tid = self.session.squads[si].tabs[ti].id;
        // Same pane cap as the strict path: a numeric anchor or explicit
        // --tab resolves here instead, and the cap the caller believes in
        // must not silently stop at one spelling of --at. The New-tab branch
        // above is born with a single pane and needs no check.
        if let Some(cap) = placement.max_panes {
            let pane_count = tree::leaves(&self.session.squads[si].tabs[ti].root).len();
            if pane_count >= cap {
                self.reap_pane(pid);
                return Err((
                    err_code::BAD_REQUEST,
                    format!(
                        "placement refused: target tab has {pane_count} panes and the cap is {cap}. Use --workspace <name> to place this worker elsewhere"
                    ),
                ));
            }
        }
        let vp = self.tab_rect(tid);
        let anchor = placement
            .at
            .unwrap_or(self.session.squads[si].tabs[ti].focus);
        let dir = placement.split.unwrap_or(Dir::Down);
        if !tree::leaves(&self.session.squads[si].tabs[ti].root).contains(&anchor) {
            self.reap_pane(pid);
            return Err((
                err_code::BAD_REQUEST,
                format!("anchor pane {anchor} is not in the target tab"),
            ));
        }
        let res = {
            let tab = &mut self.session.squads[si].tabs[ti];
            tree::split_at(tab, vp, anchor, dir, pid)
        };
        match res {
            Ok(()) => Ok((sid, tid, false)),
            // Min-size refusal falls back to a fresh tab, never a dead-end -
            // mirroring place_spawned_pane. Strict placement never reaches here
            // (it routes through place_strict_at_anchor), but a Refuse policy on
            // any selector-resolved path still fails closed rather than minting.
            Err(tree::SplitError::TooSmall { .. }) => {
                if placement.fallback == PlacementFallback::Refuse {
                    self.reap_pane(pid);
                    return Err((
                        err_code::BAD_REQUEST,
                        "placement cannot fit: a resulting pane would be below the minimum size"
                            .into(),
                    ));
                }
                let ntid = self.session.mint_tab_id();
                self.session.squads[si].tabs.push(Tab {
                    name: None,
                    id: ntid,
                    root: Node::Leaf(pid),
                    focus: pid,
                });
                Ok((sid, ntid, true))
            }
            Err(e) => {
                self.reap_pane(pid);
                Err((err_code::BAD_REQUEST, e.to_string()))
            }
        }
    }

    /// Strict origin placement for `--at current` (v44): the anchor is
    /// pinned to the calling pane id, so focus races cannot redirect it. The
    /// server locates the anchor once inside this (serialized) turn, infers its
    /// squad+tab, refuses a conflicting explicit selector, splits beside it, and
    /// never creates a tab or moves focus. Any failure reaps the pre-spawned
    /// child and leaves the tree, focus, and registry untouched.
    fn place_strict_at_anchor(
        &mut self,
        pid: u64,
        placement: &PanePlacement,
    ) -> Result<(u64, TabId, bool), (u32, String)> {
        let anchor = placement.at.expect("strict placement carries an anchor");
        let dir = placement.split.unwrap_or(Dir::Down);
        let (sid, ti) = match self.session.find_pane(anchor) {
            Some(loc) => loc,
            None => {
                self.reap_pane(pid);
                return Err((
                    err_code::BAD_REQUEST,
                    format!("anchor pane {anchor} no longer exists"),
                ));
            }
        };
        // An explicit workspace/tab selector must name the anchor's actual
        // location; anything else is refused rather than redirecting placement.
        if !matches!(placement.target, PaneTarget::CurrentRoute) {
            let ok = matches!(self.resolve_placement_target(&placement.target, None), Ok(s) if s == Some(sid));
            if !ok {
                self.reap_pane(pid);
                return Err((
                    err_code::BAD_REQUEST,
                    format!("anchor pane {anchor} is not in the requested workspace"),
                ));
            }
        }
        if let Some(sel) = placement.tab.as_ref() {
            if matches!(sel, TabSel::New) {
                // Strict placement pins the anchor's tab and never mints one;
                // `--tab new` asks for the opposite, so the combination is
                // refused rather than silently dropping the new-tab request.
                self.reap_pane(pid);
                return Err((
                    err_code::BAD_REQUEST,
                    "exact placement cannot combine --at with a new-tab selector".into(),
                ));
            }
            let ok = matches!(self.resolve_tab_index(sid, sel), Ok(rti) if rti == ti);
            if !ok {
                self.reap_pane(pid);
                return Err((
                    err_code::BAD_REQUEST,
                    format!("anchor pane {anchor} is not in the requested tab"),
                ));
            }
        }
        if let Some(cap) = placement.max_panes {
            let pane_count = tree::leaves(
                &self
                    .session
                    .squad(sid)
                    .expect("find_pane returned a live squad")
                    .tabs[ti]
                    .root,
            )
            .len();
            if pane_count >= cap {
                self.reap_pane(pid);
                return Err((
                    err_code::BAD_REQUEST,
                    format!(
                        "exact placement refused: target tab has {pane_count} panes and the cap is {cap}. Use --workspace <name> to place this worker elsewhere"
                    ),
                ));
            }
        }
        let tid = self
            .session
            .squad(sid)
            .expect("find_pane returned a live squad")
            .tabs[ti]
            .id;
        let vp = self.tab_rect(tid);
        let res = {
            let tab = &mut self
                .session
                .squad_mut(sid)
                .expect("find_pane returned a live squad")
                .tabs[ti];
            tree::split_at(tab, vp, anchor, dir, pid)
        };
        match res {
            Ok(()) => Ok((sid, tid, false)),
            Err(tree::SplitError::TooSmall { .. }) => {
                self.reap_pane(pid);
                Err((
                    err_code::BAD_REQUEST,
                    "exact placement cannot fit: a resulting pane would be below the minimum size"
                        .into(),
                ))
            }
            Err(e) => {
                self.reap_pane(pid);
                Err((err_code::BAD_REQUEST, e.to_string()))
            }
        }
    }

    /// Split an ARBITRARY pane (not just the focus) into a fresh shell pane on
    /// `direction`'s side. `no_focus` (default true on the wire) keeps every
    /// viewer's focus put (Locked Decision 3); focus moves only on the opt-in.
    /// Spawn-first, so a min-size refusal reaps the pre-spawned shell with the
    /// tree untouched.
    fn split_pane_script(
        &mut self,
        pane: u64,
        direction: Dir,
        no_focus: bool,
    ) -> Result<u64, (u32, String)> {
        let (sid, ti) = self
            .session
            .find_pane(pane)
            .ok_or((err_code::DEAD_PANE, format!("no such pane: {pane}")))?;
        let tid = self.session.squad(sid).expect("find_pane live").tabs[ti].id;
        let vp = self.tab_rect(tid);
        let cwd = self
            .session
            .squad(sid)
            .map(|s| s.canonical_cwd().to_string())
            .unwrap_or_default();
        let (rows, cols) = self
            .panes
            .get(&pane)
            .map(|e| e.vt.size())
            .unwrap_or((vp.rows, vp.cols));
        let new_pid = self
            .spawn_pane(rows, cols, &cwd)
            .map_err(|e| (err_code::SPAWN_FAILED, e))?;
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == sid)
            .expect("squad live");
        let res = {
            let tab = &mut self.session.squads[si].tabs[ti];
            tree::split_at(tab, vp, pane, direction, new_pid).map(|()| {
                if !no_focus {
                    tab.focus = new_pid;
                }
            })
        };
        match res {
            Ok(()) => {
                self.push_layout(true);
                Ok(new_pid)
            }
            Err(e) => {
                self.reap_pane(new_pid);
                Err((err_code::BAD_REQUEST, e.to_string()))
            }
        }
    }

    /// List a squad's tabs for [`ControlVerb::TabLs`].
    fn tab_ls(&self, squad: &PaneTarget) -> Result<Vec<TabInfo>, (u32, String)> {
        let sid = self.resolve_squad(squad)?;
        let sq = self
            .session
            .squad(sid)
            .ok_or((err_code::BAD_REQUEST, format!("no such squad id: {sid}")))?;
        let active_ti = sq.active_tab.min(sq.tabs.len().saturating_sub(1));
        Ok(sq
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabInfo {
                tab_id: t.id,
                name: t.name.clone(),
                pane_ids: tree::leaves(&t.root),
                active: i == active_ti,
            })
            .collect())
    }

    /// Create a new tab (born with one shell leaf) for [`ControlVerb::TabCreate`].
    /// Returns the new leaf's pane id.
    fn tab_create(
        &mut self,
        squad: &PaneTarget,
        name: Option<String>,
    ) -> Result<u64, (u32, String)> {
        let sid = self.resolve_squad(squad)?;
        let cwd = self
            .session
            .squad(sid)
            .map(|s| s.canonical_cwd().to_string())
            .unwrap_or_default();
        let pid = self
            .spawn_pane(vt::DEFAULT_ROWS, vt::DEFAULT_COLS, &cwd)
            .map_err(|e| (err_code::SPAWN_FAILED, e))?;
        let Some(si) = self.session.squads.iter().position(|s| s.id == sid) else {
            self.reap_pane(pid);
            return Err((err_code::SPAWN_FAILED, "selected squad vanished".into()));
        };
        let tid = self.session.mint_tab_id();
        self.session.squads[si].tabs.push(Tab {
            name: clean_tab_name(name),
            id: tid,
            root: Node::Leaf(pid),
            focus: pid,
        });
        self.push_layout(true);
        Ok(pid)
    }

    /// Rename a tab for [`ControlVerb::TabRename`]. A blank name clears it.
    fn tab_rename(
        &mut self,
        squad: &PaneTarget,
        sel: &TabSel,
        name: String,
    ) -> Result<(), (u32, String)> {
        let sid = self.resolve_squad(squad)?;
        let ti = self
            .resolve_tab_index(sid, sel)
            .map_err(|e| (err_code::BAD_REQUEST, e))?;
        let clean = clean_tab_name(Some(name));
        let sq = self
            .session
            .squad_mut(sid)
            .ok_or((err_code::BAD_REQUEST, "squad vanished".to_string()))?;
        let tid = sq.tabs[ti].id;
        sq.tabs[ti].name = clean;
        // A template tab's stored spec is keyed by tab name; a rename
        // must re-persist so restore finds it under the new name and drops the
        // old key (set_tab_specs replaces the squad's whole list by current tabs).
        if self.template_specs.contains_key(&tid) {
            self.persist_template_specs(sid);
        }
        self.push_layout(true);
        Ok(())
    }

    /// The ONE reorder trunk: move `tab_id` `delta` slots within
    /// `squad_id`, holding the squad's active tab across the move. Returns
    /// whether anything changed; the caller pushes the layout. `find_tab` and
    /// the cross-squad guard stay with the callers - the interactive path
    /// resolves by id anywhere in the session, the control path resolves both
    /// selectors inside one squad.
    fn reorder_tab(&mut self, squad_id: u64, tab_id: TabId, delta: i32) -> bool {
        let Some(sq) = self.session.squad_mut(squad_id) else {
            return false;
        };
        let Some(idx) = sq.tabs.iter().position(|t| t.id == tab_id) else {
            return false;
        };
        let new = (idx as i64 + delta as i64).clamp(0, sq.tabs.len() as i64 - 1) as usize;
        if new == idx {
            return false;
        }
        let active = sq.tabs.get(sq.active_tab).map(|tab| tab.id);
        let moved = sq.tabs.remove(idx);
        sq.tabs.insert(new, moved);
        sq.active_tab = active
            .and_then(|id| sq.tabs.iter().position(|candidate| candidate.id == id))
            .unwrap_or_else(|| sq.active_tab.min(sq.tabs.len().saturating_sub(1)));
        true
    }

    /// (v65) `ControlVerb::TabReorder`: the `fno mux tab move` door
    /// onto the trunk the tab bar uses. Both selectors resolve through the
    /// shared grammar; the destination names a POSITION, so the delta is
    /// computed here and never shipped over the wire.
    fn tab_reorder(
        &mut self,
        squad: &PaneTarget,
        tab: &TabSel,
        to: &TabSel,
    ) -> Result<(), (u32, String)> {
        let sid = self.resolve_squad(squad)?;
        let from = self
            .resolve_tab_index(sid, tab)
            .map_err(|e| (err_code::BAD_REQUEST, e))?;
        let dest = self
            .resolve_tab_index(sid, to)
            .map_err(|e| (err_code::BAD_REQUEST, e))?;
        let tid = self
            .session
            .squad(sid)
            .ok_or((err_code::BAD_REQUEST, "squad vanished".to_string()))?
            .tabs[from]
            .id;
        if self.reorder_tab(sid, tid, (dest as i64 - from as i64) as i32) {
            self.push_layout(true);
        }
        Ok(())
    }

    /// Close one resolved tab after every pre-mutation guard has passed. This
    /// is the single cascade used by both the interactive command and the
    /// script control verb, so reaping, member cleanup, persistence cleanup,
    /// template cleanup, and viewer re-anchoring cannot drift.
    fn close_tab_cascade(
        &mut self,
        sid: u64,
        ti: usize,
    ) -> Option<(TabId, Vec<u64>, RemoveOutcome)> {
        let (tid, pids) = {
            let sq = self.session.squad(sid)?;
            let tab = sq.tabs.get(ti)?;
            (tab.id, tree::leaves(&tab.root))
        };
        let ctxs: Vec<_> = pids
            .iter()
            .filter_map(|&pid| self.member_ctx(pid))
            .collect();
        let ident = self.squad_identity(sid);
        for &pid in &pids {
            self.reap_pane(pid);
        }
        let outcome = self.session.remove_tab(sid, ti);
        let survived = matches!(outcome, RemoveOutcome::TabRemoved);
        let member_ctx_count = ctxs.len();
        for ctx in ctxs {
            self.reconcile_member_close(Some(ctx), false);
        }
        if matches!(
            outcome,
            RemoveOutcome::SquadRemoved | RemoveOutcome::SessionEmpty
        ) {
            self.squad_members.remove(&sid);
            if let Some((name, key)) = ident {
                self.persist_remove(&name, &key);
            }
        } else if survived && member_ctx_count == 0 {
            // A shell-only tab close persisted NOTHING before (the
            // member reconcile is the only other writer), so its stored tree
            // outlived the close and restore replayed it forever. Capture the
            // surviving topology here.
            self.persist_squad(sid);
        }
        self.tab_areas.remove(&tid);
        if matches!(outcome, RemoveOutcome::SessionEmpty) {
            self.template_specs.remove(&tid);
            return Some((tid, pids, outcome));
        }
        if self.template_specs.remove(&tid).is_some() {
            self.persist_template_specs(sid);
        }
        self.reanchor_views();
        self.push_layout(true);
        Some((tid, pids, outcome))
    }

    /// Resolve and guard a script close before entering the shared mutation
    /// helper. A worker is safe to ignore only when its fresh row is
    /// positively `Dead`; `Alive` and `Unmeasured` both refuse.
    fn tab_close(
        &mut self,
        squad: &PaneTarget,
        sel: &TabSel,
        force: bool,
        agents: Option<&[RegistryAgent]>,
    ) -> Result<(TabId, Vec<u64>, RemoveOutcome), (u32, String)> {
        let sid = self.resolve_squad(squad)?;
        let ti = self
            .resolve_tab_index(sid, sel)
            .map_err(|e| (err_code::BAD_REQUEST, e))?;
        let pids = self
            .session
            .squad(sid)
            .and_then(|sq| sq.tabs.get(ti))
            .map(|tab| tree::leaves(&tab.root))
            .ok_or((err_code::BAD_REQUEST, "selected tab vanished".into()))?;
        if !force {
            let Some(rows) = agents else {
                return Err((
                    err_code::REGISTRY_UNAVAILABLE,
                    "agent registry unavailable".into(),
                ));
            };
            let blockers = tab_close_blockers(&self.session_name, &pids, rows);
            if !blockers.is_empty() {
                return Err((
                    err_code::BAD_REQUEST,
                    format!("tab contains protected worker(s): {}", blockers.join(", ")),
                ));
            }
        }
        self.close_tab_cascade(sid, ti)
            .ok_or((err_code::BAD_REQUEST, "selected tab vanished".into()))
    }

    /// The nested tree + per-pane geometry of one tab (Locked Decision 5).
    /// `agents` (Some) additionally fills the per-pane worker join for the
    /// control-verb path; `None` keeps the machine shape unchanged.
    fn tab_layout(&self, tab: &Tab, agents: Option<&[RegistryAgent]>) -> TabLayout {
        let vp = self.tab_rect(tab.id);
        TabLayout {
            tab_id: tab.id,
            name: tab.name.clone(),
            focus: tab.focus,
            root: tab.root.clone(),
            panes: tree::layout(&tab.root, vp),
            workers: agents.map(|rows| {
                tree::leaves(&tab.root)
                    .into_iter()
                    .map(|pid| TabPaneOccupant {
                        pane_id: pid,
                        fno_id: self.fno_id_for_pane_with_agents(pid, rows),
                    })
                    .collect()
            }),
        }
    }

    fn squad_layout(&self, sq: &Squad, agents: Option<&[RegistryAgent]>) -> SquadLayout {
        SquadLayout {
            squad_id: sq.id,
            squad_name: sq.name.clone(),
            tabs: sq.tabs.iter().map(|t| self.tab_layout(t, agents)).collect(),
        }
    }

    /// Dump a [`LayoutScope`] for [`ControlVerb::LayoutGet`].
    fn layout_get(
        &self,
        scope: &LayoutScope,
        agents: Option<&[RegistryAgent]>,
    ) -> Result<Vec<SquadLayout>, (u32, String)> {
        match scope {
            LayoutScope::Session => Ok(self
                .session
                .squads
                .iter()
                .map(|s| self.squad_layout(s, agents))
                .collect()),
            LayoutScope::Squad(t) => {
                let sid = self.resolve_squad(t)?;
                let sq = self
                    .session
                    .squad(sid)
                    .ok_or((err_code::BAD_REQUEST, format!("no such squad id: {sid}")))?;
                Ok(vec![self.squad_layout(sq, agents)])
            }
            LayoutScope::Tab { squad, tab } => {
                let sid = self.resolve_squad(squad)?;
                let ti = self
                    .resolve_tab_index(sid, tab)
                    .map_err(|e| (err_code::BAD_REQUEST, e))?;
                let sq = self.session.squad(sid).expect("resolve_squad live");
                Ok(vec![SquadLayout {
                    squad_id: sq.id,
                    squad_name: sq.name.clone(),
                    tabs: vec![self.tab_layout(&sq.tabs[ti], agents)],
                }])
            }
        }
    }

    /// Point every attached (non-passive) viewer at `pane`, wherever it lives:
    /// the [`ControlVerb::PaneFocus`] handler, and the inverse of every other
    /// `Pane*` verb (those act ON a pane for an agent; this moves the OPERATOR).
    ///
    /// The actual focus is [`Command::FocusPane`] through [`Core::command`] - the
    /// one trunk the `-> Focus` menu entry and the navigator already end at. That
    /// is deliberate reuse, not laziness: `FocusPane` resolves the leaf
    /// session-wide and `set_view`s the client onto its (squad, tab), so
    /// cross-squad and cross-tab goto come for free, and so does clearing a Done
    /// pane's unseen bit via `mark_seen_if_done`. A bespoke handler here would
    /// silently drop that side effect, which is exactly when it should fire: an
    /// agent pointing the operator at a finished pane.
    ///
    /// `clients_moved` counts clients whose view IS the resolved (squad, tab)
    /// afterwards, not clients the loop visited. Accepting a command is not
    /// evidence anything moved on screen, and a receipt that conflates the two
    /// is the `queued (durable)`-read-as-delivered class of lie.
    fn pane_focus(&mut self, pane: u64) -> ServerMsg {
        let Some((sid, ti)) = self.session.find_pane(pane) else {
            return ServerMsg::Err {
                code: err_code::DEAD_PANE,
                msg: format!("no such pane: {pane}"),
            };
        };
        let sq = self.session.squad(sid).expect("find_pane live");
        let dict = sq.tab_dict(ti);
        let (tab_id, squad_name) = (sq.tabs[ti].id, sq.name.clone());
        // A passive (observer) client is read-only at the server and has no
        // viewport to move, so it is not a candidate and never inflates the
        // count. No candidates at all is a REFUSAL, not a zero-moved success:
        // "nobody is watching" is a different problem from "your pane is gone",
        // and an absent client reported as a pass is the absence-versus-
        // instrument-failure trap.
        let targets: Vec<u64> = self
            .clients
            .iter()
            .filter(|c| !c.passive)
            .map(|c| c.id)
            .collect();
        if targets.is_empty() {
            return ServerMsg::Err {
                code: err_code::NO_CLIENT,
                msg: "no attached client to move".into(),
            };
        }
        let mut clients_moved = 0usize;
        for cid in targets {
            self.command(cid, Command::FocusPane(pane));
            // Re-read the view rather than trusting the dispatch: a client that
            // disconnected between the snapshot above and this dispatch is gone
            // from `clients` and simply does not count.
            if self
                .clients
                .iter()
                .any(|c| c.id == cid && c.view == (sid, tab_id))
            {
                clients_moved += 1;
            }
        }
        ServerMsg::PaneFocused {
            pane,
            squad_id: sid,
            squad_name,
            tab_id,
            tab_name: dict.as_ref().and_then(|d| d.name.clone()),
            tab_ordinal: dict.map(|d| d.ordinal),
            clients_moved,
        }
    }

    /// Break `pane` into its own new tab in the same squad, keeping the PTY
    /// alive ([`tree::detach_leaf`], never a reap). If the source tab emptied,
    /// remove it (AC1-EDGE: never leave an empty tab). Returns the new tab id.
    fn pane_break(&mut self, pane: u64, name: Option<String>) -> Result<TabId, (u32, String)> {
        let (sid, ti) = self
            .session
            .find_pane(pane)
            .ok_or((err_code::DEAD_PANE, format!("no such pane: {pane}")))?;
        let tid = self.session.squad(sid).expect("find_pane live").tabs[ti].id;
        let vp = self.tab_rect(tid);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == sid)
            .expect("squad live");
        let src_name = self.session.squads[si].tabs[ti].name.clone();
        let outcome = {
            let tab = &mut self.session.squads[si].tabs[ti];
            tree::detach_leaf(tab, vp, pane).map_err(|e| (err_code::BAD_REQUEST, e.to_string()))?
        };
        let new_tid = self.session.mint_tab_id();
        // Push the new tab FIRST so the squad is never transiently empty, then
        // (for TabEmptied) drop the now-orphaned source tab that still holds
        // this pane - otherwise the pane would live in two tabs.
        self.session.squads[si].tabs.push(Tab {
            // An explicit name wins. Otherwise, when the break empties the
            // source tab, carry its name over instead of dropping it: the tab
            // is being rebuilt around the same single pane, and the operator
            // named that tab rather than a tree shape. Breaking one pane out of
            // several is a genuinely new tab and stays unnamed.
            name: clean_tab_name(name).or_else(|| {
                matches!(outcome, tree::DetachOutcome::TabEmptied)
                    .then_some(src_name)
                    .flatten()
            }),
            id: new_tid,
            root: Node::Leaf(pane),
            focus: pane,
        });
        if matches!(outcome, tree::DetachOutcome::TabEmptied) {
            self.session.remove_tab(sid, ti);
            // The source tab is gone: clear its cached area and re-anchor any
            // client that was viewing it, exactly as the CloseTab path does -
            // otherwise push_layout would skip that client with a blank view.
            self.tab_areas.remove(&tid);
            self.reanchor_views();
        }
        // The broken pane's hosting tab changed (it moved into a new
        // tab, and its old tab may be gone), so refresh the persisted member
        // tab_names for a tracked workspace - else a restart before the next
        // persist restores the member to the old/removed tab. In the shared
        // helper so the script (ControlVerb) and drag (Command) paths stay
        // consistent; a no-op for a squad with no recruited members.
        self.persist_squad_if_members(sid);
        self.push_layout(true);
        Ok(new_tid)
    }

    /// Join a whole source tab into the anchor pane's tab as a split, removing
    /// the source tab. Refuses join-into-self up front (BAD_REQUEST). All PTYs
    /// survive; a min-size failure leaves BOTH trees untouched.
    fn tab_join(
        &mut self,
        src_sel: &TabSel,
        anchor_pane: u64,
        dir: Dir,
    ) -> Result<(), (u32, String)> {
        let (sid, dst_ti) = self.session.find_pane(anchor_pane).ok_or((
            err_code::DEAD_PANE,
            format!("no such anchor pane: {anchor_pane}"),
        ))?;
        let src_ti = self
            .resolve_tab_index(sid, src_sel)
            .map_err(|e| (err_code::BAD_REQUEST, e))?;
        if src_ti == dst_ti {
            return Err((
                err_code::BAD_REQUEST,
                "cannot join a tab into itself".into(),
            ));
        }
        let dst_tid = self.session.squad(sid).expect("find_pane live").tabs[dst_ti].id;
        let vp = self.tab_rect(dst_tid);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == sid)
            .expect("squad live");
        let src_tid = self.session.squads[si].tabs[src_ti].id;
        let src_subtree = self.session.squads[si].tabs[src_ti].root.clone();
        {
            let tab = &mut self.session.squads[si].tabs[dst_ti];
            tree::graft_subtree(tab, vp, anchor_pane, dir, src_subtree)
                .map_err(|e| (err_code::BAD_REQUEST, e.to_string()))?;
        }
        // Graft committed (all-or-nothing): remove the src tab. Its panes rode
        // into dst as a cloned subtree; remove_tab drops only the Tab, never a
        // PTY, so no pane is reaped. Clear the removed tab's cached area and
        // re-anchor any client viewing it (the CloseTab contract) so push_layout
        // never leaves that viewer on a dangling tab.
        self.session.remove_tab(sid, src_ti);
        self.tab_areas.remove(&src_tid);
        self.reanchor_views();
        // The joined tab's members now live in the anchor's tab (their
        // hosting tab changed and the source tab is gone), so refresh the
        // persisted member tab_names for a tracked workspace - same shared-helper
        // reconcile as pane_break, covering both the script and drag paths.
        self.persist_squad_if_members(sid);
        self.push_layout(true);
        Ok(())
    }

    /// Move `mover` out of its source tab to sit adjacent to `target` in another
    /// tab - the cross-tab arm of [`Command::MovePane`] (a sideline-row drop whose
    /// pane lives in a different tab from the drop). Composes the two #553
    /// primitives: graft the mover leaf into the destination FIRST (validated
    /// all-or-nothing), THEN detach it from the source. Ordering is load-bearing -
    /// a min-size refusal returns from the graft before the source is ever
    /// touched, so the pane is never left detached-but-ungrafted. Only the leaf id
    /// moves between trees; the PTY is untouched (detach_leaf never reaps).
    fn move_pane_cross_tab(
        &mut self,
        mover: u64,
        (src_sid, src_ti): (u64, usize),
        target: u64,
        (dst_sid, dst_ti): (u64, usize),
        dir: Dir,
    ) -> Result<(), tree::MoveError> {
        let src_si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == src_sid)
            .expect("src squad live");
        let dst_si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == dst_sid)
            .expect("dst squad live");
        let src_tid = self.session.squads[src_si].tabs[src_ti].id;
        let dst_tid = self.session.squads[dst_si].tabs[dst_ti].id;
        let src_vp = self.tab_rect(src_tid);
        let dst_vp = self.tab_rect(dst_tid);

        // Capture the source's persistence state BEFORE the move (find_pane still
        // resolves the mover in its source): its member context if the mover is a
        // recruited member, and the source squad's persisted name. The move can
        // relocate a member across workspaces, keep it in place in the same
        // workspace, or empty and remove the source squad entirely - each needs a
        // different persistence reconcile below.
        let moved_member = self.member_ctx(mover);
        // The source squad's store identity (name when named, else the durable
        // key), so a move that empties it can depersist the right entry - an
        // unnamed lane persists too now, not only named workspaces.
        let src_identity = self.squad_identity(src_sid);

        // Graft into the destination first, and focus the moved pane there while
        // the dst index is still valid (removing the source tab below can shift
        // sibling indices when both tabs share a squad). On Err the dst is
        // unchanged and the source is never touched.
        {
            let dst_tab = &mut self.session.squads[dst_si].tabs[dst_ti];
            tree::graft_subtree(dst_tab, dst_vp, target, dir, Node::Leaf(mover))?;
            dst_tab.focus = mover;
        }
        // Graft committed: remove the mover from the source. It was present at
        // resolution (find_pane, no intervening yield on the core loop), so
        // detach cannot fail on presence; a now-empty source tab is dropped and
        // its viewers re-anchored, exactly as pane_break / CloseTab do.
        let outcome = {
            let src_tab = &mut self.session.squads[src_si].tabs[src_ti];
            tree::detach_leaf(src_tab, src_vp, mover)?
        };
        if matches!(outcome, tree::DetachOutcome::TabEmptied) {
            self.session.remove_tab(src_sid, src_ti);
            self.tab_areas.remove(&src_tid);
            self.reanchor_views();
        }
        // Reconcile the source workspace's persistence against what the move did:
        if self.session.squad(src_sid).is_none() {
            // The move emptied and removed the source squad. Depersist it
            // REGARDLESS of whether the moved pane was a member - a squad's own
            // initial shell counts, and a lingering `squad_members` entry would
            // resurrect it on restart. Keyed by name when named, else origins;
            // `persist_remove` is a no-op if the squad was never persisted.
            self.squad_members.remove(&src_sid);
            if let Some((name, key)) = src_identity {
                self.persist_remove(&name, &key);
            }
        } else if let Some(ctx) = moved_member {
            if src_sid != dst_sid {
                // Cross-squad: the member left its source workspace - de-recruit
                // it (drop from squad_members) and persist, the CloseTab path.
                self.reconcile_member_close(Some(ctx), false);
            } else {
                // Same-squad relocation: the member stays, but its hosting tab
                // changed, so persist to refresh its stored `tab_name` (else a
                // restart before the next persisting action restores it to the
                // old tab).
                self.persist_squad(src_sid);
            }
        }
        self.push_layout(true);
        Ok(())
    }

    /// Resolve an `fno_id` to its live location for [`ControlVerb::PaneWhere`],
    /// or one of the three distinct error codes. Never an empty-successful
    /// location (Locked Decision 4). REGISTRY_UNAVAILABLE is the CLI's to emit
    /// (it reads registry.json); the server's cache is always consultable, so
    /// an unmatched id here is NOT_FOUND, and a matched-but-paneless one is
    /// NOT_PANE_HOSTED.
    #[cfg(test)]
    fn pane_where(&self, fno_id: &str) -> Result<ServerMsg, u32> {
        self.pane_where_with_agents(fno_id, &self.agents)
    }

    fn pane_where_with_agents(
        &self,
        fno_id: &str,
        agents: &[RegistryAgent],
    ) -> Result<ServerMsg, u32> {
        let id = fno_id.trim();
        if id.is_empty() {
            return Err(err_code::NOT_FOUND);
        }
        // Exact identity wins; a prefix only resolves when it is unambiguous
        // (hits a single distinct identity). An ambiguous prefix is NOT_FOUND,
        // never a silent pick of the first registry row (codex P2).
        let exact: Vec<&RegistryAgent> = agents.iter().filter(|a| identity_exact(a, id)).collect();
        let matched: Vec<&RegistryAgent> = if !exact.is_empty() {
            exact
        } else {
            let prefix: Vec<&RegistryAgent> =
                agents.iter().filter(|a| identity_prefix(a, id)).collect();
            let mut ids: Vec<&str> = prefix
                .iter()
                .filter_map(|a| a.effective_identity())
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() > 1 {
                return Err(err_code::NOT_FOUND);
            }
            prefix
        };
        if matched.is_empty() {
            return Err(err_code::NOT_FOUND);
        }
        let mut panes: Vec<u64> = Vec::new();
        let mut tabs: Vec<(TabId, Option<String>)> = Vec::new();
        let mut tab_ordinals: Vec<usize> = Vec::new();
        let mut squad_id: Option<u64> = None;
        let mut squad_name: Option<String> = None;
        for a in &matched {
            let Some((sess, pane)) = &a.mux else { continue };
            if sess != &self.session_name {
                continue;
            }
            if let Some((sid, ti)) = self.session.find_pane(*pane) {
                let sq = self.session.squad(sid).expect("find_pane live");
                squad_id.get_or_insert(sid);
                if squad_name.is_none() {
                    squad_name = sq.name.clone();
                }
                panes.push(*pane);
                let t = &sq.tabs[ti];
                if !tabs.iter().any(|(tid, _)| *tid == t.id) {
                    tabs.push((t.id, t.name.clone()));
                    tab_ordinals.push(ti + 1);
                }
            }
        }
        match squad_id {
            Some(squad_id) => Ok(ServerMsg::PaneLocation {
                fno_id: id.to_string(),
                squad_id,
                squad_name,
                tabs,
                tab_ordinals: Some(tab_ordinals),
                panes,
            }),
            None => Err(err_code::NOT_PANE_HOSTED),
        }
    }

    fn pane_where_from_fresh_agents(
        &self,
        fno_id: &str,
        agents: Option<&[RegistryAgent]>,
    ) -> ServerMsg {
        let Some(rows) = agents else {
            return ServerMsg::Err {
                code: err_code::REGISTRY_UNAVAILABLE,
                msg: "agent registry unavailable".into(),
            };
        };
        match self.pane_where_with_agents(fno_id, rows) {
            Ok(location) => location,
            Err(code) => ServerMsg::Err {
                code,
                msg: format!("fno_id not located: {fno_id}"),
            },
        }
    }

    /// The reverse location lookup: resolve a location selector to
    /// the tab living there and join every pane's worker from the registry.
    /// Builds on the same ordered workspace tabs and
    /// [`Self::fno_id_for_pane_with_agents`] join `PaneList`/`PaneWhere`
    /// already use - never a second identity source. Errors are
    /// `(code, one refusal message)`; the ambiguity refusals print every
    /// candidate with workspace, label, and `tab_id` so the operator can
    /// qualify and retry.
    fn tab_where(
        &self,
        sel: &str,
        target: &PaneTarget,
        agents: &[RegistryAgent],
    ) -> Result<ServerMsg, (u32, String)> {
        let parsed = parse_loc_sel(sel).map_err(|e| (err_code::BAD_REQUEST, e))?;
        // Candidate workspaces: a qualified target names one; the default is
        // UNQUALIFIED and searches every squad.
        let squads: Vec<&Squad> = match target {
            PaneTarget::CurrentRoute => self.session.squads.iter().collect(),
            t => {
                let sid = self.resolve_squad(t)?;
                vec![self
                    .session
                    .squad(sid)
                    .ok_or((err_code::BAD_REQUEST, format!("no such squad id: {sid}")))?]
            }
        };
        // Resolve one dictionary form across the candidates. An absent form
        // is a miss in that workspace; any other refusal (a repeated name,
        // ordinal 0) is a hard error.
        let resolve_form = |form: &TabSel| -> Result<Vec<(&Squad, usize)>, (u32, String)> {
            let mut hits = Vec::new();
            for sq in &squads {
                match sq.resolve_tab(form) {
                    Ok(ti) => hits.push((*sq, ti)),
                    Err(e) if e.starts_with("no tab ") => {}
                    Err(e) => return Err((err_code::BAD_REQUEST, e)),
                }
            }
            Ok(hits)
        };
        let hits = match &parsed {
            LocSel::Ordinal(n) => {
                let hits = resolve_form(&TabSel::Index(*n))?;
                if hits.is_empty() {
                    return Err((err_code::NOT_FOUND, format!("no tab at ordinal {n}")));
                }
                hits
            }
            LocSel::Id(id) => {
                let hits = resolve_form(&TabSel::Id(*id))?;
                if hits.is_empty() {
                    return Err((err_code::NOT_FOUND, format!("no tab with id {id}")));
                }
                hits
            }
            LocSel::Name(n) => {
                let hits = resolve_form(&TabSel::Name(n.clone()))?;
                if hits.is_empty() {
                    return Err((err_code::NOT_FOUND, format!("no tab named {n}")));
                }
                hits
            }
            LocSel::Bare(n) => {
                let ordinal = usize::try_from(*n)
                    .map(|n| resolve_form(&TabSel::Index(n)))
                    .unwrap_or_else(|_| Ok(Vec::new()))?;
                let by_id = resolve_form(&TabSel::Id(*n))?;
                // Only a reading that ACTUALLY hit more than one tab can make
                // the bare number ambiguous by form; a one-sided miss falls
                // through to the generic handling below, so the refusal the
                // operator gets is one whose remediation works (qualify a
                // multi-workspace ordinal, or accept a single unambiguous
                // reading) instead of advice naming a form that matches
                // nothing.
                if ordinal.len() > 1 && by_id.is_empty() {
                    ordinal
                } else if ordinal.is_empty() && by_id.len() == 1 {
                    by_id
                } else {
                    let mut distinct: Vec<(&Squad, usize)> = ordinal.clone();
                    for h in &by_id {
                        if !distinct
                            .iter()
                            .any(|(s, ti)| s.tabs[*ti].id == h.0.tabs[h.1].id)
                        {
                            distinct.push(*h);
                        }
                    }
                    match distinct.len() {
                        0 => {
                            return Err((
                                err_code::NOT_FOUND,
                                format!("no tab at ordinal {n} and no tab with id {n}"),
                            ))
                        }
                        1 => distinct,
                        _ => {
                            let ord = ordinal
                                .first()
                                .map(|(s, ti)| self.hit_line(s, *ti))
                                .unwrap_or_default();
                            let id = by_id
                                .first()
                                .map(|(s, ti)| self.hit_line(s, *ti))
                                .unwrap_or_default();
                            return Err((
                                err_code::BAD_REQUEST,
                                format!(
                                    "bare number {n} is ambiguous: {ord} as an ordinal, {id} as \
                                     an id; use ordinal:{n} or id:{n}"
                                ),
                            ));
                        }
                    }
                }
            }
        };
        if hits.len() > 1 {
            let listed = hits
                .iter()
                .map(|(s, ti)| format!("  {}", self.hit_line(s, *ti)))
                .collect::<Vec<_>>()
                .join("\n");
            return Err((
                err_code::BAD_REQUEST,
                format!(
                    "selector {sel:?} matches {} workspaces:\n{listed}",
                    hits.len()
                ),
            ));
        }
        let (sq, ti) = hits[0];
        let Some(tab) = sq.tabs.get(ti) else {
            return Err((err_code::BAD_REQUEST, "selected tab vanished".into()));
        };
        let dict = sq
            .tab_dict(ti)
            .ok_or((err_code::BAD_REQUEST, "selected tab vanished".to_string()))?;
        let panes = tree::leaves(&tab.root)
            .into_iter()
            .map(|pid| TabPaneOccupant {
                pane_id: pid,
                fno_id: self.fno_id_for_pane_with_agents(pid, agents),
            })
            .collect();
        Ok(ServerMsg::TabLocation {
            squad_id: sq.id,
            squad_name: sq.name.clone(),
            tab_id: dict.tab_id,
            name: dict.name,
            ordinal: dict.ordinal,
            focus: tab.focus,
            panes,
        })
    }

    /// The fresh-registry wrapper for [`CoreMsg::TabWhere`]: an
    /// unreadable registry is its OWN exit class, distinct from a successful
    /// answer whose panes hold no workers.
    fn tab_where_from_fresh_agents(
        &self,
        sel: &str,
        target: &PaneTarget,
        agents: Option<&[RegistryAgent]>,
    ) -> ServerMsg {
        let Some(rows) = agents else {
            return ServerMsg::Err {
                code: err_code::REGISTRY_UNAVAILABLE,
                msg: "agent registry unavailable".into(),
            };
        };
        match self.tab_where(sel, target, rows) {
            Ok(msg) => msg,
            Err((code, msg)) => ServerMsg::Err { code, msg },
        }
    }

    /// One candidate line for a location refusal: workspace,
    /// name-or-ordinal label, stable id - everything the operator needs to
    /// qualify and retry.
    fn hit_line(&self, sq: &Squad, ti: usize) -> String {
        let tid = sq.tabs.get(ti).map(|t| t.id).unwrap_or(0);
        format!(
            "workspace={} tab={} tab_id={tid}",
            self.squad_display_label(sq),
            sq.tab_label(ti)
        )
    }

    /// The workspace's sideline label: its explicit name, else the
    /// display name derived from its origins (the same label
    /// `resolve_placement_target` matches squad names against).
    fn squad_display_label(&self, sq: &Squad) -> String {
        if let Some(n) = &sq.name {
            return n.clone();
        }
        let cwds: Vec<String> = self
            .session
            .squads
            .iter()
            .map(|s| s.canonical_cwd().to_string())
            .collect();
        let derived = squad::display_names(&cwds);
        self.session
            .squads
            .iter()
            .position(|s| s.id == sq.id)
            .and_then(|i| derived.get(i).cloned())
            .unwrap_or_default()
    }

    /// Resolve an fno session id to a single live pane it hosts in THIS session,
    /// mirroring [`Self::pane_where`]'s exact-then-unambiguous-prefix match
    /// (reuses the registry join). `None` for an id that resolves
    /// to no live, pane-hosted, in-session session - the caller demotes that
    /// slot to a shell (never a duplicate spawn of the dead session).
    fn resolve_local_pane(&self, fno_id: &str) -> Option<u64> {
        let id = fno_id.trim();
        if id.is_empty() {
            return None;
        }
        let held: Vec<u64> = self
            .held_workers
            .iter()
            .filter_map(|(pane, worker)| {
                (worker.name == id || worker.harness_session_id == id).then_some(*pane)
            })
            .collect();
        if held.len() == 1 && self.panes.contains_key(&held[0]) {
            return held.first().copied();
        }
        let exact: Vec<&RegistryAgent> = self
            .agents
            .iter()
            .filter(|a| identity_exact(a, id))
            .collect();
        let matched: Vec<&RegistryAgent> = if !exact.is_empty() {
            exact
        } else {
            let prefix: Vec<&RegistryAgent> = self
                .agents
                .iter()
                .filter(|a| identity_prefix(a, id))
                .collect();
            let mut ids: Vec<&str> = prefix
                .iter()
                .filter_map(|a| a.effective_identity())
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() > 1 {
                return None; // ambiguous prefix is never a silent first-pick
            }
            prefix
        };
        matched.iter().find_map(|a| {
            let (sess, pane) = a.mux.as_ref()?;
            (sess == &self.session_name && self.panes.contains_key(pane)).then_some(*pane)
        })
    }

    fn detached_pane_for_agent(&self, agent: &RegistryAgent) -> Option<u64> {
        let hits: Vec<u64> = self
            .detached_panes
            .iter()
            .filter(|(pane, detached)| {
                detached.matches_agent(agent)
                    && self
                        .panes
                        .get(pane)
                        .is_some_and(|entry| entry.pty.is_child_alive())
            })
            .map(|(pane, _)| *pane)
            .collect();
        match hits.as_slice() {
            [pane] => Some(*pane),
            _ => None,
        }
    }

    fn detached_pane_for_member(&self, member: &crate::squad_store::StoredMember) -> Option<u64> {
        let hits: Vec<u64> = self
            .detached_panes
            .iter()
            .filter(|(pane, detached)| {
                detached.matches_member(member)
                    && self
                        .panes
                        .get(pane)
                        .is_some_and(|entry| entry.pty.is_child_alive())
            })
            .map(|(pane, _)| *pane)
            .collect();
        match hits.as_slice() {
            [pane] => Some(*pane),
            _ => None,
        }
    }

    fn detached_pane_for_resume(
        &self,
        name: &str,
        member: Option<&crate::squad_store::StoredMember>,
    ) -> Result<Option<(u64, DetachedPane)>, String> {
        let hits: Vec<(u64, DetachedPane)> = self
            .detached_panes
            .iter()
            .filter(|(_, detached)| {
                detached.name == name && member.is_none_or(|member| detached.matches_member(member))
            })
            .map(|(pane, detached)| (*pane, detached.clone()))
            .collect();
        match hits.as_slice() {
            [] => Ok(None),
            [(pane, detached)] => Ok(Some((*pane, detached.clone()))),
            _ => Err(format!("{name} is ambiguous - use the CLI")),
        }
    }

    fn bind_worker_pane(&mut self, detached: &DetachedPane, pane: u64) {
        let panes = self.worker_pane.entry(detached.name.clone()).or_default();
        if !panes.contains(&pane) {
            panes.push(pane);
        }
        if let (Some(harness), Some(session_id)) = (
            detached.harness.as_deref(),
            detached.harness_session_id.as_deref(),
        ) {
            self.worker_session_pane
                .insert((harness.to_string(), session_id.to_string()), pane);
        }
    }

    fn persist_detached_member(&mut self, detached: &DetachedPane, is_detached: bool) {
        let members = self.squad_members.entry(detached.squad).or_default();
        if let Some(member) = members
            .iter_mut()
            .find(|member| detached.matches_member(member))
        {
            member.detached = is_detached;
            if is_detached {
                member.tombstone = false;
            }
        } else if is_detached {
            members.push(crate::squad_store::StoredMember {
                attach_id: String::new(),
                tombstone: false,
                tombstone_reason: None,
                detached: true,
                tab_name: detached.tab_name.clone(),
                cwd: (!detached.cwd.is_empty()).then(|| detached.cwd.clone()),
                worker: Some(detached.name.clone()),
                harness: detached.harness.clone(),
                harness_session_id: detached.harness_session_id.clone(),
                pane_id: None,
            });
        }
        self.persist_squad(detached.squad);
    }

    /// Create a fresh tab (one shell leaf) in squad `sid`, returning its stable
    /// id. The sid-based twin of [`Self::tab_create`] (which resolves a
    /// `PaneTarget` and returns the leaf pane); used where the squad id is
    /// already in hand (template apply / restore).
    fn create_tab_in(&mut self, sid: u64, name: Option<String>) -> Result<TabId, (u32, String)> {
        let cwd = self
            .session
            .squad(sid)
            .map(|s| s.canonical_cwd().to_string())
            .unwrap_or_default();
        let pid = self
            .spawn_pane(vt::DEFAULT_ROWS, vt::DEFAULT_COLS, &cwd)
            .map_err(|e| (err_code::SPAWN_FAILED, e))?;
        let Some(si) = self.session.squads.iter().position(|s| s.id == sid) else {
            self.reap_pane(pid);
            return Err((err_code::SPAWN_FAILED, "selected squad vanished".into()));
        };
        let tid = self.session.mint_tab_id();
        self.session.squads[si].tabs.push(Tab {
            name: clean_tab_name(name),
            id: tid,
            root: Node::Leaf(pid),
            focus: pid,
        });
        Ok(tid)
    }

    /// Realize a [`LayoutSpec`] onto a tab for [`ControlVerb::LayoutApply`]
    ///. Arity + fit are checked pre-mutation (atomic top-level `Err`);
    /// past that, bound panes relocate in place, unbound/shell slots reuse a
    /// spare shell or spawn one, and dropped shells close. A live bound pane is
    /// never killed. Serialized by the single core loop: this whole method runs
    /// as one atomic turn (no `.await`), so a concurrent apply cannot interleave
    /// - the per-(squad,tab) busy flag the design left conditional is unneeded.
    fn layout_apply(
        &mut self,
        squad: &PaneTarget,
        tab_sel: &TabSel,
        spec: &LayoutSpec,
        focus: bool,
    ) -> Result<Vec<SlotResult>, (u32, String)> {
        let sid = self.resolve_squad(squad)?;
        self.apply_spec(sid, tab_sel, spec, focus)
    }

    /// Realize an [`AnchoredLayoutSpec`] as a local subtree at `anchor`
    /// (v44). Atomic: validate -> resolve bindings -> spawn shells ->
    /// detach reused panes -> swap the candidate in, all in one serialized turn.
    /// No partial success (a refusal is a top-level `Err`); no live PTY killed.
    fn layout_graft(
        &mut self,
        squad: &PaneTarget,
        anchor: u64,
        spec: &AnchoredLayoutSpec,
        _focus: bool,
    ) -> Result<ServerMsg, (u32, String)> {
        use crate::proto::{GraftOutcome, GraftSlotResult, LayoutBinding};
        use std::collections::HashMap;

        // 1. Pure topology + slot-integrity validation, pre-mutation.
        crate::templates::validate_anchored_spec(spec)
            .map_err(|e| (err_code::BAD_REQUEST, e.to_string()))?;

        // 2. Locate the anchor; refuse stale + a conflicting workspace selector.
        let (sid, ti) = match self.session.find_pane(anchor) {
            Some(loc) => loc,
            None => {
                return Err((
                    err_code::BAD_REQUEST,
                    format!("anchor pane {anchor} no longer exists"),
                ));
            }
        };
        if !matches!(squad, PaneTarget::CurrentRoute) {
            let ok = matches!(self.resolve_placement_target(squad, None), Ok(s) if s == Some(sid));
            if !ok {
                return Err((
                    err_code::BAD_REQUEST,
                    format!("anchor pane {anchor} is not in the requested workspace"),
                ));
            }
        }
        let tid = self.session.squad(sid).expect("find_pane live").tabs[ti].id;
        let vp = self.tab_rect(tid);

        // 3. Resolve Anchor + Fno bindings to live pane ids; collect Shell slots.
        //    An unavailable Fno is REFUSED, never degraded to a shell. Duplicates
        //    (two bindings -> one pane, or Fno -> the anchor) are refused.
        let mut resolved: HashMap<String, u64> = HashMap::new();
        let mut shell_slots: Vec<String> = Vec::new();
        let mut results: Vec<GraftSlotResult> = Vec::new();
        for slot in &spec.slots {
            let (pid, outcome) = match &slot.binding {
                LayoutBinding::Anchor => (anchor, GraftOutcome::Anchor),
                LayoutBinding::Fno(id) => {
                    let p = self.resolve_local_pane(id).ok_or_else(|| {
                        (
                            err_code::BAD_REQUEST,
                            format!("fno binding {:?} has no live pane", slot.name),
                        )
                    })?;
                    (p, GraftOutcome::Fno)
                }
                LayoutBinding::Shell => {
                    shell_slots.push(slot.name.clone());
                    continue;
                }
            };
            if resolved.values().any(|&v| v == pid) {
                return Err((
                    err_code::BAD_REQUEST,
                    format!("two graft slots resolve to the same pane {pid}"),
                ));
            }
            resolved.insert(slot.name.clone(), pid);
            results.push(GraftSlotResult {
                slot: slot.name.clone(),
                pane_id: pid,
                outcome,
            });
        }

        // 4. Pre-spawn fit probe: realize the subtree with placeholder shell ids
        //    and confirm the candidate fits, so a too-small graft refuses BEFORE
        //    any shell spawns (AC4-EDGE-MINIMUM-FIT).
        let mut fit_map = resolved.clone();
        for (i, name) in shell_slots.iter().enumerate() {
            fit_map.insert(name.clone(), u64::MAX - i as u64);
        }
        let probe_tab = self.session.squad(sid).expect("live").tabs[ti].clone();
        let probe_subtree = crate::templates::realize_spec_tree(&spec.tree, &fit_map);
        if tree::replace_anchor_with_candidate(&probe_tab, vp, anchor, probe_subtree).is_err() {
            return Err((
                err_code::BAD_REQUEST,
                "graft cannot fit: a resulting pane would be below the minimum size".into(),
            ));
        }

        // 5. Spawn Shell slots AFTER pure validation. All-or-nothing: a failed
        //    spawn reaps every shell spawned so far (no detach happened yet, so
        //    rollback is shell-only - AC4-EDGE-SHELL-ROLLBACK).
        let cwd = self
            .session
            .squad(sid)
            .map(|s| s.canonical_cwd().to_string())
            .unwrap_or_default();
        let mut spawned: Vec<u64> = Vec::new();
        for name in &shell_slots {
            match self.spawn_pane(vt::DEFAULT_ROWS, vt::DEFAULT_COLS, &cwd) {
                Ok(p) => {
                    spawned.push(p);
                    resolved.insert(name.clone(), p);
                    results.push(GraftSlotResult {
                        slot: name.clone(),
                        pane_id: p,
                        outcome: GraftOutcome::Shell,
                    });
                }
                Err(_) => {
                    for p in &spawned {
                        self.reap_pane(*p);
                    }
                    return Err((
                        err_code::SPAWN_FAILED,
                        format!("graft shell slot {name:?} failed to spawn; rolled back"),
                    ));
                }
            }
        }

        // 6. Detach reused Fno panes from their current leaves (PTY kept) so the
        //    candidate's substitution is their only occurrence. The anchor stays;
        //    it is the substitution site and lives at its Anchor slot.
        for slot in &spec.slots {
            if let LayoutBinding::Fno(_) = slot.binding {
                let p = resolved[&slot.name];
                if p != anchor {
                    self.detach_pane_keep_pty(p);
                }
            }
        }

        // 7. Commit: realize the subtree with real ids and swap it in. The fit
        //    probe already passed; this re-validates on the live (post-detach)
        //    tab and swaps the candidate in one turn. Re-resolve the anchor's tab
        //    by its stable id: a detached single-pane source tab earlier in the
        //    squad shifts `ti`, so the pre-detach index can no longer name it.
        let subtree = crate::templates::realize_spec_tree(&spec.tree, &resolved);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == sid)
            .expect("squad live");
        let ti = self.session.squads[si]
            .tabs
            .iter()
            .position(|t| t.id == tid)
            .ok_or_else(|| {
                // The anchor's own tab vanished mid-turn (a concurrent close);
                // reap the shells and refuse rather than graft into the void.
                for p in &spawned {
                    self.reap_pane(*p);
                }
                (
                    err_code::BAD_REQUEST,
                    format!("anchor pane {anchor} tab vanished during graft"),
                )
            })?;
        let commit = {
            let tab = &mut self.session.squads[si].tabs[ti];
            match tree::replace_anchor_with_candidate(tab, vp, anchor, subtree) {
                Ok(node) => {
                    tab.root = node;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };
        if let Err(e) = commit {
            // A race only (the probe passed); reap the shells and refuse.
            for p in &spawned {
                self.reap_pane(*p);
            }
            return Err((
                err_code::BAD_REQUEST,
                format!("graft commit failed: {e}; rolled back spawned shells"),
            ));
        }

        self.push_layout(true);
        // Order results by the tree's slot traversal for a stable receipt.
        results.sort_by_key(|r| {
            spec.slots
                .iter()
                .position(|s| s.name == r.slot)
                .unwrap_or(usize::MAX)
        });
        let dict = self.session.squad(sid).and_then(|s| s.tab_dict(ti));
        Ok(ServerMsg::LayoutGrafted {
            anchor,
            squad: sid,
            tab: tid,
            tab_name: dict.as_ref().and_then(|d| d.name.clone()),
            tab_ordinal: dict.map(|d| d.ordinal),
            results,
        })
    }

    /// The sid-resolved core of [`Self::layout_apply`], shared with US8 restore
    /// (which already knows the squad id it just built).
    fn apply_spec(
        &mut self,
        sid: u64,
        tab_sel: &TabSel,
        spec: &LayoutSpec,
        focus: bool,
    ) -> Result<Vec<SlotResult>, (u32, String)> {
        let k = spec.slots.len();

        // 1. Topology + arity (pure, pre-mutation).
        let shape = crate::templates::topology(spec.template, k).map_err(|e| {
            let crate::templates::TemplateError::Arity {
                want,
                got,
                variadic,
            } = e;
            (
                err_code::TEMPLATE_ARITY,
                format!(
                    "template arity: want {want}{}, got {got}",
                    if variadic { "+" } else { "" }
                ),
            )
        })?;

        // 2. Resolve the target tab's viewport WITHOUT mutating yet. For an
        //    existing tab, its rect; for a New tab, a representative rect (all
        //    tabs in a squad share the client viewport) - so arity/fit are
        //    validated before any tab is spawned (an unfittable New apply is
        //    truly atomic, with no observable shell/id side effect).
        let created_new = matches!(tab_sel, TabSel::New);
        let existing_tid: Option<TabId> = if created_new {
            None
        } else {
            let ti = self
                .resolve_tab_index(sid, tab_sel)
                .map_err(|e| (err_code::BAD_REQUEST, e))?;
            Some(self.session.squad(sid).expect("resolve live").tabs[ti].id)
        };
        let vp = match existing_tid {
            Some(t) => self.tab_rect(t),
            None => self
                .session
                .squad(sid)
                .and_then(|sq| sq.tabs.first().map(|t| t.id))
                .map(|t| self.tab_rect(t))
                .unwrap_or(Rect {
                    x: 0,
                    y: 0,
                    rows: vt::DEFAULT_ROWS,
                    cols: vt::DEFAULT_COLS,
                }),
        };

        // 3. Fit (pre-mutation, atomic): any region below the minimum names the
        //    overflowing slots and refuses; nothing is mutated.
        let overflow: Vec<u64> = tree::layout(&shape, vp)
            .into_iter()
            .filter(|(_, r)| r.rows < tree::MIN_ROWS || r.cols < tree::MIN_COLS)
            .map(|(slot, _)| slot)
            .collect();
        if !overflow.is_empty() {
            let slots = overflow
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(",");
            return Err((
                err_code::TEMPLATE_UNFITTABLE,
                format!(
                    "template {:?} does not fit: slots {slots} fall below the minimum",
                    spec.template
                ),
            ));
        }

        // 4. Resolve each slot's binding to a live pane (reuse) or a shell need.
        enum Plan {
            Reuse(u64),
            Shell,   // explicit `-`
            Unbound, // an fno that resolved to no live pane -> shell, reported
        }
        let plans: Vec<Plan> = spec
            .slots
            .iter()
            .map(|b| match b {
                SlotBinding::Shell => Plan::Shell,
                SlotBinding::Fno(id) => match self.resolve_local_pane(id) {
                    Some(p) => Plan::Reuse(p),
                    None => Plan::Unbound,
                },
            })
            .collect();

        // 4a. Two slots resolving to the SAME live pane would commit a duplicate
        //     leaf (one session hosts one pane). Refuse pre-mutation, atomic.
        let reuse_set: std::collections::HashSet<u64> = plans
            .iter()
            .filter_map(|p| {
                if let Plan::Reuse(p) = p {
                    Some(*p)
                } else {
                    None
                }
            })
            .collect();
        let reuse_count = plans.iter().filter(|p| matches!(p, Plan::Reuse(_))).count();
        if reuse_set.len() != reuse_count {
            return Err((
                err_code::BAD_REQUEST,
                "two slots bind the same session (one session hosts one pane)".into(),
            ));
        }

        // 5. Materialize the target tab now that arity/fit/dup all passed, then
        //    relocate reuse panes that live in a DIFFERENT tab (a reuse pane
        //    already in the target tab stays put, reused from its old leaves).
        let target_tid = match existing_tid {
            Some(t) => t,
            None => self.create_tab_in(sid, None)?,
        };
        let target_leaves_before: Vec<u64> = {
            let ti = self.tab_index_by_id(sid, target_tid);
            tree::leaves(&self.session.squad(sid).expect("live").tabs[ti].root)
        };
        for &p in &reuse_set {
            if !target_leaves_before.contains(&p) {
                self.detach_pane_keep_pty(p);
            }
        }

        // 6. Partition the leftovers (target-tab panes the new spec does not
        //    reuse). A live bound leftover must NEVER be recycled as a shell in
        //    step 7 - it can only be rehomed in step 9 - so only genuine shells
        //    feed the recycle pool. Recycling a live pane into a Shell/Unbound
        //    slot silently absorbs a running agent under an `outcome: Shell`
        //    report; the never-kill guard in step 9 never sees it
        //    because step 7 consumed it first. Splitting up front makes the
        //    step-9 reap provably shell-only.
        //    Shells drain in tree order (FIFO) so a re-apply reassigns each shell
        //    to the SAME slot it held - the tree comes back byte-identical (AC3
        //    idempotence). A LIFO pop would reverse the shells across slots.
        let (live_leftovers, shell_pool): (Vec<u64>, Vec<u64>) = target_leaves_before
            .iter()
            .copied()
            .filter(|p| !reuse_set.contains(p))
            .partition(|&p| self.pane_hosts_live_session(p));
        let mut spare: std::collections::VecDeque<u64> = shell_pool.into();

        // 7. Assign a pane to every slot, spawning shells only after fit passed.
        let cwd = self
            .session
            .squad(sid)
            .map(|s| s.canonical_cwd().to_string())
            .unwrap_or_default();
        let mut results: Vec<SlotResult> = Vec::with_capacity(k);
        let mut filled: Vec<(usize, u64)> = Vec::with_capacity(k); // (slot idx, pane)
        for (i, plan) in plans.into_iter().enumerate() {
            let (pane, outcome) = match plan {
                Plan::Reuse(p) => (Some(p), SlotOutcome::Reused),
                Plan::Shell | Plan::Unbound => {
                    // An fno slot that reached here resolved to no live pane -> shell,
                    // reported Unbound; an explicit `-` slot is a plain Shell.
                    let is_unbound = matches!(spec.slots[i], SlotBinding::Fno(_));
                    let pane = spare.pop_front().or_else(|| {
                        self.spawn_pane(vt::DEFAULT_ROWS, vt::DEFAULT_COLS, &cwd)
                            .ok()
                    });
                    match pane {
                        Some(p) => (
                            Some(p),
                            if is_unbound {
                                SlotOutcome::Unbound
                            } else {
                                SlotOutcome::Shell
                            },
                        ),
                        None => (None, SlotOutcome::SpawnFailed),
                    }
                }
            };
            if let Some(p) = pane {
                filled.push((i, p));
            }
            results.push(SlotResult {
                slot: i as u32,
                pane_id: pane,
                outcome,
            });
        }

        // If every slot failed to obtain a pane (total PTY exhaustion), leave
        // the tab untouched rather than commit an empty tree - the per-slot
        // SpawnFailed results already tell the caller nothing landed.
        if filled.is_empty() {
            return Ok(results);
        }

        // 8. Build the committed tree. All slots filled -> the exact template.
        //    A rare shell-spawn failure -> a flat even split over the surviving
        //    panes (never an orphan, never a kill; the failed slot is reported).
        // ponytail: the flat fallback only fires on PTY exhaustion; a reduced
        // template shape is not worth the code when a plain row shows every pane.
        let new_root = if filled.len() == k {
            let map: std::collections::HashMap<u64, u64> = filled
                .iter()
                .map(|(slot, pane)| (*slot as u64, *pane))
                .collect();
            substitute_leaves(&shape, &map)
        } else {
            flat_row(&filled.iter().map(|(_, p)| *p).collect::<Vec<_>>())
        };

        // 9. Dispose of leftovers the new tree did not consume. The step-6
        //    partition guarantees `spare` is shell-only, so an unconsumed shell
        //    is a plain reap - the never-kill invariant is enforced by
        //    construction, not by a downstream check racing step-7 recycling.
        //    Live leftovers (a bound slot the new spec dropped) rehome into their
        //    own tab, never reaped - the load-bearing never-kill invariant.
        let kept: std::collections::HashSet<u64> = tree::leaves(&new_root).into_iter().collect();
        for p in spare {
            if !kept.contains(&p) {
                self.reap_pane(p);
            }
        }
        // A live leftover is in neither reuse_set nor the shell pool, so it can
        // never be a leaf of new_root; the kept check would always pass. Rehome
        // unconditionally.
        for p in live_leftovers {
            self.rehome_pane_to_new_tab(sid, p);
        }

        // 10. Commit: swap the root, keep focus unless opted in / gone. The
        //     fallback focus is the tree's FIRST leaf (deterministic = slot 0),
        //     never an arbitrary HashSet pick.
        let first_leaf = tree::leaves(&new_root).first().copied();
        let ti = self.tab_index_by_id(sid, target_tid);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == sid)
            .expect("live");
        {
            let tab = &mut self.session.squads[si].tabs[ti];
            let new_focus = if focus {
                filled.first().map(|(_, p)| *p)
            } else if kept.contains(&tab.focus) {
                Some(tab.focus)
            } else {
                None
            };
            tab.root = new_root;
            tab.focus = new_focus.or(first_leaf).unwrap_or(tab.focus);
            if let Err(e) = tree::check_invariants(tab) {
                e2e_log(format_args!("layout_apply produced an invalid tree: {e}"));
            }
        }

        // 11. Record the spec as the tab's live template + persist it (US8).
        self.template_specs.insert(target_tid, spec.clone());
        self.persist_template_specs(sid);

        self.push_layout(true);
        Ok(results)
    }

    /// The current index of the tab with id `tid` in squad `sid`. Callers hold a
    /// stable `TabId` across tab removals and re-resolve the index at each use.
    fn tab_index_by_id(&self, sid: u64, tid: TabId) -> usize {
        self.session
            .squad(sid)
            .expect("squad live")
            .tabs
            .iter()
            .position(|t| t.id == tid)
            .expect("target tab live")
    }

    /// Does pane `p` currently host a live (non-exited) session in THIS mux
    /// session? Used by the reconcile to tell a template-owned shell (safe to
    /// close) from a bound session's pane whose slot the new spec dropped (must
    /// survive - the never-kill invariant).
    fn pane_hosts_live_session(&self, p: u64) -> bool {
        self.agents.iter().any(|a| {
            !a.exited
                && matches!(&a.mux, Some((sess, pane)) if sess == &self.session_name && *pane == p)
        })
    }

    /// Break a live pane out into its own new tab in squad `sid`, keeping its
    /// PTY. The reconcile's escape hatch for a bound session whose slot a
    /// re-apply dropped: its pane keeps running in a fresh tab instead of being
    /// reaped.
    fn rehome_pane_to_new_tab(&mut self, sid: u64, p: u64) {
        let Some(si) = self.session.squads.iter().position(|s| s.id == sid) else {
            return;
        };
        let tid = self.session.mint_tab_id();
        self.session.squads[si].tabs.push(Tab {
            name: None,
            id: tid,
            root: Node::Leaf(p),
            focus: p,
        });
    }

    /// Rebuild squad `sid`'s template-managed tabs from their stored specs (US8),
    /// returning how many tabs were created. Each spec gets a fresh named tab
    /// addressed by id (so a member tab of the same name never makes the target
    /// ambiguous), then a re-apply that pulls the restored members' panes into
    /// the template topology and empties their member tabs. Per-tab failure is a
    /// notice, never a crash (restore isolation).
    fn restore_template_tabs(
        &mut self,
        sid: u64,
        specs: &[crate::squad_store::StoredTabSpec],
    ) -> usize {
        let mut created = 0;
        for st in specs {
            let tid = match self.create_tab_in(sid, Some(st.tab_name.clone())) {
                Ok(t) => t,
                Err((_, e)) => {
                    self.notice_all(format!("restore: template tab {}: {e}", st.tab_name));
                    continue;
                }
            };
            created += 1;
            if let Err((_, e)) = self.apply_spec(sid, &TabSel::Id(tid), &st.spec, false) {
                self.notice_all(format!("restore: template apply {}: {e}", st.tab_name));
            }
        }
        created
    }

    /// Drain queued US8 template restores, called on every AgentRows
    /// tick. A pending restore applies once every fno slot resolves (so live
    /// sessions bind rather than restore as shells), or after
    /// [`MAX_RESTORE_ATTEMPTS`] ticks (degrading unresolved slots to shells). On
    /// apply it removes the zero-live-member fallback tab it superseded.
    fn drain_template_restores(&mut self) {
        if self.pending_template_restores.is_empty() {
            return;
        }
        let mut keep = Vec::new();
        for mut pr in std::mem::take(&mut self.pending_template_restores) {
            pr.attempts += 1;
            let all_resolve = pr.specs.iter().all(|st| {
                st.spec.slots.iter().all(|s| match s {
                    SlotBinding::Fno(id) => self.resolve_local_pane(id).is_some(),
                    SlotBinding::Shell => true,
                })
            });
            if !all_resolve && pr.attempts < MAX_RESTORE_ATTEMPTS {
                keep.push(pr);
                continue;
            }
            let created = self.restore_template_tabs(pr.sid, &pr.specs);
            if created > 0 {
                if let Some(tid) = pr.fallback_tid {
                    self.remove_fallback_tab(pr.sid, tid);
                }
            }
        }
        self.pending_template_restores = keep;
        self.push_layout(true);
    }

    /// Remove the zero-live-member fallback shell tab once real template tabs
    /// have landed. No-op if it is the squad's only remaining tab (the
    /// >=1-tab invariant wins) or already gone.
    fn remove_fallback_tab(&mut self, sid: u64, tid: TabId) {
        let Some(sq) = self.session.squad(sid) else {
            return;
        };
        if sq.tabs.len() < 2 {
            return;
        }
        let Some(ti) = sq.tabs.iter().position(|t| t.id == tid) else {
            return;
        };
        for p in tree::leaves(&self.session.squad(sid).expect("live").tabs[ti].root) {
            self.reap_pane(p);
        }
        self.session.remove_tab(sid, ti);
        self.tab_areas.remove(&tid);
        self.reanchor_views();
    }

    /// Resolve a [`PaneTarget`] to a squad id, defaulting `CurrentRoute` to the
    /// active squad, for the tab/layout verbs.
    ///
    /// The pane-run path passes a DIFFERENT default for the same token: the
    /// squad owning the spawn's cwd (`find_by_cwd`). So `CurrentRoute` means
    /// "where the operator is looking" here and "where the work is" there. That
    /// divergence is intended - a tab verb typed by hand should act on the
    /// visible squad - but it means a caller who spawns a pane and then reads
    /// tabs unscoped gets two different squads. Scope such a read with
    /// `--workspace id:<n>` from the pane's own `squad_id` rather than relying
    /// on the default.
    fn resolve_squad(&self, target: &PaneTarget) -> Result<u64, (u32, String)> {
        self.resolve_placement_target(target, self.session.active_squad)
            .map_err(|e| (err_code::BAD_REQUEST, e))?
            .ok_or((err_code::BAD_REQUEST, "no target squad".into()))
    }

    /// A one-line refusal/notice to ONE client (BEL + transient message on
    /// its side). Errors write to the session log, never a client terminal
    /// (the compositor owns it).
    /// The sideline-attach catalog gate: is `id` a live watch-only row
    /// (paneless, not exited) whose jobId matches? Both a registry bg row and
    /// a roster-synthesized foreign row share this shape, so foreign
    /// rows attach through the existing path with no new spawn logic (AC2-HP).
    fn attachable_agent(&self, id: &str) -> bool {
        self.agents
            .iter()
            .any(|a| a.mux.is_none() && !a.exited && a.attach_id.as_deref() == Some(id))
    }

    /// Does the capability table declare a usable
    /// `interactive_resume` form for this harness: what a pane runs to bring
    /// a session back from that harness's own persisted state. Two
    /// mechanisms, one result: a claude bg session whose daemon still owns it
    /// ATTACHES (the existing `attach_id` path); everything here resumes from
    /// disk and needs no live process. A harness with no usable form offers
    /// no Resume - an honest dead row beats a button that fails. The argv
    /// itself is built later by [`resume_argv_for`] from the same declared
    /// form, so adding a harness to
    /// `cli/src/fno/agents/harness_capabilities.toml` is the whole change.
    pub(crate) fn resume_form(harness: &str) -> bool {
        declared_resume_form(harness).is_some()
    }

    /// (v53) Classify the registry facts once so resumability and the final
    /// paneless notice cannot disagree about harness/session/liveness truth.
    fn row_resume_disposition(a: &RegistryAgent) -> RowResumeDisposition {
        let Some(h) = a.harness.as_deref() else {
            return RowResumeDisposition::NoPane(AgentNoPaneReason::MissingHarness);
        };
        if !Self::resume_form(h) {
            return RowResumeDisposition::NoPane(AgentNoPaneReason::UnsupportedHarness);
        }
        let has_sid = a
            .harness_session_id
            .as_deref()
            .or(a.claude_session_uuid.as_deref())
            .is_some_and(|s| !s.is_empty());
        if !has_sid {
            return RowResumeDisposition::NoPane(AgentNoPaneReason::MissingSessionId);
        }
        // Reconcile marks a row it measured gone `orphaned`, not `exited`:
        // a positive dead reading resumes whatever the status word says.
        match (a.exited, &a.liveness) {
            (false, agents_view::Liveness::Alive) => {
                RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless)
            }
            (false, agents_view::Liveness::Unmeasured) => {
                RowResumeDisposition::NoPane(AgentNoPaneReason::LivenessUnmeasured)
            }
            _ => RowResumeDisposition::Resumable,
        }
    }

    /// The disposition with the pane join applied: a pane in THIS
    /// session whose run argv resumes the row's session id is direct
    /// observation that the backend is live, whatever the registry's
    /// liveness field says (and however stale). The row then reads
    /// LivePaneless - peek, do not resume; resuming would open a second
    /// writer on the live rollout. This join is also what removes the need
    /// for a pane-to-registry bind verb: the pane's own argv names the
    /// session the registry row carries.
    fn row_resume_disposition_in_session(&self, a: &RegistryAgent) -> RowResumeDisposition {
        if self.pane_resumes_session(a) {
            return RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless);
        }
        Self::row_resume_disposition(a)
    }

    /// Does any live pane of this session resume this row's session
    /// id? The comparison uses the same id sources the disposition does.
    /// Liveness is read from the pane, not assumed from presence: entries
    /// leave `self.panes` only on kill/reap, so a pane whose child died or
    /// whose integration markers show the resume command finished must not
    /// keep answering "backend live". A pane without markers cannot be read
    /// (`Unmeasured`): the join then keeps protecting the rollout rather
    /// than guessing the backend dead.
    fn pane_resumes_session(&self, a: &RegistryAgent) -> bool {
        if self.detached_pane_for_agent(a).is_some() {
            return true;
        }
        let sid = a
            .harness_session_id
            .as_deref()
            .or(a.claude_session_uuid.as_deref())
            .unwrap_or("");
        !sid.is_empty()
            && self.panes.iter().any(|(id, e)| {
                e.resume_target.as_deref() == Some(sid)
                    // "of THIS session" is both halves, the same pair the
                    // `live_pane` check in `agent_rows` uses: a pane can
                    // outlive its place in the layout, and one that has must
                    // not keep answering "backend live".
                    && self.session.find_pane(*id).is_some()
                    && e.pty.is_child_alive()
                    && !matches!(
                        e.vt.shell_activity(),
                        vt::ShellActivity::Idle | vt::ShellActivity::Empty
                    )
            })
    }

    /// Can this registry row be resumed into a pane? Only a DEAD
    /// row: a live session has a process writing its state (its own harness
    /// process, or claude's daemon), and resuming under it would open a
    /// second writer on the same session. A live claude bg row with a jobId
    /// is doubly excluded - its daemon owns the session, and the existing
    /// attach path is the correct gesture.
    fn row_resumable(a: &RegistryAgent) -> bool {
        matches!(
            Self::row_resume_disposition(a),
            RowResumeDisposition::Resumable
        )
    }

    /// The session-aware twin: a session a pane of this session is
    /// already resuming is never resumable again from its row.
    fn row_resumable_in_session(&self, a: &RegistryAgent) -> bool {
        matches!(
            self.row_resume_disposition_in_session(a),
            RowResumeDisposition::Resumable
        )
    }

    /// A live attachable row has a higher-priority client action, so it carries
    /// no registry refusal reason. Every other registry-backed paneless row can
    /// expose the classification that explains its branch-four notice.
    pub(crate) fn row_no_pane_reason(a: &RegistryAgent) -> Option<AgentNoPaneReason> {
        if a.attach_id.is_some() && !a.exited {
            return None;
        }
        match Self::row_resume_disposition(a) {
            RowResumeDisposition::Resumable => None,
            RowResumeDisposition::NoPane(reason) => Some(reason),
        }
    }

    /// The session-aware twin used where the sideline renders: the
    /// pane join can answer LivePaneless where the registry alone would have
    /// printed a dead-backend verdict.
    fn row_no_pane_reason_in_session(&self, a: &RegistryAgent) -> Option<AgentNoPaneReason> {
        if a.attach_id.is_some() && !a.exited {
            return None;
        }
        match self.row_resume_disposition_in_session(a) {
            RowResumeDisposition::Resumable => None,
            RowResumeDisposition::NoPane(reason) => Some(reason),
        }
    }

    /// The registry names that still exist, for restore's ghost
    /// prune. One disk read per restore; tests pin it via `set_known_workers`
    /// because the real registry is unreachable deterministically from a unit
    /// test. A read failure reads as empty (every worker member pruned with a
    /// notice) rather than freezing ghosts in place forever - fail toward the
    /// state that carries no dead weight.
    fn known_worker_names(&self) -> Option<HashSet<String>> {
        #[cfg(test)]
        if let Some(names) = KNOWN_WORKERS.with(|p| p.borrow().clone()) {
            return names;
        }
        // None = the registry could not be read (unreadable, torn write, an
        // unparseable body). The caller must then skip the prune entirely:
        // mapping a failed read to an empty set would delete EVERY worker
        // member and persist the deletion. Ghosts are cheap; deletion on a
        // transient IO error is not.
        std::fs::read_to_string(agents_view::registry_path())
            .ok()
            .and_then(|raw| agents_view::derive_rows(&raw, 0))
            .map(|rows| rows.into_iter().map(|a| a.name).collect())
    }

    fn worker_facts(a: &RegistryAgent) -> Option<HeldWorker> {
        let harness = a.harness.clone()?;
        let harness_session_id = a
            .harness_session_id
            .clone()
            .or_else(|| a.claude_session_uuid.clone())?;
        Some(HeldWorker {
            name: a.name.clone(),
            harness,
            harness_session_id,
            cwd: a.cwd.clone(),
        })
    }

    /// Resume facts from the persisted squad member itself, the identity of
    /// last resort after both the registry row and its spawn receipt are gone
    /// (a reaped row purges its receipt with it). Only a harness with a resume
    /// form qualifies: building facts for anything else would hand
    /// `resume_worker_into` a pane it cannot spawn.
    fn member_resume_facts(
        member: &crate::squad_store::StoredMember,
        worker_name: &str,
    ) -> Option<HeldWorker> {
        let harness = member.harness.as_deref()?;
        if !Self::resume_form(harness) {
            return None;
        }
        let harness_session_id = member.harness_session_id.as_deref()?;
        Some(HeldWorker {
            name: worker_name.to_string(),
            harness: harness.to_string(),
            harness_session_id: harness_session_id.to_string(),
            cwd: member.cwd.clone().unwrap_or_default(),
        })
    }

    fn write_restore_message(&mut self, pid: u64, message: &str) {
        let Some(entry) = self.panes.get_mut(&pid) else {
            return;
        };
        // Screen text only: the line is fed to the seat's VT and never
        // typed as shell input - a typed printf echoed and executed on the
        // placeholder, so the operator read the same message three times.
        let line = format!("{message}\r\n");
        entry.vt.feed(line.as_bytes());
    }

    fn hold_worker_pane(
        &mut self,
        facts: HeldWorker,
        rows: u16,
        cols: u16,
        fallback_cwd: &str,
    ) -> Result<u64, String> {
        let stored_cwd = (!facts.cwd.is_empty()).then_some(facts.cwd.as_str());
        let (cwd, _) = restore_member_cwd(stored_cwd, fallback_cwd, |path| {
            std::path::Path::new(path).is_dir()
        });
        // The placeholder's identity rides its argv (the same wrapper the
        // worker spawn path uses): every pane is keeper-hosted, so the
        // placeholder outlives this server, and the next one re-derives whose
        // seat it holds from the argv instead of minting a twin beside it.
        let pid = self.spawn_env_placeholder(
            format!("FNO_AGENT_SELF={}", facts.name),
            rows,
            cols,
            &cwd,
            "held placeholder",
        )?;
        self.write_restore_message(
            pid,
            &format!(
                "{} ({}, held across restart) - focus this pane to resume",
                facts.name, facts.harness
            ),
        );
        self.held_workers.insert(pid, facts);
        Ok(pid)
    }

    fn refused_worker_pane(
        &mut self,
        name: &str,
        reason: &str,
        rows: u16,
        cols: u16,
        cwd: &str,
    ) -> Result<u64, String> {
        // Spawn through the argv path with the refusal in an env wrapper
        // token: the pane's own argv then says what it is, so a
        // later server re-derives `refused_worker` on re-adoption and can
        // sweep a placeholder it did not mint. Stored state would go stale;
        // argv cannot. Each shell candidate takes its turn, so a broken
        // $SHELL falls through to /bin/sh the way the plain shell spawn
        // always did; the title carries the human-readable half.
        let pid = self.spawn_env_placeholder(
            format!("FNO_REFUSED_WORKER={name}"),
            rows,
            cols,
            cwd,
            "refused placeholder",
        )?;
        if let Some(entry) = self.panes.get_mut(&pid) {
            entry.name = Some(format!("{name} ({reason})"));
            entry.refused_worker = Some(name.to_string());
        }
        self.write_restore_message(pid, &format!("{name} could not be resumed: {reason}"));
        Ok(pid)
    }

    /// One resume attempt, shared by the ResumeAgent gesture and
    /// the workspace-restore driver. Everything the gesture's arm did up to
    /// the spawn lives here - the live-pane focus instead of a second
    /// writer, the stale-mapping drop, the catalog gates, the receipt
    /// fallback, the workspace resolution, the claude re-entry plan - so a
    /// bulk caller cannot drift from the single-gesture guards and grow a
    /// second-writer bug. The gesture wraps the outcome in notices and view
    /// changes; `client_id` is consulted only when the claude plan must
    /// resolve off-loop (the gesture replay path).
    ///
    /// `stored_hint` hands the driver's already-resolved member in; `None`
    /// resolves it from `name` exactly as the gesture always did.
    /// `dry_run` walks the same gates and stops where the spawn would
    /// happen, returning [`ResumeOutcome::Planned`] - the load-bearing
    /// classification behind the restore verb's `--dry-run`.
    fn resume_one(
        &mut self,
        name: &str,
        stored_hint: Option<crate::squad_store::StoredMember>,
        client_id: u64,
        view: (u64, TabId),
        dims: (u16, u16),
        dry_run: bool,
    ) -> ResumeOutcome {
        // A resume already mapped to a LIVE pane focuses it - a second
        // session on the same rollout would be a second writer. A stale
        // mapping (the pane died) is dropped and falls through to a fresh
        // resume. Same reconcile-first shape as AttachAgent.
        match self.unique_worker_pane_by_name(name) {
            Err(()) => {
                return ResumeOutcome::Refused {
                    reason: format!("{name} is ambiguous - use the CLI"),
                };
            }
            Ok(Some(mapped)) => {
                if self.panes.contains_key(&mapped) {
                    if let Some((sid, ti)) = self.session.find_pane(mapped) {
                        let tid = self.session.squad(sid).expect("live squad").tabs[ti].id;
                        return ResumeOutcome::Focused {
                            pane: mapped,
                            squad: sid,
                            tab: tid,
                        };
                    }
                } else {
                    self.worker_pane.remove(name);
                }
            }
            Ok(None) => {}
        }
        let stored_members: Vec<_> = match stored_hint {
            Some(member) => vec![member],
            None => self
                .squad_members
                .values()
                .flatten()
                .filter(|member| !member.tombstone && member.worker.as_deref() == Some(name))
                .cloned()
                .collect(),
        };
        if stored_members.len() > 1 {
            return ResumeOutcome::Refused {
                reason: format!("{name} is ambiguous - use the CLI"),
            };
        }
        let stored_member = stored_members.into_iter().next();
        match self.detached_pane_for_resume(name, stored_member.as_ref()) {
            Err(reason) => return ResumeOutcome::Refused { reason },
            Ok(Some((pane, detached))) => {
                if dry_run {
                    return ResumeOutcome::Planned;
                }
                let fallback_squad = self.session.find_by_cwd(&detached.cwd).unwrap_or(view.0);
                return match self.reattach_detached_pane(pane, fallback_squad) {
                    Ok((squad, tab)) => ResumeOutcome::Resumed {
                        pane,
                        squad,
                        tab,
                        notice: None,
                    },
                    Err(reason) => ResumeOutcome::Refused {
                        reason: format!("reattach failed: {reason}"),
                    },
                };
            }
            Ok(None) => {}
        }
        let journal = scan_spawn_journal();
        let (fresh_receipts, receipt_error) = (journal.receipts, journal.error);
        let mut row_name: Option<String> = None;
        let (facts, refusal) = {
            let candidates: Vec<&RegistryAgent> = self
                .agents
                .iter()
                .filter(|a| {
                    stored_member
                        .as_ref()
                        .map(|member| worker_registry_match(member, a, name))
                        .unwrap_or(a.name == name)
                })
                .collect();
            let a = match candidates.as_slice() {
                [] => {
                    return ResumeOutcome::Refused {
                        reason: "no such agent".into(),
                    };
                }
                [one] => *one,
                _ => {
                    return ResumeOutcome::Refused {
                        reason: format!("{name} is ambiguous - use the CLI"),
                    };
                }
            };
            let live_pane = a.mux.as_ref().is_some_and(|(_, pane)| {
                self.panes.contains_key(pane) && self.session.find_pane(*pane).is_some()
            });
            // The refusal names its evidence, never the bare line.
            let refusal = if live_pane {
                let pane = a.mux.as_ref().map(|(_, p)| *p).expect("live_pane checked");
                Some(format!("session already has a live pane {pane}"))
            } else if let RowResumeDisposition::NoPane(reason) =
                self.row_resume_disposition_in_session(a)
            {
                Some(Self::no_pane_reason_text(reason).to_string())
            } else {
                None
            };
            let facts = if refusal.is_some() {
                None // refused; reason carried in `refusal`
            } else {
                // The live registry name is the resolver's
                // key; it outranks the display name the facts carry.
                row_name = Some(a.name.clone());
                Self::worker_facts(a).or_else(|| {
                    let member = stored_member.as_ref()?;
                    receipt_for_member(&fresh_receipts, member)
                        .cloned()
                        .map(|mut receipt| {
                            receipt.name = name.to_string();
                            receipt
                        })
                })
            };
            (facts, refusal)
        };
        let Some(mut facts) = facts else {
            if let Some(error) = receipt_error {
                return ResumeOutcome::Refused { reason: error };
            }
            return ResumeOutcome::Refused {
                reason: refusal.unwrap_or_else(|| "agent is not resumable".into()),
            };
        };
        if let Some(worker) = stored_member
            .as_ref()
            .and_then(|member| member.worker.clone())
        {
            // The persisted member remains keyed by its original
            // worker name even when the registry display name changed.
            // Keep that join key while using the exact pair for lookup.
            facts.name = worker;
        }
        // The dry run stops HERE: after every gate, before the claude plan
        // resolution (a dry run never fires the off-loop resolver) and
        // before anything spawns.
        if dry_run {
            return ResumeOutcome::Planned;
        }
        // The revival gate asks BEFORE the claude plan or the codex argv
        // resolution: a refusal starts neither hop. The bulk restore
        // caller carries its own admission staging, so it skips this ask.
        if client_id != portal_reach::RESTORE_CLIENT {
            let replay_name = row_name.clone().unwrap_or_else(|| facts.name.clone());
            match self.revival_admitted(
                client_id,
                &facts,
                ResumeReplay::Gesture { name: replay_name },
            ) {
                None => return ResumeOutcome::PlanPending,
                Some(Err(reason)) => return ResumeOutcome::Refused { reason },
                Some(Ok(())) => {}
            }
        }
        let sid = self
            .squad_members
            .iter()
            .find(|(_, members)| {
                members.iter().any(|member| {
                    stored_member
                        .as_ref()
                        .is_some_and(|stored| stored == member)
                        || member.worker.as_deref() == Some(name)
                })
            })
            .map(|(sid, _)| *sid)
            .or_else(|| self.session.find_by_cwd(&facts.cwd))
            .unwrap_or(view.0);
        // A claude row's resume runs the canonical re-entry
        // plan; the `None` arm fires the off-loop resolution and the
        // gesture replays with the verdict staged. A receipt-only row
        // (no registry row) has no recorded binding, so its name
        // misses the resolver and the visible refusal is the design -
        // no bare claude resume on this axis.
        let plan;
        let staged_argv;
        if facts.harness == "claude" {
            let name = row_name.unwrap_or_else(|| facts.name.clone());
            let Some(verdict) = self.resume_gesture_plan(
                client_id,
                &name,
                ReentrySpawnRequest::Resume { name: name.clone() },
            ) else {
                return ResumeOutcome::PlanPending;
            };
            plan = Some(verdict);
            staged_argv = None;
        } else {
            // The argv (codex grant + --cd) resolves off-loop. If
            // nothing is staged, fire the resolution and stop: the
            // `ResumeArgvReady` replay re-dispatches this gesture.
            match self.staged_resume_argv.take() {
                Some(argv) => staged_argv = Some(argv),
                None => {
                    let replay_name = row_name.unwrap_or_else(|| facts.name.clone());
                    let stored_cwd = (!facts.cwd.is_empty()).then_some(facts.cwd.as_str());
                    let (spawn_cwd, gone) = self.member_resume_cwd(sid, stored_cwd);
                    self.resolve_resume_argv(
                        client_id,
                        facts.harness.as_str(),
                        &facts.harness_session_id,
                        &spawn_cwd,
                        gone.is_none(),
                        ResumeReplay::Gesture { name: replay_name },
                    );
                    return ResumeOutcome::PlanPending;
                }
            }
            plan = None;
        }
        let (pid, tid, fallback_notice) = match self.resume_worker_into(
            &facts,
            sid,
            None,
            dims.0,
            dims.1,
            plan.as_ref(),
            staged_argv.as_deref(),
        ) {
            Ok(result) => result,
            Err(error) => {
                return ResumeOutcome::Refused {
                    reason: format!("resume failed: {error}"),
                };
            }
        };
        ResumeOutcome::Resumed {
            pane: pid,
            squad: sid,
            tab: tid,
            notice: fallback_notice,
        }
    }

    /// The live, non-tombstoned worker members a restore acts on,
    /// as (worker name, member) pairs in stored order. `harness` narrows the
    /// run to one harness's members. A member with no worker name records no
    /// resumable identity, so it is never a candidate. The same
    /// classifier the startup loop answers to drops the Gone members here,
    /// so the verb and the startup path can never disagree about who is a
    /// corpse.
    fn restore_candidates(
        &self,
        harness: Option<&str>,
    ) -> Vec<(String, crate::squad_store::StoredMember)> {
        let known_workers = self.known_worker_names();
        let receipts = crate::spawn_journal::scan_spawn_journal().receipts;
        self.squad_members
            .values()
            .flatten()
            .filter(|m| !m.tombstone)
            .filter(|m| m.worker.as_deref().is_some_and(|w| !w.trim().is_empty()))
            .filter(|m| harness.is_none_or(|h| m.harness.as_deref() == Some(h)))
            .filter(|m| {
                classify_member(m, known_workers.as_ref(), &receipts) != MemberVerdict::Gone
            })
            .map(|m| (m.worker.clone().expect("checked above"), m.clone()))
            .collect()
    }

    /// The directory a resumed member spawns at, and the missing
    /// recorded directory when it is gone. Extracted from
    /// [`Core::resume_worker_into`] so the off-loop argv resolution grants
    /// the SAME directory the spawn will use.
    fn member_resume_cwd(&self, sid: u64, stored_cwd: Option<&str>) -> (String, Option<String>) {
        let fallback_cwd = self
            .session
            .squad(sid)
            .map(|s| s.canonical_cwd().to_string())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| h.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        restore_member_cwd(stored_cwd, &fallback_cwd, |path| {
            std::path::Path::new(path).is_dir()
        })
    }

    fn resume_worker_into(
        &mut self,
        facts: &HeldWorker,
        sid: u64,
        replace: Option<u64>,
        rows: u16,
        cols: u16,
        plan: Option<&ReentryVerdict>,
        staged_argv: Option<&[String]>,
    ) -> Result<(u64, TabId, Option<String>), String> {
        // Fail closed: the one spawn site spawns only behind a staged
        // revival admission, so a future caller cannot skip the gate.
        self.take_revival_admission(&facts.name)?;
        if !Self::resume_form(&facts.harness) {
            return Err("agent harness has no resume form".into());
        }
        let stored_cwd = (!facts.cwd.is_empty()).then_some(facts.cwd.as_str());
        let (spawn_cwd, gone) = self.member_resume_cwd(sid, stored_cwd);
        let fallback_notice = gone.map(|missing| {
            format!(
                "resume: {}'s directory {missing} is gone; resuming at {spawn_cwd}",
                facts.name
            )
        });
        // A staged re-entry verdict replaces the bare provider argv;
        // its `env` prefix carries the row's recorded account context. A
        // non-claude row runs the argv the off-loop resume-argv resolution
        // staged (: the codex grant + --cd ride it); without one it
        // resumes exactly as before (the declared-form render, which is also
        // the fail-open fallback the resolution stages on failure).
        let argv = match plan {
            Some(verdict) => verdict.prefixed_argv(),
            None => match staged_argv {
                Some(argv) => argv.to_vec(),
                None => resume_argv_for(&facts.harness, &facts.harness_session_id)?,
            },
        };
        // Unit fixtures replace the provider with short-lived `/bin/cat`; it
        // can exit before a keeper answers Identify. Production resumes use
        // the keeper so the worker has the same ownership contract as a
        // `pane run --worker` launch.
        #[cfg(test)]
        let pid = self.spawn_pane_cmd(&argv, rows, cols, &spawn_cwd)?;
        #[cfg(not(test))]
        let pid = {
            let permit = crate::process_admission::admit_fleet().map_err(|e| e.to_string())?;
            self.spawn_pane_shell_with_permit(&argv, rows, cols, &spawn_cwd, permit, true)?
        };
        if let Some(entry) = self.panes.get_mut(&pid) {
            entry.name = Some(facts.name.clone());
        }
        let tid = if let Some(old) = replace {
            let Some((found_sid, ti)) = self.session.find_pane(old) else {
                self.reap_pane(pid);
                return Err("held pane vanished".into());
            };
            if found_sid != sid {
                self.reap_pane(pid);
                return Err("held pane moved to another workspace".into());
            }
            let tab = &mut self.session.squad_mut(sid).expect("live squad").tabs[ti];
            let tid = tab.id;
            if !tree::replace_leaf(tab, old, pid) {
                self.reap_pane(pid);
                return Err("held pane slot vanished".into());
            }
            self.reap_pane(old);
            tid
        } else {
            let tid = self.session.mint_tab_id();
            let Some(squad) = self.session.squad_mut(sid) else {
                self.reap_pane(pid);
                return Err("target workspace vanished".into());
            };
            squad.tabs.push(Tab {
                name: None,
                id: tid,
                root: Node::Leaf(pid),
                focus: pid,
            });
            tid
        };
        self.worker_pane
            .entry(facts.name.clone())
            .or_default()
            .push(pid);
        self.worker_session_pane.insert(
            (facts.harness.clone(), facts.harness_session_id.clone()),
            pid,
        );
        self.record_worker_member(
            sid,
            &facts.name,
            pid,
            &spawn_cwd,
            Some(&facts.harness_session_id),
        );
        Ok((pid, tid, fallback_notice))
    }

    // ---- Persisted named squads --------------------------------

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

    fn snapshot_squad(&mut self, sid: u64) -> Option<SquadSnapshot> {
        let Some(sq) = self.session.squad(sid) else {
            return None;
        };
        let name = sq.name.clone().unwrap_or_default();
        let mut key = sq.key.clone();
        let origins = sq.origins.clone();
        if name.is_empty() && key.is_empty() {
            // First persist of an unnamed squad: derive its durable identity from
            // its origins when it has any (stable across restarts, so a repo's
            // home squad upserts onto one row forever -), else mint a
            // random key (an originless squad has no stable identity). Recording
            // it on the live squad makes every later persist in this process
            // reuse it.
            key = if origins.is_empty() {
                crate::squad_store::mint_key()
            } else {
                crate::squad_store::origin_key(&origins)
            };
            if let Some(s) = self.session.squad_mut(sid) {
                s.key = key.clone();
            }
        }
        // A pre-restore bootstrap shell must not overwrite an existing
        // same-origin squad that contains keeper-backed members.
        if self.shared_identity_write_skipped(sid, &name, &key)
            || !self.restored
                && self.pre_restore_squads.contains(&sid)
                && self.squad_members.get(&sid).is_some_and(Vec::is_empty)
                && crate::squad_store::load().squads.iter().any(|stored| {
                    stored.name == name && stored.key == key && stored.origins == origins
                })
        {
            return None;
        }
        // (US4) Re-derive each member's hosting tab name and write it back
        // into the AUTHORITATIVE in-memory list, not just the store copy. Other
        // write paths (RenameSquad, a churned member's `persist_stored`) persist
        // `squad_members` verbatim; refreshing here keeps them from erasing a
        // freshly-renamed tab name on the next write (codex review). A tombstone
        // (no live pane) resolves to None.
        let member_panes: Vec<Option<u64>> = self
            .squad_members
            .get(&sid)
            .map(|members| {
                members
                    .iter()
                    .map(|member| self.member_pane(member))
                    .collect()
            })
            .unwrap_or_default();
        let names: Vec<Option<Option<String>>> = member_panes
            .iter()
            .map(|pane| pane.and_then(|pid| self.pane_tab_name(sid, pid)))
            .collect();
        if let Some(list) = self.squad_members.get_mut(&sid) {
            for (m, resolved) in list.iter_mut().zip(names) {
                // Only overwrite when the member's pane resolved to a tab
                // (Some(name_opt)): Some -> named, None -> the tab is unnamed, so
                // a blank rename clears. An UNRESOLVABLE pane (a tombstone or a
                // transient restore reattach failure, `None`) PRESERVES the last
                // stored name so a temporary miss never erases it (codex review).
                if let Some(tab_name) = resolved {
                    m.tab_name = tab_name;
                }
            }
            // Re-derive each live member's pane cwd the same way: only
            // a resolvable live pane overwrites, so a tombstone or a transient
            // miss keeps the last-known cwd rather than erasing it. This is what
            // lets restore spawn a worktree worker back into its own worktree
            // instead of the squad's `origins[0]` (server.rs restore_squads).
            // The birth pane id rides the same resolve: only a resolvable live
            // pane overwrites, so a miss keeps the last stored id.
            for (m, pane) in list.iter_mut().zip(member_panes) {
                if let Some(pid) = pane.filter(|pid| self.panes.contains_key(pid)) {
                    m.pane_id = Some(pid);
                }
                if let Some(cwd) = pane
                    .and_then(|pid| self.panes.get(&pid))
                    .map(|p| p.cwd.clone())
                    .filter(|c| !c.is_empty())
                {
                    m.cwd = Some(cwd);
                }
            }
            // Re-derive the complete worker identity on every
            // topology flush. A registry row can publish after placement, so
            // a transient miss preserves the last durable pair rather than
            // erasing it; a hit always replaces both fields authoritatively.
            let worker_member_counts: HashMap<String, usize> = list
                .iter()
                .filter_map(|member| member.worker.clone())
                .fold(HashMap::new(), |mut counts, worker| {
                    *counts.entry(worker).or_default() += 1;
                    counts
                });
            for member in list.iter_mut() {
                let Some(worker) = member.worker.as_deref() else {
                    continue;
                };
                let named_rows: Vec<&RegistryAgent> = self
                    .agents
                    .iter()
                    .filter(|agent| agent.name == worker)
                    .collect();
                let row = match (
                    member.harness.as_deref(),
                    member.harness_session_id.as_deref(),
                ) {
                    (Some(harness), Some(session_id)) => named_rows.iter().copied().find(|agent| {
                        agent.harness.as_deref() == Some(harness)
                            && agent_harness_session_id(agent) == Some(session_id)
                    }),
                    (None, None)
                        if worker_member_counts.get(worker) == Some(&1)
                            && named_rows.len() == 1 =>
                    {
                        named_rows.first().copied()
                    }
                    _ => None,
                };
                if let Some(facts) = row.and_then(Self::worker_facts) {
                    member.harness = Some(facts.harness);
                    member.harness_session_id = Some(facts.harness_session_id);
                }
            }
        }
        let members = self.squad_members.get(&sid).cloned().unwrap_or_default();
        let (tab_trees, active_tab) = self.stored_tab_trees(sid)?;
        Some(SquadSnapshot {
            name,
            key,
            origins,
            members,
            tab_trees,
            active_tab: Some(active_tab),
        })
    }

    /// Seat `adopted` (a keeper shell re-adopted at its birth id) into the
    /// home tab's leaf in place of the just-spawned attach stand-in, which is
    /// then reaped: it was born this attach, holds nothing, and leaving both
    /// alive would accrete a shell tab on every restart.
    fn reclaim_home_shell(&mut self, home_sid: u64, adopted: u64) {
        let fresh = self
            .session
            .squad(home_sid)
            .and_then(|sq| sq.tabs.first())
            .and_then(|tab| tree::leaves(&tab.root).first().copied());
        let Some(fresh) = fresh else {
            return;
        };
        if fresh == adopted {
            return;
        }
        if let Some(tab) = self
            .session
            .squad_mut(home_sid)
            .and_then(|sq| sq.tabs.first_mut())
        {
            subtree_swap(&mut tab.root, fresh, adopted);
            if tab.focus == fresh {
                tab.focus = adopted;
            }
        }
        self.reap_pane(fresh);
    }

    /// Capture squad `sid`'s whole tab topology into store shape -
    /// EVERY tab, hand-split and template alike, ending the three gates
    /// (template-only, named-squad-only, named-tab-only) that left the
    /// operator's real layouts unpersisted. `None` when the squad is gone.
    fn stored_tab_trees(&self, sid: u64) -> Option<(Vec<StoredTabTree>, usize)> {
        let sq = self.session.squad(sid)?;
        // Reverse the attach join so a pane with an fno id names its slot that
        // id (stable across restarts); anything else is an ordinal shell.
        let mut pane_owner_names: HashMap<u64, String> = self
            .attached
            .iter()
            .map(|(id, pane)| (*pane, id.clone()))
            .collect();
        if let Some(members) = self.squad_members.get(&sid) {
            for member in members {
                if let (Some(pane), Some(binding)) =
                    (self.member_pane(member), worker_binding_key(member))
                {
                    pane_owner_names.insert(pane, binding);
                }
            }
        }
        let pane_owner: HashMap<u64, &str> = pane_owner_names
            .iter()
            .map(|(pane, name)| (*pane, name.as_str()))
            .collect();
        // Every live portal seat -> (index, row_key), collected once.
        // A seat is CAPTURED now, not pruned: the slot keeps the pair restore
        // needs to hold that seat again, and a tab holding only portals is a
        // tab like any other.
        let portal_seats: HashMap<u64, (u8, String)> = self
            .portals
            .iter()
            .map(|(idx, p)| (p.seat, (*idx, p.row_key.clone())))
            .collect();
        // Filled per tab below.
        let mut pane_cwd: HashMap<u64, String> = HashMap::new();
        let mut trees = Vec::with_capacity(sq.tabs.len());
        let mut active_tab = 0;
        for (i, t) in sq.tabs.iter().enumerate() {
            if i == sq.active_tab {
                active_tab = trees.len();
            }
            let root = &t.root;
            let cwd_of = |p| {
                self.panes
                    .get(&p)
                    .map(|e| (e.pty.child_pid(), e.cwd.as_str()))
            };
            crate::pane_cwd::fill_leaf_cwds(tree::leaves(root), cwd_of, &mut pane_cwd);
            let mut capture = SlotCapture::new(
                &pane_owner,
                &pane_cwd,
                &portal_seats,
                &self.portals,
                &self.agents,
            );
            let tree = capture.node_to_spec(root);
            let focus = capture.slot_of(t.focus);
            trees.push(StoredTabTree {
                tab_name: t.name.clone(),
                tree,
                slots: capture.slots,
                focus,
            });
        }
        Some((trees, active_tab))
    }

    /// How long a topology mutation stays dirty before the tick flushes it.
    /// One write per gesture, not one per drag event.
    const TOPOLOGY_DEBOUNCE: Duration = Duration::from_secs(2);

    /// How long after a grid-changing resize the deferred repaint
    /// request waits before firing: past the child's own SIGWINCH repaint,
    /// short enough to feel instant. Fired by the 1s core tick, so the real
    /// delay is this floor plus up to one tick.
    const NUDGE_DELAY: Duration = Duration::from_millis(300);

    /// Fire every due deferred repaint request (the 1s core tick's pass).
    /// Re-reads each pane's CURRENT requested size (not `vt.size()`: a
    /// keeper-hosted pane's `vt` only catches up once its resize ack lands,
    /// so reading `vt.size()` here could still see the OLD size and nudge
    /// the pty right back to it), so a nudge armed by an older geometry
    /// never re-introduces a stale winsize.
    fn fire_due_nudges(&mut self) {
        let due: Vec<u64> = self
            .panes
            .iter()
            .filter_map(|(pid, e)| {
                (e.nudge_due.is_some_and(|t| Instant::now() >= t)).then_some(*pid)
            })
            .collect();
        for pid in due {
            if let Some(entry) = self.panes.get_mut(&pid) {
                entry.nudge_due = None;
                let (rows, cols) = entry.requested_size;
                entry.pty.nudge_winch(rows, cols);
                e2e_log(format_args!(
                    "resize repaint nudge fired for pane {pid} at {rows}x{cols}"
                ));
            }
        }
    }

    /// Mark the topology dirty (called from every `push_layout(true)`, the one
    /// funnel every layout mutation crosses) and flush immediately when the
    /// debounce window is already past, so an isolated mutation does not wait
    /// for the next tick.
    ///
    /// A no-op before `self.restored` flips true: `attach()`'s FIRST
    /// `push_layout(true)` (server.rs, the freshly-minted home squad's own
    /// initial layout push) fires before `restore_squads` runs a few lines
    /// later in the same call. Capturing there would persist the brand-new
    /// squad, and `restore_squads` would then read its own just-written row
    /// back as a "restored" lane matching this squad's origin and home-merge
    /// a second pane into it - the session observing its own bootstrap as a
    /// restart. Once `self.restored` is true this can never happen again (the
    /// gate is one-shot for the server's lifetime), so every later mutation
    /// captures exactly as before.
    fn mark_topology_dirty(&mut self) {
        if !self.restored {
            return;
        }
        self.topology_dirty = true;
        let due = self
            .last_topology_flush
            .is_none_or(|t| t.elapsed() >= Self::TOPOLOGY_DEBOUNCE);
        if due {
            self.flush_topology();
        }
    }

    /// Write every dirty persistable squad through `persist_squad`, keeping
    /// membership identity and topology keys on one path.
    fn flush_topology(&mut self) {
        if !self.topology_dirty {
            return;
        }
        self.topology_dirty = false;
        self.last_topology_flush = Some(Instant::now());
        let sids: Vec<u64> = self.session.squads.iter().map(|s| s.id).collect();
        for sid in sids {
            self.persist_squad(sid);
        }
    }

    /// Persist a just-attached session as a member of squad `sid` (idempotent) -
    /// the same write recruit and restore already do - so restore rebuilds its
    /// pane and its row renders pane-hosted (click == focus) after a restart.
    /// A tombstoned prior membership is revived (the session is live again).
    /// Call only AFTER placement succeeded and `attached` holds the mapping;
    /// `persist_squad` keys it by name when named, else origins, and skips a
    /// squad with neither (harmless).
    fn persist_attached_member(&mut self, sid: u64, id: &str) {
        let members = self.squad_members.entry(sid).or_default();
        match members.iter_mut().find(|m| m.attach_id == id) {
            Some(m) if m.tombstone => m.tombstone = false,
            Some(_) => return, // already a live member - no duplicate (AC1-EDGE)
            None => members.push(crate::squad_store::StoredMember {
                attach_id: id.to_string(),
                tombstone: false,
                tombstone_reason: None,
                detached: false,
                tab_name: None,
                cwd: None,
                worker: None,
                harness: None,
                harness_session_id: None,
                pane_id: None,
            }),
        }
        self.persist_squad(sid);
    }

    /// Record a worker pane as a member of squad `sid`, keyed by its
    /// registry NAME rather than a claude jobId - the capture funnel
    /// `persist_attached_member` never covered, which is why squads full of
    /// codex/agy workers persisted with `members: []`. Called from `run_pane`
    /// on the `--worker` path, the one server-side operation every pane
    /// producer crosses (mux_spawn.py's spawn lane and the TUI's dispatch
    /// card both shell into it), so the guard cannot be bypassed by a caller.
    /// Idempotent by name; a tombstoned prior membership is revived. Restore
    /// never respawns this member - it renders idle and resumes through the
    /// harness's own form (`Command::ResumeAgent`).
    fn record_worker_member(
        &mut self,
        sid: u64,
        name: &str,
        pid: u64,
        cwd: &str,
        persisted_session_id: Option<&str>,
    ) {
        if let Some(entry) = self.panes.get_mut(&pid) {
            entry.name = Some(name.to_string());
        }
        // The pane's hosting tab name at record time, the pid-based twin of
        // `member_tab_name` (which joins through `attached`, a claude-id map a
        // worker pane never enters). persist_squad's re-derivation misses
        // worker members too, so this stored value is the durable one.
        let tab_name = self
            .session
            .squad(sid)
            .and_then(|sq| {
                sq.tabs
                    .iter()
                    .find(|t| tree::leaves(&t.root).contains(&pid))
                    .map(|t| t.name.clone())
            })
            .flatten();
        let (named_rows_len, facts) = {
            let named_rows: Vec<&RegistryAgent> =
                self.agents.iter().filter(|a| a.name == name).collect();
            let facts = match persisted_session_id {
                Some(session_id) => {
                    let pair_rows: Vec<&RegistryAgent> = named_rows
                        .iter()
                        .copied()
                        .filter(|a| agent_harness_session_id(a) == Some(session_id))
                        .collect();
                    match pair_rows.as_slice() {
                        [one] => Self::worker_facts(one),
                        _ => None,
                    }
                }
                None => match named_rows.as_slice() {
                    [one] => Self::worker_facts(one),
                    _ => None,
                },
            };
            (named_rows.len(), facts)
        };
        let harness = facts.as_ref().map(|facts| facts.harness.clone());
        let harness_session_id = persisted_session_id
            .map(str::to_string)
            .or_else(|| facts.as_ref().map(|facts| facts.harness_session_id.clone()));
        let detached = DetachedPane {
            name: name.to_string(),
            harness: harness.clone(),
            harness_session_id: harness_session_id.clone(),
            cwd: cwd.to_string(),
            squad: sid,
            squad_name: String::new(),
            squad_key: String::new(),
            origins: Vec::new(),
            tab_name: tab_name.clone(),
        };
        self.bind_worker_pane(&detached, pid);
        let members = self.squad_members.entry(sid).or_default();
        let exact_identity = facts
            .as_ref()
            .map(|facts| (facts.harness.as_str(), facts.harness_session_id.as_str()));
        let existing = exact_identity
            .and_then(|(harness, session_id)| {
                members.iter().position(|member| {
                    member.worker.as_deref() == Some(name)
                        && member.harness.as_deref() == Some(harness)
                        && member.harness_session_id.as_deref() == Some(session_id)
                })
            })
            .or_else(|| {
                if named_rows_len > 1 {
                    return None;
                }
                let candidates: Vec<usize> = members
                    .iter()
                    .enumerate()
                    .filter_map(|(index, member)| {
                        (member.worker.as_deref() == Some(name)).then_some(index)
                    })
                    .collect();
                let [index] = candidates.as_slice() else {
                    return None;
                };
                let member = &members[*index];
                if exact_identity.is_some()
                    && (member.harness.is_some() || member.harness_session_id.is_some())
                {
                    return None;
                }
                if let Some(session_id) = persisted_session_id {
                    if member
                        .harness_session_id
                        .as_deref()
                        .is_some_and(|existing| existing != session_id)
                    {
                        return None;
                    }
                }
                Some(*index)
            });
        match existing.map(|index| &mut members[index]) {
            Some(m) if m.tombstone => {
                m.tombstone = false;
                m.detached = false;
                m.tab_name = tab_name;
                m.cwd = (!cwd.is_empty()).then(|| cwd.to_string());
                m.harness = harness.clone().or(m.harness.clone());
                m.harness_session_id = harness_session_id.or(m.harness_session_id.clone());
            }
            Some(m) => {
                m.detached = false;
                m.harness = harness.clone().or(m.harness.clone());
                m.harness_session_id = harness_session_id.or(m.harness_session_id.clone());
            }
            None => {
                let cwd = (!cwd.is_empty()).then(|| cwd.to_string());
                members.push(crate::squad_store::StoredMember {
                    attach_id: String::new(),
                    tombstone: false,
                    tombstone_reason: None,
                    detached: false,
                    tab_name,
                    cwd,
                    worker: Some(name.to_string()),
                    harness,
                    harness_session_id,
                    pane_id: None,
                });
            }
        }
        self.persist_squad(sid);
    }

    /// Refresh a tracked workspace's persisted member `tab_name`s after
    /// a tree op that relocated a member's hosting tab (break / join). Only fires
    /// when the squad actually has recruited members, so it never newly persists
    /// an unnamed or member-less squad; `persist_squad` then re-derives each
    /// member's tab from the live tree.
    fn persist_squad_if_members(&mut self, sid: u64) {
        if self.squad_members.get(&sid).is_some_and(|m| !m.is_empty()) {
            self.persist_squad(sid);
        }
    }

    /// Resolve the pane hosting a squad member: the persisted birth pane id
    /// while it is still live, else the derived worker joins below.
    fn member_pane(&self, member: &crate::squad_store::StoredMember) -> Option<u64> {
        if let Some(pane) = member.pane_id.filter(|p| self.panes.contains_key(p)) {
            return Some(pane);
        }
        if let Some(worker) = member.worker.as_deref() {
            if let Some(detached) = self.detached_pane_for_member(member) {
                return Some(detached);
            }
            if let (Some(harness), Some(session_id)) = (
                member.harness.as_deref(),
                member.harness_session_id.as_deref(),
            ) {
                return self
                    .worker_session_pane
                    .get(&(harness.to_string(), session_id.to_string()))
                    .copied()
                    .or_else(|| {
                        let matches: Vec<u64> = self
                            .held_workers
                            .iter()
                            .filter(|(_, held)| {
                                held.harness == harness && held.harness_session_id == session_id
                            })
                            .map(|(pane, _)| *pane)
                            .collect();
                        matches
                            .as_slice()
                            .first()
                            .copied()
                            .filter(|_| matches.len() == 1)
                    });
            }
            return self
                .unique_worker_pane_by_name(worker)
                .ok()
                .flatten()
                .or_else(|| {
                    let matches: Vec<u64> = self
                        .held_workers
                        .iter()
                        .filter(|(_, held)| held.name == worker)
                        .map(|(pane, _)| *pane)
                        .collect();
                    matches
                        .as_slice()
                        .first()
                        .copied()
                        .filter(|_| matches.len() == 1)
                });
        }
        self.attached.get(&member.attach_id).copied()
    }

    fn pane_tab_name(&self, sid: u64, pid: u64) -> Option<Option<String>> {
        let sq = self.session.squad(sid)?;
        let tab = sq
            .tabs
            .iter()
            .find(|t| tree::leaves(&t.root).contains(&pid))?;
        Some(tab.name.clone())
    }

    fn worker_pane_for_agent(&self, agent: &RegistryAgent) -> Option<u64> {
        if let (Some(harness), Some(session_id)) =
            (agent.harness.as_deref(), agent_harness_session_id(agent))
        {
            return self
                .worker_session_pane
                .get(&(harness.to_string(), session_id.to_string()))
                .copied();
        }
        self.worker_pane
            .get(&agent.name)
            .and_then(|panes| (panes.len() == 1).then_some(panes[0]))
    }

    fn member_squad_for_agent(&self, agent: &RegistryAgent) -> Option<u64> {
        let exact: Vec<u64> = match (agent.harness.as_deref(), agent_harness_session_id(agent)) {
            (Some(harness), Some(session_id)) => self
                .squad_members
                .iter()
                .filter_map(|(sid, members)| {
                    members
                        .iter()
                        .any(|member| {
                            member.worker.as_deref() == Some(agent.name.as_str())
                                && member.harness.as_deref() == Some(harness)
                                && member.harness_session_id.as_deref() == Some(session_id)
                        })
                        .then_some(*sid)
                })
                .collect(),
            _ => Vec::new(),
        };
        if exact.len() == 1 {
            return exact.first().copied();
        }
        if exact.len() > 1 {
            return None;
        }
        let legacy: Vec<u64> = self
            .squad_members
            .iter()
            .filter_map(|(sid, members)| {
                members
                    .iter()
                    .any(|member| {
                        member.worker.as_deref() == Some(agent.name.as_str())
                            && member.harness.is_none()
                            && member.harness_session_id.is_none()
                    })
                    .then_some(*sid)
            })
            .collect();
        (legacy.len() == 1).then(|| legacy[0])
    }

    fn unique_worker_pane_by_name(&self, name: &str) -> Result<Option<u64>, ()> {
        match self.worker_pane.get(name) {
            None => Ok(None),
            Some(panes) if panes.len() == 1 => Ok(Some(panes[0])),
            Some(_) => Err(()),
        }
    }

    /// The store identity of a live squad: `(name, key)`, `name` empty for an
    /// unnamed one. Captured BEFORE a mutation that may remove the squad, so the
    /// de-persist has something to key on afterwards.
    fn squad_identity(&self, sid: u64) -> Option<(String, String)> {
        let sq = self.session.squad(sid)?;
        Some((sq.name.clone().unwrap_or_default(), sq.key.clone()))
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
    /// (the reap clears `attached` and the tree). `(squad id, name, key, origins,
    /// attach_id)`, or `None` when the pane is not a member of a tracked squad.
    /// `name` is empty for an unnamed squad, whose store identity is the durable
    /// `key`; members persist too now.
    #[allow(clippy::type_complexity)]
    fn member_ctx(&self, pid: u64) -> Option<(u64, String, String, Vec<String>, String)> {
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
        // Unnamed squads persist too (keyed by the durable `key`); an empty name
        // is the unnamed sentinel, not a "skip" - so a picker-attached member of
        // the home squad / a lane still reconciles its close.
        let name = sq.name.clone().unwrap_or_default();
        Some((sid, name, sq.key.clone(), sq.origins.clone(), attach_id))
    }

    /// Broadcast a one-line notice to every attached client (restore + degraded
    /// paths that are not scoped to one sender). Reports whether ANY client
    /// received it, so a caller latching on the broadcast knows the message
    /// actually landed: a latch set on an empty room burns a
    /// once-per-lifetime notice on nobody.
    pub(crate) fn notice_all(&self, text: impl Into<String>) -> bool {
        let text = text.into();
        let mut delivered = false;
        for c in &self.clients {
            if c.reliable_tx
                .try_send(ServerMsg::Notice { text: text.clone() })
                .is_ok()
            {
                delivered = true;
            }
        }
        delivered
    }

    /// The birth account + isolated `config_dir` for a to-be-attached
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

    /// Stamp a freshly-attached pane's registered worker name so its
    /// tab/pane title matches the sidepane row, not the `claude` command basename.
    /// The attach argv is `claude attach <id>` with no `FNO_AGENT_SELF`, so unlike
    /// a fresh spawn the name is not in the argv - resolve it from the live catalog
    /// (the sidepane's in-memory source) and fall back to the roster when the
    /// catalog is empty (cold restore). `None` (an ad-hoc attach, or a worker not
    /// yet known) leaves the field as the spawn set it, so the label falls through.
    fn name_attached_pane(
        &mut self,
        pid: u64,
        attach_id: &str,
        config_dir: Option<&std::path::Path>,
    ) {
        let name = self
            .agents
            .iter()
            .find(|a| a.attach_id.as_deref() == Some(attach_id))
            .map(|a| a.name.clone())
            .or_else(|| agents_view::registered_name_for(attach_id, config_dir));
        if let (Some(name), Some(entry)) = (name, self.panes.get_mut(&pid)) {
            entry.name = Some(name);
        }
    }

    /// The attach-ids that are LIVE right now, read synchronously from the
    /// registry + roster files. Restore runs at the first attach, before the
    /// off-loop 1s reader has populated `self.agents`, so a stale in-memory
    /// catalog would tombstone every member (AC1-HP). One-shot read per server
    /// lifetime, off the steady loop. Delegates to [`live_attach_ids_snapshot`]
    /// so the mux squad prune verb computes the same liveness.
    fn live_attach_ids_now(&self) -> HashSet<String> {
        live_attach_ids_snapshot()
    }

    fn worker_identity_published(&self, rows: &[RegistryAgent]) -> bool {
        self.squad_members.values().flatten().any(|member| {
            let Some(worker) = member.worker.as_deref() else {
                return false;
            };
            let Some(agent) = rows.iter().find(|agent| agent.name == worker) else {
                return false;
            };
            Self::worker_facts(agent).is_some()
                && (member.harness.is_none() || member.harness_session_id.is_none())
        })
    }

    /// Remove positively dead stored members from every surviving squad and
    /// refresh the in-memory membership projection before the next Layout.
    /// Registry-row cleanup remains the existing off-loop `fno-agents reap`
    /// action, so one menu gesture covers both durable sideline stores.
    fn sweep_dead_sideline(&mut self, client_id: u64) {
        let refused: Vec<(u64, String)> = self
            .panes
            .iter()
            .filter_map(|(&pid, entry)| {
                entry
                    .refused_worker
                    .as_ref()
                    .map(|worker| (pid, worker.clone()))
            })
            .collect();
        let mut evidence = self.member_evidence();
        for (_, worker) in &refused {
            evidence.add_dead(worker.clone());
        }
        // A never-bound member has no refused pane and no session
        // row: its death marker lives in the journal under its worker name.
        for name in scan_spawn_journal().never_bound.into_keys() {
            evidence.add_dead_name(name);
        }
        let live_cwds: Vec<String> = self.panes.values().map(|p| p.cwd.clone()).collect();
        let origin_exists = |path: &str| Path::new(path).exists();
        let now = crate::squad_store::now_epoch_secs();
        let outcome = crate::squad_store::prune_with_evidence_with_generations(
            Some(&self.store_generations),
            |squad| {
                crate::squad_store::prune_decision_with_evidence(
                    squad,
                    true,
                    &evidence,
                    &live_cwds,
                    &origin_exists,
                    now,
                )
            },
            &evidence,
        );
        match outcome {
            Ok((outcome, batch)) => {
                if !self.persist_result(Ok(batch)) {
                    self.notice(client_id, "sweep skipped: squad store changed");
                    return;
                }
                self.reload_members_from_store();
                // Refused restore placeholders are positive dead markers even
                // when their registry row is gone. Remove their visible panes
                // after the store pass; keep a last pane so the session's
                // >=1-pane invariant remains intact.
                let mut closed_placeholders = 0;
                for (pid, _) in refused {
                    if self.panes.len() <= 1 {
                        break;
                    }
                    self.close_pane(pid);
                    closed_placeholders += 1;
                }
                self.notice(
                    client_id,
                    format!(
                        "swept {} dead member(s) and {} refused pane(s); kept {} live, {} unknown",
                        outcome.members_reaped,
                        closed_placeholders,
                        outcome.members_kept_live,
                        outcome.members_kept_unknown
                    ),
                );
                self.reap_action(client_id);
                self.push_layout(true);
            }
            Err(error) => self.notice(client_id, format!("sweep failed: {error}")),
        }
    }

    /// Materialize the persisted named squads at the first real attach (US2).
    /// `rows`/`cols` are the attaching client's dims; `home_sid` is its own cwd
    /// squad, restored as the active anchor afterward so the restored squads sit
    /// in the sideline without stealing the view. Per-squad failure isolation:
    /// a squad that cannot even open a shell is skipped with a notice, never a
    /// crash (AC2-FR: a degraded restore leaves a fully usable session).
    fn restore_squads(&mut self, rows: u16, cols: u16, home_sid: u64) {
        // Heal the store before reading: the old random-mint identity
        // let a repo's home squad append a row per mux restart. The write side
        // now derives a stable key; this migrates the backlog rows already on
        // disk onto that key and collapses the duplicates. One locked mutation,
        // prune-shaped; a write error degrades to a notice, never refuses
        // (AC2-FR / AC-ERR1).
        if let Err(e) = crate::squad_store::collapse_duplicate_squads() {
            self.notice_all(format!("squad collapse at restore skipped: {e}"));
        }
        let loaded = crate::squad_store::load();
        self.store_generations = loaded.generations;
        if let Some(n) = loaded.notice {
            self.notice_all(n);
        }
        // Stash the external-lifecycle tombstones so the sideline can
        // render them BEFORE the squads-empty early-out - a store with no named
        // squads can still hold stopped/failed external rows to act on. The
        // startup reconcile against `claude agents --all` runs off-loop from the
        // attach path and refreshes this via `ExternalLifecycleSync`.
        self.external_lifecycle = loaded.external_lifecycle;
        if loaded.squads.is_empty() {
            // No stored workspace: adopted panes get their own tabs in home
            // rather than dangling unplaced.
            self.place_adopted_leftovers(home_sid);
            self.restored = true;
            return;
        }
        let live = self.live_attach_ids_now();
        // (US4) Self-heal sweep: drop unnamed squads whose every origin is
        // gone and which host no restorable member, writing through remove() so
        // the store converges without a manual prune. Named squads are never
        // touched (Locked Decision 3). A remove() failure - including the
        // build-tree write guard refusing, or a read-only store - degrades to a
        // notice and the squad is still skipped in memory, so restore never
        // aborts because of the sweep (AC2-FR).
        let origin_exists = |p: &str| std::path::Path::new(p).exists();
        let mut squads: Vec<_> = Vec::with_capacity(loaded.squads.len());
        for sq in loaded.squads {
            let sweep = sq.name.is_empty()
                && !sq.origins.iter().any(|o| origin_exists(o.as_str()))
                && !sq
                    .members
                    .iter()
                    .any(|m| !m.tombstone && live.contains(&m.attach_id));
            if sweep {
                match crate::squad_store::remove_with_generations(
                    Some(&self.store_generations),
                    "",
                    &sq.key,
                ) {
                    Ok(batch) => self.store_generations.extend(batch.generations),
                    Err(e) => {
                        self.notice_all(format!("squad prune at restore skipped: {e}"));
                    }
                }
                continue;
            }
            squads.push(sq);
        }
        // Reverse lookup so a persisted isolated-account member restores
        // against its own daemon, not the default ~/.claude.
        let iso_ctx = isolated_attach_ctx();
        let home_cwd = std::env::var_os("HOME")
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Worker members left idle across every restored squad, for
        // the one post-restore notice. An operator who sees no resumed worker
        // must be able to tell "zero were recorded" from "the code never ran".
        let mut idle_workers_total = 0usize;
        // The registry names that still exist, read once up front:
        // a worker member whose row was reaped (`fno agents rm`, a GC pass)
        // can never resume, so restore prunes it instead of counting a ghost
        // idle row forever. A name still present but exited STAYS - that row
        // is exactly the resumable card this feature exists for.
        let known_workers = self.known_worker_names();
        if known_workers.is_none() {
            self.notice_all(
                "restore: registry unreadable; worker member prune skipped (no row deleted)",
            );
        }
        let mut pruned_workers = 0usize;
        #[cfg(test)]
        let policy = RESTORE_POLICY_OVERRIDE.with(|slot| {
            if let Some(p) = slot.borrow().as_ref().copied() {
                return p;
            }
            HOLD_WORKERS_OVERRIDE.with(|slot| {
                slot.borrow()
                    .map(crate::digest_overlay::policy_from_hold_workers)
                    .unwrap_or_else(restore_policy_now)
            })
        });
        #[cfg(not(test))]
        let policy = restore_policy_now();
        // The one knob, three states. `hold` is today's default:
        // named held panes, resume on focus. `idle` leaves every member an
        // idle row. `resume` walks the same idle path here and then runs the
        // bulk driver at the end, so startup never silently respawns unless
        // the operator asked for exactly that (AC4-EDGE).
        let hold_workers = policy == crate::digest_overlay::MuxRestorePolicy::Hold;
        let journal = scan_spawn_journal();
        let receipt_store_error = journal.error;
        if let Some(error) = receipt_store_error.as_deref() {
            self.notice_all(format!("restore: {error}"));
        }
        let SpawnJournal {
            receipts: spawn_receipts,
            never_bound,
            ..
        } = journal;
        let mut worker_members_total = 0usize;
        let mut held_workers_total = 0usize;
        // Portal seats held idle across every restored squad.
        let mut held_portals_total = 0usize;
        let mut live_portals_total = 0usize;
        let mut refused_workers_total = 0usize;
        // Worker members whose work the graph says is DONE. They are
        // history, not garbage: kept as members, never held, never refused-
        // pane'd, never shell-substituted in the tree lane. Read via the test
        // override so a unit test never reads the real graph (same seam as
        // RESTORE_REGISTRY_ROWS).
        #[cfg(test)]
        let done_sessions = RESTORE_DONE_SESSIONS.with(|slot| {
            slot.borrow()
                .clone()
                .unwrap_or_else(crate::backlog_view::done_session_ids)
        });
        #[cfg(not(test))]
        let done_sessions = crate::backlog_view::done_session_ids();
        let mut done_workers_total = 0usize;
        let mut done_worker_names: Vec<String> = Vec::new();
        let mut skipped_done_tabs = 0usize;
        // Member accretion: every attach-id the fresh registry file
        // still names, EXITED rows included (an exited row is the resumable
        // dim card; only a forgotten id is dead weight). `None` = unreadable
        // registry, retire nothing (same fail-safe as the worker prune).
        let registry_rows = restore_registry_rows();
        let known_attach_ids: Option<HashSet<String>> = registry_rows
            .as_ref()
            .map(|rows| rows.iter().filter_map(|r| r.attach_id.clone()).collect());
        //  The tombstone evidence sets, read from the SAME parse.
        // Revival joins a tombstoned member to a non-terminal registry row
        // (declared join, both keys); death joins it to a terminal row or a
        // positively falsified pid (claude rows only, `stale_live_attach_ids`).
        // A member ABSENT from both sets is unknown, never dead.
        let live_row_ids: HashSet<String> = registry_rows
            .as_ref()
            .map(|rows| {
                rows.iter()
                    .filter(|r| !r.exited)
                    .filter_map(|r| r.attach_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let live_row_sessions: HashSet<String> = registry_rows
            .as_ref()
            .map(|rows| {
                rows.iter()
                    .filter(|r| !r.exited)
                    .filter_map(|r| r.harness_session_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let stale_ids: HashSet<String> = crate::restore_gate::stale_live_attach_ids_for_restore();
        let mut retired_members_total = 0usize;
        //  Members whose tombstone a live registry row falsified -
        // the false deaths this bug wrote. Counted for the one-line notice.
        let mut revived_members_total = 0usize;
        //  Members kept without death evidence (not provably live,
        // not provably dead). Counted for the one-line notice.
        let mut kept_unknown_members = 0usize;
        for ps in squads {
            let cwd0 = ps
                .origins
                .first()
                .cloned()
                .unwrap_or_else(|| home_cwd.clone());
            let mut members: Vec<crate::squad_store::StoredMember> = Vec::new();
            // Worker bindings skipped as done in this squad's member
            // loop, so the tree lane prunes their leaves instead of minting
            // shells. Reset per squad: bindings name workers, not squads.
            let mut done_bindings: HashSet<String> = HashSet::new();
            // Worker members left idle by this restore. Counted for
            // the notice, never spawned.
            let mut idle_workers = 0usize;
            // The tabs we build, in final order. Attach mappings are inserted at
            // spawn time (; the tree lane binds panes into trees before
            // any tab exists, so the mapping cannot ride the tab list).
            let mut tabs: Vec<Tab> = Vec::new();
            // Portal slots held idle by this restore: (index, row,
            // pane, tab id). Filled in the tree lane once the tab id exists,
            // inserted into `portals` after the squad lands.
            let mut held_portal_seats: Vec<(u8, String, u64, TabId, Option<String>)> = Vec::new();
            // Live member panes spawned but not yet placed in a tab:
            // (attach_id, pane, stored tab name). The tree lane places them by
            // slot; the legacy lane gives each its own tab.
            let mut member_panes: Vec<(String, u64, Option<String>)> = Vec::new();
            let mut detached_adoptions: Vec<(u64, crate::squad_store::StoredMember)> = Vec::new();
            // The zero-live-member fallback shell tab, if we create one;
            // a deferred template restore removes it once real template tabs land.
            let mut fallback_tid: Option<TabId> = None;
            // An empty stored name is the unnamed sentinel (a home squad / lane);
            // it restores unnamed. A restored unnamed lane folds into the home
            // squad when origins match, else into the live squad already holding
            // its key: one live squad per stored identity.
            let restore_name = (!ps.name.is_empty()).then(|| ps.name.clone());
            let fold_sid = match restore_name {
                Some(_) => None,
                None if (self.session.squad(home_sid)).is_some_and(|h| h.origins == ps.origins) => {
                    Some(home_sid)
                }
                None => self.live_holder_of("", &ps.key, None),
            };
            // The persisted durable identity, adopted onto the rebuilt squad so a
            // later persist reuses its store entry instead of minting a new one.
            let restore_key = ps.key.clone();
            for m_orig in &ps.members {
                //  A tombstone against a session the registry
                // currently calls live is a FALSE death: the walk lifts it and
                // the cleared clone takes the live path below, so every push
                // (worker keep or plain re-attach) persists tombstone: false
                // and the row renders again instead of waiting for a hand
                // edit. The clear is noticed, never silent.
                let revived_clear = if m_orig.tombstone {
                    let revived = live_row_ids.contains(&m_orig.attach_id)
                        || m_orig
                            .harness_session_id
                            .as_deref()
                            .is_some_and(|s| live_row_sessions.contains(s));
                    if revived {
                        revived_members_total += 1;
                        let mut cleared = m_orig.clone();
                        let recorded_reason = cleared
                            .tombstone_reason
                            .take()
                            .unwrap_or_else(|| "reason unrecorded".into());
                        self.notice_all(format!(
                            "restore: revived {} - its tombstone stood against a live registry row ({recorded_reason})",
                            m_orig.attach_id
                        ));
                        cleared.tombstone = false;
                        Some(cleared)
                    } else {
                        // A tombstone the registry has FORGOTTEN (no row
                        // names its attach-id, live or exited) is dead weight a
                        // first-seen death only borrowed: retire it. An exited row
                        // still naming the id keeps the dim card. An unreadable
                        // registry retires nothing (fail-safe, rule). A
                        // FRESH death still tombstones once below - retention is
                        // bounded at one restart cycle, not forever.
                        let forgotten = !m_orig.attach_id.is_empty()
                            && known_attach_ids
                                .as_ref()
                                .is_some_and(|ids| !ids.contains(&m_orig.attach_id));
                        if forgotten {
                            retired_members_total += 1;
                            done_bindings.insert(m_orig.attach_id.clone());
                            continue;
                        }
                        members.push(m_orig.clone()); // already dead - stays a tombstone
                        continue;
                    }
                } else {
                    None
                };
                let m: &crate::squad_store::StoredMember = revived_clear.as_ref().unwrap_or(m_orig);
                // A worker member is a registry NAME, not a
                // claude jobId. A keeper-hosted pane may survive the previous
                // server and arrive through `keeper_readopt`; a worker with no
                // adopted pane remains idle and is never respawned silently.
                // Resume uses the harness's own form, while a detached marker
                // keeps an adopted live pane paneless until that gesture.
                if m.worker.is_some() {
                    worker_members_total += 1;
                    let worker_name = m.worker.as_deref().expect("checked above");
                    // A worker launched into this still-running server is
                    // already represented by its worker mapping. Attaching a
                    // client triggers restore, but must not import that same
                    // member again or create a held placeholder beside it.
                    let existing = self.member_pane(m).or_else(|| {
                        // The refused placeholder's TITLE carries the
                        // refusal reason, so an exact-name scan misses it and a
                        // second restore pass minted a twin pane. Its
                        // `refused_worker` IS the binding: same pane reused.
                        self.panes.iter().find_map(|(pid, entry)| {
                            ((entry.name.as_deref() == Some(worker_name)
                                || entry.refused_worker.as_deref() == Some(worker_name))
                                && self.session.find_pane(*pid).is_some())
                            .then_some(*pid)
                        })
                    });
                    if let Some(pid) = existing.filter(|pid| {
                        self.panes.contains_key(pid)
                            && (self.session.find_pane(*pid).is_some()
                                || self.detached_panes.contains_key(pid))
                    }) {
                        members.push(m.clone());
                        if m.detached {
                            debug_assert!(self.detached_panes.contains_key(&pid));
                        }
                        continue;
                    }
                    // A keeper-hosted pane SURVIVED the server death and was
                    // already re-adopted: bind it into its stored tab and
                    // spawn nothing. Same pid before and after (the child
                    // never died), and the worker_pane mapping makes every
                    // later resume FOCUS it instead of opening a second
                    // writer.
                    if let Some(pid) = self.take_adopted_for_member(m) {
                        members.push(m.clone());
                        if m.detached {
                            detached_adoptions.push((pid, m.clone()));
                            continue;
                        }
                        let binding =
                            worker_binding_key(m).unwrap_or_else(|| worker_name.to_string());
                        self.worker_pane
                            .entry(binding.clone())
                            .or_default()
                            .push(pid);
                        member_panes.push((binding, pid, m.tab_name.clone()));
                        continue;
                    }
                    // Classify BEFORE the policy branch so the
                    // default `hold` policy reaches the retirement. A member
                    // the registry forgot and the journal never received is
                    // Gone: mint no pane, keep no member row on the persist,
                    // and put its binding in done_bindings so the tree lane
                    // skips its tab instead of shell-substituting it.
                    if classify_member(m, known_workers.as_ref(), &spawn_receipts)
                        == MemberVerdict::Gone
                    {
                        pruned_workers += 1;
                        done_bindings.insert(
                            worker_binding_key(m).unwrap_or_else(|| worker_name.to_string()),
                        );
                        continue;
                    }
                    if hold_workers {
                        // Doneness gate: a worker whose node is done
                        // (status done / merge_status merged / completed_at
                        // set) is shipped work. Holding a pane for it, or a
                        // refused pane when identity is missing, rebuilds a
                        // ghost the operator already collected. Keep the
                        // member (history), skip every pane.
                        let member_done =
                            match (m.harness.as_deref(), m.harness_session_id.as_deref()) {
                                (Some(harness), Some(session_id)) => done_sessions
                                    .contains(&(harness.to_string(), session_id.to_string())),
                                _ => false,
                            };
                        if member_done {
                            members.push(m.clone());
                            done_workers_total += 1;
                            done_worker_names.push(worker_name.to_string());
                            done_bindings.insert(
                                worker_binding_key(m).unwrap_or_else(|| worker_name.to_string()),
                            );
                            continue;
                        }
                        members.push(m.clone());
                        let matching_rows: Vec<&RegistryAgent> = self
                            .agents
                            .iter()
                            .filter(|agent| worker_registry_match(m, agent, worker_name))
                            .collect();
                        let row = match matching_rows.as_slice() {
                            [one] => Some(*one),
                            [] => None,
                            _ => {
                                self.notice_all(format!(
                                    "restore: {worker_name} is ambiguous; resume by exact session id"
                                ));
                                None
                            }
                        };
                        let held = row
                            .filter(|agent| Self::row_resumable(agent))
                            .and_then(Self::worker_facts)
                            .or_else(|| {
                                receipt_for_member(&spawn_receipts, m)
                                    .cloned()
                                    .map(|mut facts| {
                                        facts.name = worker_name.to_string();
                                        facts
                                    })
                            })
                            // A reaped row purges its spawn receipt with it, so
                            // a worker that died and was reaped arrives here
                            // with neither. The persisted member itself is the
                            // remaining durable identity (harness + full session
                            // id + cwd); resume from it rather than refuse.
                            .or_else(|| {
                                row.is_none()
                                    .then(|| Self::member_resume_facts(m, worker_name))
                                    .flatten()
                            });
                        let pane = if let Some(facts) = held {
                            self.hold_worker_pane(facts, rows, cols, &cwd0)
                                .inspect(|_| {
                                    held_workers_total += 1;
                                })
                        } else {
                            let reason = restore_worker_refusal_reason(
                                m,
                                row,
                                receipt_store_error.as_deref(),
                                &spawn_receipts,
                                &never_bound,
                            );
                            self.refused_worker_pane(worker_name, &reason, rows, cols, &cwd0)
                                .inspect(|_| {
                                    refused_workers_total += 1;
                                })
                        };
                        match pane {
                            Ok(pid) => {
                                let binding = worker_binding_key(m)
                                    .unwrap_or_else(|| worker_name.to_string());
                                member_panes.push((binding, pid, m.tab_name.clone()));
                            }
                            Err(error) => {
                                self.notice_all(format!(
                                    "restore: could not hold {worker_name}: {error}"
                                ));
                            }
                        }
                        continue;
                    }
                    // Only the idle/resume policies reach here: the
                    // classifier above already retired every Gone member, so
                    // the old known_workers arm is gone with it. A kept
                    // member restores as an idle row; nothing respawns
                    // silently.
                    members.push(m.clone());
                    idle_workers += 1;
                    continue;
                }
                //  A tombstone is a death claim, so it is written only
                // from evidence of death: a terminal registry status or a
                // positively falsified pid. The complement of `live` proves
                // nothing - an unreadable registry, a row this snapshot never
                // saw, and a member with no attach id all land there, and
                // tombstoning them buried live sessions (the af8e03f2 false
                // death). Anything not provably dead keeps as an idle row and
                // is re-decided on the next restore; nothing respawns.
                let joined_row = registry_rows.as_ref().and_then(|rows| {
                    rows.iter().find(|r| {
                        crate::squad_store::member_joins_row(
                            m,
                            r.attach_id.as_deref(),
                            r.harness_session_id.as_deref(),
                        )
                    })
                });
                let death_reason = joined_row.and_then(|r| {
                    if r.exited {
                        Some("registry row exited")
                    } else if r
                        .attach_id
                        .as_deref()
                        .is_some_and(|id| stale_ids.contains(id))
                    {
                        Some("recorded pid is gone")
                    } else {
                        None
                    }
                });
                if let Some(reason) = death_reason {
                    // Dead with evidence: tombstone it (AC1-EDGE dimmed row,
                    // persisted, carrying the reason it fired).
                    members.push(crate::squad_store::StoredMember {
                        attach_id: m.attach_id.clone(),
                        tombstone: true,
                        tombstone_reason: Some(reason.to_string()),
                        detached: false,
                        tab_name: m.tab_name.clone(),
                        cwd: m.cwd.clone(),
                        worker: None,
                        harness: None,
                        harness_session_id: None,
                        pane_id: None,
                    });
                    continue;
                }
                if !live.contains(&m.attach_id) {
                    // Not provably live, not provably dead: keep the member
                    // (no pane, no tombstone) and re-decide on the next
                    // restore. The keep is noticed below, never silent.
                    members.push(m.clone());
                    kept_unknown_members += 1;
                    continue;
                }
                // Live: re-attach it into a fresh pane, routed to its daemon.
                let (acct, cd) = match iso_ctx.get(&m.attach_id) {
                    Some((a, d)) => (Some(a.as_str()), Some(d.as_path())),
                    None => (None, None),
                };
                // A staged re-entry plan (a live claude member)
                // supplies the argv and its config dir; a staged refusal keeps
                // the member and spawns nothing - the visible outcome the
                // plan promises. No entry is an off-axis member: the
                // reconstructed argv stands, exactly as before.
                let spawn = self.staged_batch_argv(&m.attach_id, acct, cd);
                // (case 2/3) Spawn at the member's OWN stored cwd when it
                // still exists - a worktree worker restores into its worktree,
                // not the squad's `origins[0]`. A gone cwd (an archived worktree)
                // falls back to `origins[0]` with a two-path notice so the
                // operator learns where it landed rather than discovering it
                // silently mid-edit.
                let (spawn_cwd, fallback_notice) =
                    restore_member_cwd(m.cwd.as_deref(), &cwd0, |p| {
                        std::path::Path::new(p).is_dir()
                    });
                if let Some(gone) = fallback_notice {
                    self.notice_all(format!(
                        "restore: {}'s directory {gone} is gone; restored at {cwd0} instead",
                        m.attach_id
                    ));
                }
                let spawned = spawn.and_then(|(argv, dir)| {
                    self.spawn_pane_cmd(&argv, rows, cols, &spawn_cwd)
                        .map(|pid| (pid, dir))
                });
                match spawned {
                    Ok((pid, dir)) => {
                        // Title the restored pane from its registered name
                        // (the roster, the sidepane's source) so it matches the
                        // fresh-spawn label across reattach/restart.
                        self.name_attached_pane(pid, &m.attach_id, dir.as_deref());
                        self.attached.insert(m.attach_id.clone(), pid);
                        member_panes.push((m.attach_id.clone(), pid, m.tab_name.clone()));
                        members.push(crate::squad_store::StoredMember {
                            attach_id: m.attach_id.clone(),
                            tombstone: false,
                            tombstone_reason: None,
                            detached: false,
                            tab_name: m.tab_name.clone(),
                            cwd: m.cwd.clone(),
                            worker: None,
                            harness: None,
                            harness_session_id: None,
                            pane_id: None,
                        });
                    }
                    Err(e) => {
                        // AC2-FR: keep the member (not tombstone - it is live),
                        // skip its pane, notice; restore continues.
                        self.notice_all(format!("restore: could not attach {}: {e}", m.attach_id));
                        members.push(crate::squad_store::StoredMember {
                            attach_id: m.attach_id.clone(),
                            tombstone: false,
                            tombstone_reason: None,
                            detached: false,
                            tab_name: m.tab_name.clone(),
                            cwd: m.cwd.clone(),
                            worker: None,
                            harness: None,
                            harness_session_id: None,
                            pane_id: None,
                        });
                    }
                }
            }
            idle_workers_total += idle_workers;
            // The tree lane: stored full topologies rebuild the exact
            // shape - hand splits, arbitrary weights, tab order, focus - instead
            // of one flat tab per member. Every slot is resolved BEFORE the tree
            // is built (shells and unbound fno ids spawn their substitute panes
            // up front), so a malformed document refuses the whole tab before
            // half of one exists, per-tab, without touching the others.
            let mut placed: HashSet<u64> = HashSet::new();
            if !ps.tab_trees.is_empty() {
                let mut pane_by_id: HashMap<String, u64> = member_panes
                    .iter()
                    .map(|(id, pid, _)| (id.clone(), *pid))
                    .collect();
                // A capture taken before the registry row published names the
                // bare worker, while the member now binds by session. Alias the
                // bare name to the member's pane, or to nothing when two members
                // share that name.
                let mut pane_aliases: HashMap<&str, Option<u64>> = HashMap::new();
                for m in &members {
                    let (Some(worker), Some(binding)) =
                        (m.worker.as_deref(), worker_binding_key(m))
                    else {
                        continue;
                    };
                    if binding == worker {
                        continue;
                    }
                    let Some(pane) = pane_by_id.get(&binding).copied() else {
                        continue;
                    };
                    pane_aliases
                        .entry(worker)
                        .and_modify(|seen| *seen = None)
                        .or_insert(Some(pane));
                }
                for (alias, pane) in pane_aliases {
                    if let Some(pane) = pane {
                        pane_by_id.entry(alias.to_string()).or_insert(pane);
                    }
                }
                // The home lane's fresh attach shell already IS an
                // unnamed shell at cwd0. Rebuilding a stored unnamed
                // pure-shell tree beside it minted a second one and
                // re-captured both, so every server life left one more shell
                // tree in the store (the measured 12). The first such tree
                // consumes the claim: the fresh tab stands in for it.
                let mut fresh_home_shell_claim = fold_sid == Some(home_sid)
                    && (self.session.squad(home_sid)).is_some_and(|h| !h.tabs.is_empty());
                for st in &ps.tab_trees {
                    // Prune done leaves BEFORE any pane minting: a
                    // slot binding a done member's leaf is removed, one-child
                    // splits collapse. An all-done tab is skipped whole:
                    // the shell substitute must never rebuild a ghost tab the
                    // gate just declined to hold.
                    let done_slot_names: HashSet<String> = st
                        .slots
                        .iter()
                        .filter(|slot| slot_names_done(slot, &done_bindings))
                        .map(|slot| slot.name.clone())
                        .collect();
                    let pruned_tree = prune_done_slots(&st.tree, &done_slot_names);
                    let Some(tree_spec) = pruned_tree else {
                        skipped_done_tabs += 1;
                        continue;
                    };
                    let kept_slots: Vec<&LayoutSlot> = st
                        .slots
                        .iter()
                        .filter(|slot| !slot_names_done(slot, &done_bindings))
                        .collect();
                    // The stored shell the fresh attach shell already
                    // represents: skip the tree, no pane minted, the claim is
                    // consumed once. A portal slot is not that shell:
                    // skipping the tree would drop the portal, so an unnamed
                    // one-slot portal tab restores as its own tab, held.
                    let pure_shell_unnamed = st.tab_name.is_none()
                        && matches!(&tree_spec, LayoutTreeSpec::Slot(_))
                        && kept_slots.len() == 1
                        && matches!(kept_slots[0].binding, LayoutBinding::Shell)
                        && kept_slots[0].portal.is_none();
                    if pure_shell_unnamed && fresh_home_shell_claim {
                        fresh_home_shell_claim = false;
                        // The fresh home shell stands in for the stored tab.
                        // The stored slot's own keeper shell re-adopted at its
                        // birth id, though: seat THAT pane in the home tab (the
                        // operator keeps their shell, ring included) and retire
                        // the just-minted stand-in, so a restart converges
                        // instead of accreting a shell tab every cycle.
                        if let Some(birth) = kept_slots.first().and_then(|slot| slot.pane_id) {
                            if let Some(adopted) = self.take_adopted_for_slot(birth) {
                                self.reclaim_home_shell(home_sid, adopted);
                            }
                        }
                        continue;
                    }
                    let mut slot_pane: HashMap<&str, u64> = HashMap::new();
                    let mut missing: Vec<&str> = Vec::new();
                    for slot in &kept_slots {
                        let pane = match &slot.binding {
                            LayoutBinding::Fno(id) => match pane_by_id.get(id.as_str()) {
                                Some(p) => Some(*p),
                                // The worker did not come back. NEVER auto-relaunch
                                // a dead worker (a claude session costs money and
                                // may re-enter a loop it was killed out of):
                                // substitute a shell, keep the shape, name it.
                                None => {
                                    missing.push(id.as_str());
                                    None
                                }
                            },
                            LayoutBinding::Shell | LayoutBinding::Anchor => None,
                        };
                        match pane {
                            Some(p) => {
                                slot_pane.insert(slot.name.as_str(), p);
                            }
                            None => {
                                let tab_name = st.tab_name.as_deref().unwrap_or("?");
                                if let Some(p) =
                                    self.restore_shell_slot(slot, rows, cols, &cwd0, tab_name)
                                {
                                    slot_pane.insert(slot.name.as_str(), p);
                                }
                            }
                        }
                    }
                    let lookup = |name: &str| slot_pane.get(name).copied();
                    match spec_to_node(&tree_spec, &lookup) {
                        Some(root) => {
                            let leaves = tree::leaves(&root);
                            // Every slot pane was minted for THIS tab, so a
                            // resolved focus pane is always one of its leaves.
                            let focus = st
                                .focus
                                .as_deref()
                                .and_then(|f| slot_pane.get(f).copied())
                                .or_else(|| leaves.first().copied())
                                .unwrap_or(leaves[0]);
                            let tid = self.session.mint_tab_id();
                            // A portal slot's seat is the shell the
                            // substitute already minted; remember it until the
                            // tab ids are final.
                            portal_reach::collect_portal_slot_seats(
                                &kept_slots,
                                &slot_pane,
                                tid,
                                &mut held_portal_seats,
                            );
                            placed.extend(leaves.iter().copied());
                            tabs.push(Tab {
                                name: st.tab_name.clone(),
                                id: tid,
                                root,
                                focus,
                            });
                            if !missing.is_empty() {
                                self.notice_all(format!(
                                    "restore: {} worker(s) did not come back (tab {}): {} - resume with `fno agents resume <name>`",
                                    missing.len(),
                                    st.tab_name.as_deref().unwrap_or("?"),
                                    missing.join(", ")
                                ));
                            }
                        }
                        None => {
                            self.notice_all(format!(
                                "restore: tab {} has a malformed stored tree; restoring flat",
                                st.tab_name.as_deref().unwrap_or("?")
                            ));
                        }
                    }
                }
                // A live member pane no tree placed (recruited after the last
                // capture, or every tree refused) still gets its own tab - a
                // live pane must never be left dangling without one.
                for (_id, pid, tab_name) in member_panes {
                    if !placed.contains(&pid) {
                        let tid = self.session.mint_tab_id();
                        tabs.push(Tab {
                            name: tab_name,
                            id: tid,
                            root: Node::Leaf(pid),
                            focus: pid,
                        });
                    }
                }
            } else {
                // The legacy lane: one tab per live member, named from the
                // member's stored tab name (US4).
                for (_id, pid, tab_name) in member_panes {
                    let tid = self.session.mint_tab_id();
                    tabs.push(Tab {
                        name: tab_name,
                        id: tid,
                        root: Node::Leaf(pid),
                        focus: pid,
                    });
                }
            }
            // >=1-tab invariant (AC1-EDGE zero-live, or every attach spawn
            // failed): open one shell at origins[0] (else $HOME). Skipped for a
            // home-merge lane - home_sid already has its own shell tab, so an
            // all-dead lane merges only its tombstone members (no extra shell).
            if tabs.is_empty() && fold_sid.is_none() {
                match self.spawn_pane(rows, cols, &cwd0) {
                    Ok(pid) => {
                        let tid = self.session.mint_tab_id();
                        fallback_tid = Some(tid);
                        tabs.push(Tab {
                            name: None,
                            id: tid,
                            root: Node::Leaf(pid),
                            focus: pid,
                        });
                    }
                    Err(e) => {
                        // Cannot even open a shell: skip this squad entirely, the
                        // rest of the restore proceeds (per-squad isolation).
                        self.notice_all(format!("restore: skipped workspace {}: {e}", ps.name));
                        continue;
                    }
                }
            }
            // Register the squad with its first tab, push the rest, so
            // agent_rows reconciles the panes and member_ctx resolves them. A
            // folded lane merges its tabs + members INTO the live squad
            // holding its identity rather than adding a duplicate.
            let sid = if let Some(home_sid) = fold_sid {
                for tab in tabs {
                    self.session
                        .squad_mut(home_sid)
                        .expect("fold squad live")
                        .tabs
                        .push(tab);
                }
                let home_members = self.squad_members.entry(home_sid).or_default();
                for member in members {
                    if !home_members.iter().any(|existing| existing == &member) {
                        home_members.push(member);
                    }
                }
                // Home adopts the lane's durable key so its next persist updates
                // the SAME store entry instead of minting a second one.
                if let Some(h) = self.session.squad_mut(home_sid) {
                    h.key = restore_key;
                }
                home_sid
            } else {
                let sid = self.next_squad_id;
                self.next_squad_id += 1;
                let mut it = tabs.into_iter();
                let first_tab = it.next().expect("tabs is non-empty above");
                self.session
                    .add_squad(sid, ps.origins.clone(), restore_name, first_tab);
                // Adopt the persisted key so the rebuilt squad keeps its identity.
                if let Some(s) = self.session.squad_mut(sid) {
                    s.key = restore_key;
                    // The active tab is part of the captured shape; a
                    // stored index is clamped, never trusted to be in range.
                    if !ps.tab_trees.is_empty() {
                        if let Some(idx) = ps.active_tab {
                            let n = s.tabs.len().max(1);
                            s.active_tab = idx.min(n - 1);
                        }
                    }
                }
                for tab in it {
                    self.session
                        .squad_mut(sid)
                        .expect("just added")
                        .tabs
                        .push(tab);
                }
                self.squad_members.insert(sid, members);
                sid
            };
            for (pane, member) in detached_adoptions {
                if let Some(detached) = DetachedPane::from_member(
                    &member,
                    sid,
                    ps.name.clone(),
                    ps.key.clone(),
                    ps.origins.clone(),
                ) {
                    self.detached_panes.insert(pane, detached);
                }
            }
            // Re-arm every held portal seat (in portal_reach): the
            // entry goes back in the map, the seat pane gets its name and its
            // held message, and the reach or a focus fills it on first demand.
            // A seat whose re-adopted viewer joined its slot re-arms LIVE.
            let (this_held, this_live) =
                portal_reach::rearm_held_portal_seats(self, std::mem::take(&mut held_portal_seats));
            held_portals_total += this_held;
            live_portals_total += this_live;
            // Persist the reconciled membership (members dead at restore are now
            // tombstoned in the store) plus the just-restored tree capture, so a
            // second restart restores the same shape.
            self.persist_squad(sid);
            // (US8) DEFER the template rebuild: at restore the off-loop
            // registry reader has not populated `self.agents`, so applying now
            // would bind every fno slot to a shell and leave the restored member
            // panes stranded in their own tabs. Queue it and drain on the first
            // AgentRows tick once the sessions register - the re-apply then pulls
            // each restored pane into the template topology and empties its member
            // tab. A slot whose session never returns degrades to a shell.
            // Skipped when trees restored: the tree capture is uniform
            // over every tab (template tabs included), so re-applying the
            // template lane on top would build every template tab a second time.
            if !ps.tab_specs.is_empty() && ps.tab_trees.is_empty() {
                self.pending_template_restores.push(PendingRestore {
                    sid,
                    specs: ps.tab_specs.clone(),
                    fallback_tid,
                    attempts: 0,
                });
            }
        }
        // One notice for the whole restore, so "no worker came back"
        // is distinguishable from "the counter never ran" (the positive-marker
        // rule): a positive count names how many idle rows await a resume.
        if idle_workers_total > 0 {
            self.notice_all(format!(
                "restore: {idle_workers_total} worker row(s) idle - resume them from the agent panel"
            ));
        }
        if hold_workers && worker_members_total == 0 && held_portals_total == 0 {
            self.notice_all("restore: 0 worker member(s) recorded; held 0 worker pane(s)");
        } else if hold_workers || held_portals_total > 0 || live_portals_total > 0 {
            portal_reach::notify_held_receipt(
                self,
                held_workers_total,
                held_portals_total,
                live_portals_total,
            );
        }
        if hold_workers && refused_workers_total > 0 {
            self.notice_all(format!(
                "restore: {refused_workers_total} worker(s) could not be held"
            ));
        }
        if pruned_workers > 0 {
            self.notice_all(format!(
                "restore: retired {pruned_workers} worker member(s) whose registry row is gone"
            ));
        }
        if done_workers_total > 0 || skipped_done_tabs > 0 {
            // The skipped members are named ONCE, in aggregate, not
            // per row - the same positive-marker shape as pruned_workers. A
            // tab skipped whole (every slot done) counts here too: the
            // operator's tab count is the thing the receipt must explain.
            let mut names = done_worker_names
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if done_worker_names.len() > 6 {
                names.push_str(", ...");
            }
            self.notice_all(format!(
                "restore: skipped {done_workers_total} done worker pane(s) and {skipped_done_tabs} done tab(s): {names}"
            ));
        }
        if retired_members_total > 0 {
            // Same positive-marker shape: the retirement is named,
            // restore never acts on an absence silently.
            self.notice_all(format!(
                "restore: retired {retired_members_total} member(s) the registry no longer names"
            ));
        }
        if revived_members_total > 0 {
            self.notice_all(format!(
                "restore: revived {revived_members_total} member(s) whose tombstone stood against a live registry row"
            ));
        }
        if kept_unknown_members > 0 {
            self.notice_all(format!(
                "restore: kept {kept_unknown_members} member(s) with no death evidence; they re-decide on the next restore"
            ));
        }
        // policy = resume: the walk deliberately left every member
        // idle; the bulk driver now brings each back through its own
        // harness's declared form. The reply end is dropped on purpose - at
        // startup the report reaches the operator through the notices the
        // driver emits, not through a control connection.
        self.place_adopted_leftovers(home_sid);
        if policy == crate::digest_overlay::MuxRestorePolicy::Resume {
            let (tx, _rx) = oneshot::channel::<ServerMsg>();
            self.workspace_restore_start(false, None, tx);
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
        ctx: Option<(u64, String, String, Vec<String>, String)>,
        churn: bool,
    ) {
        let Some((sid, name, key, origins, attach_id)) = ctx else {
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
                //  A pane death is a real observed event, so the
                // churn arm keeps tombstoning - it just names why now.
                mm.tombstone_reason = Some("member pane died".into());
            }
            let members = members.clone();
            self.persist_stored(&name, &key, &origins, &members);
            // A death never writes a tab-tree removal, so the
            // graceful paths must: a churned worker's collapsed tab leaves
            // the store now, not at the next restart. The persist_stored half
            // above keeps the workspace row (AC4-EDGE); this captures the
            // surviving live topology.
            if self.session.squad(sid).is_some() {
                self.persist_squad(sid);
            }
        } else {
            members.retain(|m| m.attach_id != attach_id);
            let survives = self.session.squad(sid).is_some();
            if survives {
                self.persist_squad(sid);
            } else {
                self.squad_members.remove(&sid);
                self.persist_remove(&name, &key);
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

    /// Resolve a gesture's re-entry plan OFF the core loop and
    /// re-dispatch the gesture when the verdict lands. `request` names the
    /// gesture to re-enter (the attach id + its placement, or the resume
    /// name); the `ReentryPlanReady` handler stuffs the verdict into
    /// `reentry_verdict` and replays the SAME command, so every gate
    /// (shape, catalog, reconcile-focus, placement) re-runs against live
    /// state and the arm proceeds with the canonical argv. A refusal routes
    /// back as a notice and no pane starts. `row_name` is the REGISTRY name
    /// the resolver keys on (never the attach id, which is transport-local).
    fn resolve_reentry(
        &self,
        client_id: u64,
        row_name: &str,
        transition: &str,
        request: ReentrySpawnRequest,
    ) {
        let core_tx = self.self_tx.clone();
        let name = row_name.to_string();
        let transition = transition.to_string();
        tokio::spawn(async move {
            let verdict = run_reentry_plan(&name, &transition).await;
            let _ = core_tx
                .send(CoreMsg::ReentryPlanReady {
                    id: client_id,
                    request: Box::new(request),
                    verdict,
                })
                .await;
        });
    }

    /// The live claude attach members restore must plan for, as
    /// (attach_id, registry name) pairs. Reads the same sources the restore
    /// loop reads - the squad store, the registry file, the live-id snapshot -
    /// so the batch and the loop agree on membership without a third
    /// resolver. Worker members never appear: restore holds them idle and
    /// their resume gesture (a focus) plans its own re-entry.
    fn restore_plan_targets(&self) -> Vec<(String, String)> {
        let store = crate::squad_store::load();
        if store.squads.is_empty() {
            return Vec::new();
        }
        let live = live_attach_ids_snapshot();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rows = std::fs::read_to_string(agents_view::registry_path())
            .ok()
            .and_then(|raw| agents_view::derive_rows(&raw, now));
        let Some(rows) = rows else {
            return Vec::new();
        };
        store
            .squads
            .iter()
            .flat_map(|s| s.members.iter())
            .filter(|m| !m.tombstone && m.worker.is_none() && live.contains(&m.attach_id))
            .filter_map(|m| {
                rows.iter()
                    .find(|a| {
                        a.attach_id.as_deref() == Some(m.attach_id.as_str())
                            && a.harness.as_deref() == Some("claude")
                    })
                    .map(|a| (m.attach_id.clone(), a.name.clone()))
            })
            .collect()
    }

    /// Resolve a batch of (attach id, registry name) plans OFF the
    /// core loop and route them back with the loop to re-enter. Every entry
    /// lands - a verdict or that member's refusal - so the consuming loop
    /// never waits and never guesses.
    fn resolve_plan_batch(
        &self,
        client_id: u64,
        wanted: Vec<(String, String)>,
        replay: BatchReplay,
    ) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let mut plans = HashMap::new();
            for (attach_id, name) in wanted {
                plans.insert(attach_id, run_reentry_plan(&name, "attach").await);
            }
            let _ = core_tx
                .send(CoreMsg::BatchPlansReady {
                    id: client_id,
                    plans,
                    replay: Box::new(replay),
                })
                .await;
        });
    }

    /// Restore through the canonical re-entry plans. Every live
    /// claude attach member's plan resolves OFF the core loop first, then the
    /// existing restore loop runs with the verdicts staged. No claude member
    /// to plan restores synchronously, exactly as before - which also keeps
    /// the runtime-less test attach paths free of a spawn.
    fn restore_with_plans(&mut self, client_id: u64, rows: u16, cols: u16, home_sid: u64) {
        let wanted = self.restore_plan_targets();
        if wanted.is_empty() {
            self.restore_squads(rows, cols, home_sid);
            self.reconcile_external_lifecycle();
            return;
        }
        self.restore_pending = true;
        self.resolve_plan_batch(
            client_id,
            wanted,
            BatchReplay::Restore {
                home_sid,
                rows,
                cols,
            },
        );
    }

    /// Consume one member's staged batch plan. `Ok` is the argv to
    /// spawn with (plus its config dir for the pane title); `Err` is the
    /// member's visible refusal - spawn nothing. No staged entry is an
    /// off-axis member: the reconstructed argv stands, exactly as before.
    fn staged_batch_argv(
        &mut self,
        attach_id: &str,
        acct: Option<&str>,
        cd: Option<&std::path::Path>,
    ) -> Result<(Vec<String>, Option<std::path::PathBuf>), String> {
        match self.batch_plans.remove(attach_id) {
            Some(Ok(verdict)) => {
                let dir = verdict.config_dir.clone();
                Ok((verdict.prefixed_argv(), dir))
            }
            Some(Err(reason)) => Err(reason),
            None => Ok((
                attach_argv(attach_id, acct, cd),
                cd.map(std::path::Path::to_path_buf),
            )),
        }
    }

    /// The registry row NAME behind an attach id when that row is a
    /// claude row, from the live catalog (the sidepane's in-memory source).
    /// This is the identifier the resolver requests plans by; the attach id
    /// itself is transport-local and never a registry key. The resolver is
    /// claude-only, so a non-claude or harness-less row answers None and its
    /// attach keeps the legacy argv.
    fn attach_claude_row(&self, attach_id: &str) -> Option<String> {
        self.agents
            .iter()
            .find(|a| a.attach_id.as_deref() == Some(attach_id))
            .filter(|a| a.harness.as_deref() == Some("claude"))
            .map(|a| a.name.clone())
    }

    /// The harness behind an attach id, from the live catalog. The
    /// argv builder needs it: each harness execs its own interface, and only
    /// the row knows which one this id belongs to.
    fn attach_row_harness(&self, attach_id: &str) -> Option<String> {
        self.agents
            .iter()
            .find(|a| a.attach_id.as_deref() == Some(attach_id))
            .and_then(|a| a.harness.clone())
    }

    /// One attach gesture's spawn argv. `None` means the canonical
    /// resolution is now running off-loop and the caller must stop - the
    /// verdict re-enters this gesture through `ReentryPlanReady`. A replayed
    /// gesture holds the staged verdict (its argv IS the answer, config dir
    /// included). Every non-claude row keeps the legacy argv unchanged - that
    /// axis carries no route or account binding to preserve.
    fn attach_gesture_argv(
        &mut self,
        client_id: u64,
        id: &str,
        placement: &crate::proto::PanePlacement,
    ) -> Option<(Vec<String>, Option<std::path::PathBuf>)> {
        if let Some(verdict) = self.reentry_verdict.take() {
            return Some((verdict.prefixed_argv(), verdict.config_dir));
        }
        if let Some(name) = self.attach_claude_row(id) {
            self.resolve_reentry(
                client_id,
                &name,
                "attach",
                ReentrySpawnRequest::Attach {
                    attach_id: id.to_string(),
                    placement: placement.clone(),
                },
            );
            return None;
        }
        let (acct, cd) = self.attach_account_ctx(id);
        let harness = self.attach_row_harness(id);
        Some((
            attach_argv_for(harness.as_deref(), id, acct.as_deref(), cd.as_deref()),
            cd,
        ))
    }

    /// A resume gesture's re-entry plan. A replayed gesture holds
    /// the staged verdict; otherwise the caller (a claude row only) fires the
    /// off-loop resolution and `None` tells it to stop - the verdict re-enters
    /// the gesture through `ReentryPlanReady`. The caller gates the claude
    /// check: this never runs for another harness.
    fn resume_gesture_plan(
        &mut self,
        client_id: u64,
        row_name: &str,
        request: ReentrySpawnRequest,
    ) -> Option<ReentryVerdict> {
        if let Some(verdict) = self.reentry_verdict.take() {
            return Some(verdict);
        }
        self.resolve_reentry(client_id, row_name, "resume", request);
        None
    }

    /// Shell `fno agents mail send <name> <text>` OFF the core loop, mirroring
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

    /// Shell `fno agents resume <name>` off-loop; the door owns harness routing
    /// and race-time refusals, and its verdict returns as the visible notice.
    fn resume_agent(&self, id: u64, name: String) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = agent_actions::run_resume(&name).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// Shell one `fno backlog` reorder verb OFF the core loop, mirroring
    /// `agent_action`. The notice is the CLI's verdict; the AUTHORITATIVE order
    /// change is the graph reader's next republish, never an optimistic local
    /// reorder - so a verb that fails loudly leaves the rendered order truthful.
    fn backlog_verb(&self, id: u64, node: String, verb: crate::proto::BacklogVerb) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_backlog_verb(&node, verb).await;
            let _ = core_tx.send(CoreMsg::DispatchResult { id, notice }).await;
        });
    }

    /// Shell `fno agents peek <name>` OFF the core loop, the
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

    /// Bulk-reap OFF the core loop: shell `fno-agents reap --json` once,
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

    /// Resolve a `StopExternal` target by attach id: return the
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
    /// re-push the sideline, so an in-flight `stopping…`/`removing…`
    /// state is visible the instant the CAS commits (AC1-UI), before the
    /// off-loop subprocess even starts.
    fn refresh_external_lifecycle(&mut self) {
        self.external_lifecycle = crate::squad_store::load().external_lifecycle;
        self.push_layout(true);
    }

    /// Run an external lifecycle subprocess (`claude stop|rm <attach_id>`) OFF
    /// the core loop, then durably record the completion under the
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
    /// --all` ONCE at startup (AC1-FR/AC3-FR), OFF the core loop. Filters
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
        // `id` is a primary key everywhere a client is looked up
        // (`clients.iter().find(|c| c.id == id)`); a stale entry still
        // queued for teardown (e.g. `CoreMsg::Gone` not yet drained) must
        // not survive a fresh attach under the same id, or lookups can
        // resolve to the dying client instead of this one.
        self.clients.retain(|c| c.id != id);
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
            last_press: None,
        });
        self.push_layout(true);
        // Cold-attach snapshot rides the RELIABLE channel. The
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
                    entry
                        .stats
                        .frames_composited
                        .fetch_add(1, Ordering::Relaxed);
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
        // Eager restore of persisted named squads, once per server
        // lifetime, on the first REAL (non-passive) attach - a passive observer
        // has no dims to spawn panes with, so it defers restore to the first
        // terminal. The restored squads sit in the sideline; this client's view
        // stays on its own cwd squad.
        if !self.restored && !passive {
            self.restored = true;
            // Restore resolves every live claude member's re-entry
            // plan off-loop first and runs the loop when the batch lands; the
            // reconcile-after-restore ordering lives inside.
            self.restore_with_plans(id, rows, cols, view.0);
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
    /// from it is dropped (defense-in-depth - the read-only guarantee holds
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
        // `reemit` marks a layout-changing pass, and every topology
        // mutation (split, close, drag, tab create/rename/reorder, focus)
        // funnels through here regardless of which command path drove it - so
        // this is the one capture hook. Debounced inside; a persist here never
        // blocks the geometry pass below beyond its own flock budget.
        if reemit {
            self.mark_topology_dirty();
        }
        // Geometry pass: each distinct viewed tab, once, at its view-scoped
        // smallest-client clamp (Locked 1/5). The applied area is cached so
        // the tab keeps it when its last viewer leaves.
        let viewed: HashSet<TabId> = self.clients.iter().map(|c| c.view.1).collect();
        #[allow(clippy::type_complexity)]
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
            // hits the PTY (AC1-FR). A framed pane's pty is its CONTENT
            // rect: the border ring is cells the program never sees.
            for (pid, r) in &rects {
                if let Some(entry) = self.panes.get_mut(pid) {
                    let content = crate::pane_border::content_rect(*r);
                    if entry.requested_size != (content.rows, content.cols) {
                        entry.requested_size = (content.rows, content.cols);
                        if let Err(e) = entry.pty.resize(content.rows, content.cols, 0, 0) {
                            // Grid and kernel winsize would disagree: log it.
                            eprintln!("fno mux: pty resize failed: {e}");
                        }
                        if entry.pty.is_keeper_hosted() {
                            // Applied later, in `drain_pty_output`, once the
                            // keeper's ack for THIS resize round-trips
                            // through the pane's own ordered output channel
                            // (`PaneChunk::Resized`). Flipping `vt` here,
                            // before the round trip lands, would let output
                            // the child already produced under the OLD size
                            // - still ahead of this resize on the wire -
                            // arrive after the flip and get fed into the
                            // wrong-size grid (the byte-exact reattach
                            // race this branch's keeper hop introduced).
                        } else {
                            entry.vt.resize(content.rows, content.cols);
                        }
                        // Ask the child to repaint once the resize dust
                        // settles: arm a deferred nudge the 1s core tick fires.
                        // An immediate re-signal would coalesce into the burst
                        // the resize itself raised and change nothing.
                        entry.nudge_due = Some(Instant::now() + Self::NUDGE_DELAY);
                    }
                }
            }
            tab_rects.insert(tid, (rects, focus, area));
        }

        // Evict half of the seen set: level-triggered every pass,
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
        #[allow(clippy::type_complexity)]
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

        // An observer client sees EVERY pane: its `visible` set is
        // all live panes so the browser can draw any pane the server broadcasts
        // without an upstream `View`. Precomputed here so the send loop can
        // hold `&mut self.clients` while reading it.
        let all_pane_ids: Vec<u64> = self.panes.keys().copied().collect();
        let mut dead = Vec::new();
        for (c, (layout_msg, focused_modes, rects)) in self.clients.iter_mut().zip(per) {
            // ModeSync BEFORE the Layout that assumes it (brief ordering).
            // Full means the reliable channel is wedged. Closed means its
            // writer exited; leave membership to the reader so any command
            // already on the socket stays ordered before `Gone`.
            if c.synced_modes != focused_modes {
                let bytes = vt::mode_diff(c.synced_modes, focused_modes);
                if !bytes.is_empty() {
                    match c.reliable_tx.try_send(ServerMsg::ModeSync { bytes }) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            dead.push(c.id);
                            continue;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => continue,
                    }
                }
                c.synced_modes = focused_modes;
            }
            match c.reliable_tx.try_send(layout_msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    dead.push(c.id);
                    continue;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => continue,
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
                // which for an idle shell is never: the CI flake.
                let mut d = c.dirty.lock().unwrap();
                d.clear();
                for pid in &frame_ids {
                    if let Some(entry) = self.panes.get(pid) {
                        entry
                            .stats
                            .frames_composited
                            .fetch_add(1, Ordering::Relaxed);
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
        let squads: Vec<SquadMeta> = self
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
                        // (US2) An explicit rename is the ONLY chosen
                        // name; a pane-derived or ordinal label is not. The
                        // client renders a chosen name without a forced ordinal.
                        named: t.name.is_some(),
                        name: tab_label(
                            t.name.as_deref(),
                            self.panes.get(&t.focus).map(|e| {
                                (
                                    e.name.as_deref(),
                                    e.node.as_deref(),
                                    e.cwd.as_str(),
                                    e.cmd.as_deref(),
                                )
                            }),
                            s.canonical_cwd(),
                            i,
                        ),
                        // (v22) Every leaf pane of the tab, labelled from
                        // its own entry, so the navigator can goto a pane in any
                        // tab/squad - not just the active view the client tiles.
                        panes: tree::leaves(&t.root)
                            .iter()
                            .map(|pid| {
                                let e = self.panes.get(pid);
                                let ctx = pane_meta::pane_ctx(
                                    &self.agents,
                                    &self.session_name,
                                    &self.ctx_by_session,
                                    *pid,
                                );
                                pane_meta::pane_meta(
                                    *pid,
                                    e.and_then(|e| e.name.as_deref()),
                                    e.and_then(|e| e.node.as_deref()),
                                    e.map(|e| e.cwd.as_str()).unwrap_or(""),
                                    e.and_then(|e| e.cmd.as_deref()),
                                    e.and_then(|e| self.branch_by_cwd.get(&e.cwd))
                                        .map(String::as_str),
                                    ctx.as_deref(),
                                )
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
                // Blast radius for the RemoveSquad confirm: live leaves
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
            // The focused pane's provenance for the status row. Re-sent
            // whenever `focus` changes, so the cell tracks focus for free.
            focus_node: self.panes.get(&focus).and_then(|e| e.node.clone()),
            // The work-queue lane; already board-ordered by the
            // reader, routes joined on at publish time.
            backlog: self.routed_backlog(),
            backlog_lanes: self.backlog_lanes.clone(),
            backlog_stale: self.backlog_stale,
            sweep_dead_count: self.dead_sweep_count(),
        }
    }

    /// The backlog cards with their v18 routes joined on at publish time
    /// (Phase B). An in-flight card gains, in priority order: the pane
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
    /// re-check (AC2-ERR).
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

    /// The sideline row set as a PANE UNION (Locked 5): every live pane
    /// in every squad/tab is a row, in (squad, tab, pane) order, carrying its
    /// `tab` so the client renders a tab-ordinal suffix. A pane is enriched from
    /// the registry entry that hosts it - `mux == (this session, pane)`, or a
    /// watch-only row whose `attach_id` reconciles to it (attach map) -
    /// else it is a bare pane labelled from its `PaneEntry` (Discretion 5). One
    /// row per entity: a registry agent merged onto a pane never also renders
    /// watch-only. Truly paneless registry rows (bg/headless/daemon/roster)
    /// append AFTER the pane rows, matched to a squad by cwd (exact or child) or
    /// the `squad: None` catch-all. The fact-badge lattice is unchanged: a dead
    /// pane forces `exited` over any live-TTL badge (fact beats report).
    /// (US4) Compose a row's dim line-2 subline from the off-loop
    /// branch map + the cwd's tail segment: `<branch> · <tail>`, either part
    /// omitted if absent, both absent (an empty cwd) -> `None` (AC1-EDGE: no
    /// sub-row is emitted). The client renders it verbatim and truncates.
    fn compose_subline(&self, cwd: &str) -> Option<String> {
        subline_from(self.branch_by_cwd.get(cwd).map(String::as_str), cwd)
    }

    /// A registry row's message tail from the off-loop transcript map.
    /// `None` for a row with no session uuid (a bare pane, a tombstone) or one
    /// whose transcript yielded no prose - the extended table then renders an
    /// EMPTY cell, never an inferred value. The key is the row's transcript
    /// identity: the claude uuid first, else the harness session id (the
    /// rollout uuid a codex row carries) - the same key the tick builds.
    fn compose_tail(&self, a: &RegistryAgent) -> Option<String> {
        self.tail_by_session
            .get(
                a.claude_session_uuid
                    .as_deref()
                    .or(a.harness_session_id.as_deref())?,
            )
            .cloned()
    }

    /// The reachability evidence halves a registry-backed row carries on the
    /// wire: `None` until a probe has answered for that row. The map key is
    /// the row's full harness session id when it has one (identity first),
    /// else the label; a row with neither (bare panes,
    /// tombstones, external lifecycle) never calls these - there is nothing
    /// to probe.
    fn truth_basis(&self, a: &RegistryAgent) -> Option<String> {
        self.truth_reading(a).and_then(|t| t.basis.clone())
    }

    fn truth_age(&self, a: &RegistryAgent) -> Option<u64> {
        self.truth_reading(a).and_then(|t| t.age_s)
    }

    fn truth_reading(&self, a: &RegistryAgent) -> Option<&TruthReading> {
        agent_harness_session_id(a)
            .and_then(|sid| self.truth_by_name.get(sid))
            .or_else(|| self.truth_by_name.get(&a.name))
    }

    /// The insert half of the seen set: a one-shot side effect of
    /// an actual focus action (`Command::FocusPane`; hover-focus settles to
    /// a client-side `FocusPane`, so it rides this for free), never a
    /// per-pass level check - AC2-EDGE requires that parking on a pane while
    /// it is `Working` never marks a later `Done` seen, only a fresh focus
    /// action does. `Command::AttachAgent`'s reconcile-focus arm now
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
        let t0 = Instant::now();
        let frame = entry.vt.frame();
        for c in &self.clients {
            if c.visible.contains(&pid) {
                // Per-VIEWING-CLIENT enqueue, matching frames_emitted's
                // per-client wire write one-for-one (a shared frame fanned
                // out to two clients is 2 composites and 2 emits when
                // nothing drops). Counting the shared build once would make
                // composited > emitted meaningless exactly when several
                // clients watch one pane.
                entry
                    .stats
                    .frames_composited
                    .fetch_add(1, Ordering::Relaxed);
                c.dirty.lock().unwrap().insert(pid, frame.clone());
                c.notify.notify_one();
            }
        }
        entry
            .stats
            .cpu_ns
            .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
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
                // Remember where the button went down; the release arm needs it
                // to tell a click from the tail of a drag.
                if let Some(c) = self.clients.iter_mut().find(|c| c.id == client_id) {
                    c.last_press = Some((pane, event.row, event.col));
                }
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
                // highlight, and opens the URL under it if there is one.
                //
                // No modifier: this arm is only reached in a pane that never
                // negotiated mouse reporting, where a bare left click has no
                // other meaning (click-to-focus is still unshipped, see this
                // function's doc). Shift-click stays the native-terminal escape
                // hatch - the client drops shifted events before they get here.
                match self.panes.get(&pane).and_then(|e| e.vt.selection_text()) {
                    Some(text) => self.send_copy(client_id, text),
                    None => {
                        // Only a press and release on the SAME cell of the SAME
                        // pane is a click. Without this, a drag begun in another
                        // pane arrives here as a bare release (the client
                        // re-hit-tests every report), and an ordinary cross-pane
                        // selection would launch a browser (codex P2, PR 702).
                        // TAKE, not read: a stored press authorizes exactly one
                        // gesture. Left un-consumed it also authorizes any LATER
                        // unmatched release at the same cell - and unmatched
                        // releases do reach here, because a press swallowed as
                        // chrome client-side never cancels it (codex, PR 702).
                        let clicked = self
                            .clients
                            .iter_mut()
                            .find(|c| c.id == client_id)
                            .and_then(|c| c.last_press.take())
                            == Some((pane, event.row, event.col));
                        if clicked {
                            if let Some(url) = self
                                .panes
                                .get(&pane)
                                .and_then(|e| e.vt.link_at(event.row, event.col))
                            {
                                self.send_open_link(client_id, url);
                            }
                        }
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

    /// Ship a clicked URL to the client that clicked it, mirroring
    /// [`Self::send_copy`]: only the requesting client, over the reliable
    /// channel. Re-checks the scheme allowlist so a future caller cannot reach
    /// the client's opener with an unvetted URL - `link_at` already filters, and
    /// this is the second lock on the same door.
    fn send_open_link(&mut self, client_id: u64, url: String) {
        if !crate::link::is_openable(&url) {
            return;
        }
        let Some(c) = self.clients.iter().find(|c| c.id == client_id) else {
            return;
        };
        if c.reliable_tx.try_send(ServerMsg::OpenLink { url }).is_err() {
            eprintln!(
                "fno mux: client {client_id} reliable channel wedged on OpenLink; dropping it"
            );
            self.clients.retain(|c| c.id != client_id);
            self.push_layout(true);
        }
    }

    /// (v56, hover affordance) One link-span lookup for the requesting client
    /// only: resolve the link under pane-local `(row, col)` and reply with its
    /// visible cells. The guards mirror the click path's ownership rule: the
    /// pane must be in the requester's live view, and a plain left click must
    /// be MUX-owned - a pane whose app negotiated mouse reporting gets no
    /// hover affordance, because its grid interaction belongs to the app.
    /// Coordinates only, never text; every miss (non-link cell, invisible
    /// pane, app-owned click) answers an EMPTY cell list so the client clears
    /// the underline instead of waiting. No pane state changes, no broadcast:
    /// co-viewers never see another viewer's hover.
    fn link_hover(&mut self, client_id: u64, pane: u64, row: u16, col: u16, seq: u64) {
        // Bind the requester once: the visibility check and the reply send
        // name the same client, and two independent finds are two places to
        // drift. The wedge path mutates `clients`, so it runs after the
        // borrows drop.
        let wedged = {
            let Some(c) = self.clients.iter().find(|c| c.id == client_id) else {
                return;
            };
            let cells = match self.panes.get(&pane) {
                Some(e)
                    if route_mouse(e.vt.modes(), MouseKind::Release(MouseButton::Left))
                        == MouseAction::SelectRelease
                        && c.visible.contains(&pane) =>
                {
                    e.vt.link_span(row, col)
                        .map(|span| span.cells)
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            };
            c.reliable_tx
                .try_send(ServerMsg::LinkHover {
                    pane_id: pane,
                    seq,
                    cells,
                })
                .is_err()
        };
        if wedged {
            eprintln!(
                "fno mux: client {client_id} reliable channel wedged on LinkHover; dropping it"
            );
            self.clients.retain(|c| c.id != client_id);
            self.push_layout(true);
        }
    }

    /// Apply one block-navigation op to `pane` (v8). Jump moves the
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

    /// Apply one in-scrollback search op to `pane` (v12). Open/step mutate
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

    /// Whether a rerun may write to `pane` (idle guard); see
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
    /// for this send (not `self.agents`, which is parked with no viewer); `Err`
    /// carries the refusal reason - the read failed, or the registry carries a
    /// row whose pane cannot be read - so the guard fails closed. A guarded
    /// send also consults a positive DND row first; raw `PaneSend`
    /// (`guarded == false`) is the writer-claim holder's own channel: it still
    /// consults DND, but an unreadable registry there maps to no rows observed,
    /// so absent a positive DND marker it remains unguarded.
    fn pane_send(
        &mut self,
        pane: u64,
        bytes: &[u8],
        guarded: bool,
        expected_identity: Option<&str>,
        agents: Result<Vec<RegistryAgent>, &'static str>,
    ) -> ServerMsg {
        let Some(entry) = self.panes.get(&pane) else {
            return dead_pane(pane);
        };
        if let Some(refusal) = self.pane_send_identity_gate(
            pane,
            entry.name.as_deref(),
            entry.unreconciled,
            expected_identity,
            match &agents {
                Ok(rows) => Ok(rows.as_slice()),
                Err(reason) => Err(*reason),
            },
        ) {
            return refusal;
        }
        if let Some(expected) = expected_identity {
            let host = entry.name.as_deref().unwrap_or("<unknown>");
            let rows = match agents.as_deref() {
                Ok(rows) => rows,
                Err(reason) => {
                    return ServerMsg::Err {
                        code: err_code::TARGET_IDENTITY_MISMATCH,
                        msg: format!("addressed {expected}, pane hosts {host}; {reason}"),
                    };
                }
            };
            let matches: Vec<&RegistryAgent> = rows
                .iter()
                .filter(|a| {
                    a.mux.as_ref().is_some_and(|(session, pane_id)| {
                        session == &self.session_name && *pane_id == pane
                    })
                })
                .collect();
            let viewer_row = matches
                .is_empty()
                .then(|| crate::thread_viewer::row_for_pane(&self.portals, pane, rows))
                .flatten();
            let mut occupants: Vec<&RegistryAgent> = Vec::new();
            for row in matches {
                let equivalent = occupants.iter().any(|existing| {
                    existing.name == row.name
                        && existing.effective_identity() == row.effective_identity()
                });
                if !equivalent {
                    occupants.push(row);
                }
            }
            let registry_identity = occupants
                .first()
                .and_then(|row| row.effective_identity())
                .or_else(|| viewer_row.and_then(|row| row.effective_identity()))
                .unwrap_or("<unknown>");
            if (occupants.len() != 1 && viewer_row.is_none())
                || registry_identity != expected
                || occupants
                    .first()
                    .copied()
                    .or(viewer_row)
                    .is_some_and(|row| row.name != host)
            {
                let registry = occupants
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return ServerMsg::Err {
                    code: err_code::TARGET_IDENTITY_MISMATCH,
                    msg: format!(
                        "addressed {expected}, pane hosts {host}; registry fno_id {registry_identity}; occupants: {}",
                        if registry.is_empty() {
                            "<none>"
                        } else {
                            &registry
                        }
                    ),
                };
            }
        }
        if agents
            .as_deref()
            .is_ok_and(|rows| pane_is_dnd(rows, &self.session_name, pane))
        {
            return ServerMsg::Err {
                code: err_code::TARGET_DND,
                msg: "target is DND; use fno agents mail send to queue durable until the hold lifts, or fno agents mail hold --off to release it"
                    .to_string(),
            };
        }
        if guarded {
            let rows = match agents.as_deref() {
                Ok(rows) => rows,
                Err(reason) => {
                    return ServerMsg::Err {
                        code: err_code::TARGET_NOT_IDLE,
                        msg: reason.to_string(),
                    };
                }
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

    /// Answer a blocked prompt without focusing the pane. The freshness
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

    /// The `mux_pane_counters` event payload for the current live pane set:
    /// every pane's monotonic totals plus its provenance (the join keys the
    /// spawn gate prices a pane against a bg session with). `None` when no
    /// pane is live - an idle mux writes nothing.
    fn pane_stats_payload(&self) -> Option<String> {
        let mut rows: Vec<(u64, &PaneEntry)> = self.panes.iter().map(|(&p, e)| (p, e)).collect();
        if rows.is_empty() {
            return None;
        }
        // Deterministic pane order: differencing is per-pane, but a stable
        // layout keeps consecutive samples eyeball-diffable in the journal.
        rows.sort_unstable_by_key(|(p, _)| *p);
        let panes: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(pid, e)| {
                let c = &e.stats;
                serde_json::json!({
                    "pane_id": pid,
                    "node": e.node,
                    "name": e.name,
                    "cmd": e.cmd,
                    "bytes_in": c.bytes_in.load(Ordering::Relaxed),
                    "grid_updates": c.grid_updates.load(Ordering::Relaxed),
                    "frames_composited": c.frames_composited.load(Ordering::Relaxed),
                    "frames_emitted": c.frames_emitted.load(Ordering::Relaxed),
                    "cpu_ns": c.cpu_ns.load(Ordering::Relaxed),
                })
            })
            .collect();
        Some(
            serde_json::json!({
                "session": self.session_name,
                "panes": panes,
            })
            .to_string(),
        )
    }

    /// Snapshot every live pane's monotonic counters and emit one
    /// `mux_pane_counters` event onto the machine-global events journal, so
    /// `fno agents top` and the spawn gate read one stream from any cwd.
    /// Totals, never rates (see [`PaneCounters`]): readers difference two
    /// samples over a window. Same fire-and-forget shell-out contract as
    /// [`Core::touch`]: a failure is counted, never raised to the serving
    /// path.
    fn emit_pane_stats(&self) {
        // Same guards as `touch`: under cfg!(test) current_exe is the test
        // binary, and FNO_PANE_STATS_EMIT=0 is the operator kill switch.
        if cfg!(test) || std::env::var_os("FNO_PANE_STATS_EMIT").is_some_and(|v| v == "0") {
            return;
        }
        let Some(data) = self.pane_stats_payload() else {
            return;
        };
        let failures = Arc::clone(&self.pane_stats_emit_failures);
        tokio::spawn(async move {
            const PANE_STATS_EMIT_TIMEOUT: Duration = Duration::from_secs(10);
            let mut command = crate::process_admission::tokio_command(fno_bin());
            command
                .args([
                    "doctor",
                    "event",
                    "emit",
                    "--type",
                    "mux_pane_counters",
                    "--source",
                    "daemon",
                    "--global",
                    "--data",
                    &data,
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let ok = matches!(
                tokio::time::timeout(
                    PANE_STATS_EMIT_TIMEOUT,
                    crate::process_admission::tokio_status(&mut command),
                )
                .await,
                Ok(Ok(s)) if s.success()
            );
            if !ok {
                let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
                eprintln!("fno mux: mux_pane_counters emit failed ({n} this session)");
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

    /// Toggle the git working-diff pane, for the row the menu pinned or the
    /// focused pane (keybind).
    ///
    /// Close runs before open so a press is always a close when one is live -
    /// never a queued reopen. That is what makes a double-press converge: the
    /// second press resolves against the state the first one left, so two
    /// presses land on open-then-closed, never two panes for one source.
    ///
    /// Every path ends in a visible layout change or a notice; a press that
    /// does nothing at all would read as a dead keybind.
    fn toggle_diff_pane(
        &mut self,
        client_id: u64,
        view: (u64, TabId),
        vp: Rect,
        agent: Option<String>,
        pane: Option<u64>,
    ) -> Flow {
        let focus = self.viewed_tab(view).map(|t| t.focus);
        let mut ambiguous = false;
        let src = match (&agent, pane) {
            // A pinned pane is the exact row that was clicked and carries its
            // own spawn cwd, so it resolves a row `agent_rows` synthesized from
            // the tree (never in `self.agents`) and separates two rows that
            // share a name.
            (_, Some(p)) if self.panes.contains_key(&p) => self.panes[&p].cwd.clone(),
            (Some(name), _) => {
                let mut hits = self.agents.iter().filter(|a| &a.name == name);
                match (hits.next(), hits.next()) {
                    (Some(a), None) => a.cwd.clone(),
                    // Names are not unique. Diffing the first match would show
                    // one worker's worktree under another's row - the wrong
                    // answer, delivered convincingly.
                    (Some(_), Some(_)) => {
                        ambiguous = true;
                        String::new()
                    }
                    _ => String::new(),
                }
            }
            // Keybind path, and the fallback when a named pane has since died.
            _ => focus
                .and_then(|p| self.panes.get(&p))
                .map(|p| p.cwd.clone())
                .unwrap_or_default(),
        };
        if let Some((open_src, pid)) = self.diff_pane.take() {
            // A recorded pane the registry no longer knows was closed by some
            // other path (close-pane, tab close, squad teardown). Treat it as
            // already closed rather than let a stale id wedge the toggle.
            if self.panes.contains_key(&pid) {
                let flow = self.close_pane(pid);
                if open_src == src || matches!(flow, Flow::Shutdown) {
                    return flow;
                }
                // A different source: the old pane is gone, fall through and
                // open this one (at most one diff pane in the session).
            }
        }
        if src.is_empty() {
            let why = if ambiguous {
                "more than one row goes by that name - focus its pane and press the diff key"
            } else {
                "no worktree to diff for this row"
            };
            self.notice(client_id, why);
            return Flow::Continue;
        }
        // A spawn silently ignores a cwd that is not a directory and lands in
        // the server's own cwd instead - which for a diff pane would render
        // some OTHER repo's diff under this row's name. Refuse pre-spawn: a
        // reaped worktree must say so, not show a plausible wrong answer.
        if !std::path::Path::new(&src).is_dir() {
            self.notice(client_id, format!("worktree is gone: {src}"));
            return Flow::Continue;
        }
        let (rows, cols) = focus
            .and_then(|p| self.panes.get(&p))
            .map(|e| e.vt.size())
            .unwrap_or((vp.rows, vp.cols));
        let argv = diff_argv();
        // Spawn-first: `spawn_pane_cmd` touches no tree, so a spawn failure
        // leaves the layout untouched with nothing to roll back.
        let pid = match self.spawn_pane_cmd(&argv, rows, cols, &src) {
            Ok(p) => p,
            Err(e) => {
                self.notice(client_id, format!("diff pane failed: {e}"));
                return Flow::Continue;
            }
        };
        let Some(tab) = self.viewed_tab_mut(view) else {
            // Reachable on a source switch: closing the old diff pane can empty
            // its tab, which retires the tab and re-anchors the view, leaving
            // the id captured before the close pointing at nothing. Say so -
            // reaping in silence here would read as a dead keybind.
            self.reap_pane(pid);
            self.notice(client_id, "diff pane: the tab closed under the toggle");
            return Flow::Continue;
        };
        match tree::split(tab, vp, Axis::Horizontal, pid) {
            Ok(()) => {
                self.diff_pane = Some((src, pid));
                self.push_layout(true);
            }
            Err(e) => {
                // Refused (too narrow): reap the pre-spawned pane; the tree was
                // never touched.
                self.reap_pane(pid);
                self.notice(client_id, e.to_string());
            }
        }
        Flow::Continue
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
            Command::ToggleDiffPane { agent, pane } => {
                self.toggle_diff_pane(client_id, view, vp, agent, pane)
            }
            Command::ClosePane => {
                let Some(tab) = self.viewed_tab(view) else {
                    return Flow::Continue;
                };
                self.close_by_operator(tab.focus)
            }
            Command::ClosePortal { seat } => self.close_portal(client_id, seat),
            Command::DetachPane { pane } => {
                match self.detach_worker_pane(pane) {
                    Ok(()) => self.push_layout(true),
                    Err(error) => self.notice(client_id, error),
                }
                Flow::Continue
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
            Command::ResizeSeam { a, b, pos } => {
                let Some(tab) = self.viewed_tab_mut(view) else {
                    return Flow::Continue;
                };
                // Silent on refusal, unlike ResizeDir: a drag streams commands
                // and clamps against a pane minimum constantly, so a notice per
                // rejected cell would be a wall of noise. The client reports a
                // drag that dies on a stale address; a clamp is self-evident
                // from the divider not moving.
                if tree::set_seam_pos(tab, vp, a, b, pos) {
                    self.push_layout(true);
                }
                Flow::Continue
            }
            Command::MovePane { mover, target, dir } => {
                // Resolve both ends against the VIEWED tab first: that owns the
                // keyboard-bind defaults (mover = focus; target = the pane the
                // same geometry FocusDir uses lies `dir`-ward). A drop names both
                // and skips the navigate.
                let (mover, target) = {
                    let Some(tab) = self.viewed_tab(view) else {
                        return Flow::Continue;
                    };
                    let mover = mover.unwrap_or(tab.focus);
                    let Some(target) = target.or_else(|| tree::navigate(&tab.root, vp, mover, dir))
                    else {
                        self.notice(client_id, "no pane in that direction");
                        return Flow::Continue;
                    };
                    (mover, target)
                };
                // A sideline-row drop can name a `mover` living in ANOTHER tab
                // (the row carries the pane id, not the rendered layout). When the
                // two ends live in different tabs, compose the cross-tab move
                // (detach + graft, all-or-nothing); otherwise the within-tab
                // move_leaf, unchanged. Loud on refusal, unlike ResizeSeam: a
                // relocation is ONE deliberate gesture, so a rejected drop that
                // said nothing would read as the feature being broken.
                let src = self.session.find_pane(mover);
                let dst = self.session.find_pane(target);
                match (src, dst) {
                    (Some(s), Some(d)) if s != d => {
                        // move_pane_cross_tab propagates only PaneGone / TooSmall
                        // (from detach_leaf / graft_subtree); Origin is a
                        // within-tab move_leaf verdict and cannot arise here, so
                        // every real Err is a named notice.
                        match self.move_pane_cross_tab(mover, s, target, d, dir) {
                            Ok(()) => {}
                            Err(e) => self.notice(client_id, e.to_string()),
                        }
                    }
                    _ => {
                        let Some(tab) = self.viewed_tab_mut(view) else {
                            return Flow::Continue;
                        };
                        match tree::move_leaf(tab, vp, mover, target, dir) {
                            Ok(()) => self.push_layout(true),
                            // An origin drop is a cancel the client should not have
                            // sent; silently accept it rather than scold the
                            // operator for a gesture that, from their side, did
                            // nothing on purpose.
                            Err(tree::MoveError::Origin) => {}
                            Err(e) => self.notice(client_id, e.to_string()),
                        }
                    }
                }
                Flow::Continue
            }
            Command::BreakPane { pane } => {
                // The interactive twin of ControlVerb::PaneBreak: dispatch into
                // the SAME pane_break (one tree-mutation site, Locked Decision 2).
                // pane_break itself never touches a view; the gesture additionally
                // repoints the ACTING client's focus onto the new tab (Locked
                // Decision 3 - the script path leaves every viewer where it was).
                match self.pane_break(pane, None) {
                    Ok(new_tid) => {
                        if let Some((sid, _)) = self.session.find_tab(new_tid) {
                            self.set_view(client_id, sid, new_tid);
                            self.push_layout(true);
                        }
                    }
                    Err((_code, msg)) => self.notice(client_id, msg),
                }
                Flow::Continue
            }
            Command::JoinTab {
                src_tab,
                anchor_pane,
                dir,
            } => {
                // The interactive twin of ControlVerb::TabJoin, into the SAME
                // tab_join. The gesture picked up a concrete rendered cell, so it
                // names a stable TabId; tab_join resolves a TabSel, so wrap it in
                // Id. tab_join pushes the layout and re-anchors any viewer of the
                // removed source tab. A self-join is refused BAD_REQUEST here (the
                // client also suppresses it) - shown as a named Notice, not a
                // silent swallow.
                match self.tab_join(&TabSel::Id(src_tab), anchor_pane, dir) {
                    Ok(()) => {}
                    Err((_code, msg)) => self.notice(client_id, msg),
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
                let Some((_, _, outcome)) = self.close_tab_cascade(sid, ti) else {
                    return Flow::Continue;
                };
                match outcome {
                    RemoveOutcome::SessionEmpty => Flow::Shutdown,
                    _ => Flow::Continue,
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
                let mut focus_pid = pid;
                if let Some(held) = self.held_workers.get(&pid).cloned() {
                    let current = self.agents.iter().find(|agent| {
                        agent.harness.as_deref() == Some(held.harness.as_str())
                            && agent_harness_session_id(agent)
                                == Some(held.harness_session_id.as_str())
                    });
                    // The SESSION-AWARE gate, the same one
                    // `Command::ResumeAgent` and the sideline render use: a
                    // pane of this session already running the row's session
                    // is direct observation the backend is live, and resuming
                    // under it opens a second writer on the live rollout.
                    if current.is_some_and(|agent| !self.row_resumable_in_session(agent)) {
                        self.held_workers.remove(&pid);
                        self.write_restore_message(
                            pid,
                            &format!("{} was not resumed: session is live elsewhere", held.name),
                        );
                        self.notice(client_id, "session is live elsewhere; resume refused");
                    } else {
                        let facts = current.and_then(Self::worker_facts).unwrap_or(held.clone());
                        let Some((sid, _)) = self.session.find_pane(pid) else {
                            self.held_workers.remove(&pid);
                            self.notice(client_id, "held pane vanished");
                            return Flow::Continue;
                        };
                        let (rows, cols) = self
                            .clients
                            .iter()
                            .find(|client| client.id == client_id)
                            .map(|client| client.dims)
                            .unwrap_or((vp.rows, vp.cols));
                        // The revival gate asks before any plan or argv
                        // resolution. A refusal keeps the held pane held
                        // (a retry after a worker finishes is one click)
                        // and names the gate's verdict on the seat itself.
                        match self.revival_admitted(client_id, &facts, ResumeReplay::Held { pid }) {
                            None => return Flow::Continue,
                            Some(Err(reason)) => {
                                self.write_restore_message(
                                    pid,
                                    &format!("{} was not resumed: {reason}", facts.name),
                                );
                                self.notice(client_id, format!("resume refused: {reason}"));
                                return Flow::Continue;
                            }
                            Some(Ok(())) => {}
                        }
                        // A claude row's held resume runs the
                        // canonical re-entry plan; the `None` arm fires the
                        // off-loop resolution and this focus replays with the
                        // verdict staged. A non-claude row does the
                        // same with its resolved argv.
                        let plan;
                        let staged_argv;
                        if facts.harness == "claude" {
                            let Some(verdict) = self.resume_gesture_plan(
                                client_id,
                                &facts.name,
                                ReentrySpawnRequest::FocusHeld { pid },
                            ) else {
                                return Flow::Continue;
                            };
                            plan = Some(verdict);
                            staged_argv = None;
                        } else {
                            match self.staged_resume_argv.take() {
                                Some(argv) => staged_argv = Some(argv),
                                None => {
                                    let stored_cwd =
                                        (!facts.cwd.is_empty()).then_some(facts.cwd.as_str());
                                    let (spawn_cwd, gone) = self.member_resume_cwd(sid, stored_cwd);
                                    self.resolve_resume_argv(
                                        client_id,
                                        facts.harness.as_str(),
                                        &facts.harness_session_id,
                                        &spawn_cwd,
                                        gone.is_none(),
                                        ResumeReplay::Held { pid },
                                    );
                                    return Flow::Continue;
                                }
                            }
                            plan = None;
                        }
                        match self.resume_worker_into(
                            &facts,
                            sid,
                            Some(pid),
                            rows,
                            cols,
                            plan.as_ref(),
                            staged_argv.as_deref(),
                        ) {
                            Ok((resumed, _, fallback_notice)) => {
                                focus_pid = resumed;
                                if let Some(notice) = fallback_notice {
                                    self.notice(client_id, notice);
                                }
                                self.notice(client_id, format!("resumed {}", facts.name));
                            }
                            Err(error) => {
                                self.held_workers.remove(&pid);
                                self.write_restore_message(
                                    pid,
                                    &format!("{} resume failed: {error}", facts.name),
                                );
                                self.notice(client_id, format!("resume failed: {error}"));
                            }
                        }
                    }
                }
                // A focused portal seat that is still the held shell
                // fills in place (portal_reach); anything else falls through
                // to a plain focus.
                if let Some(flow) =
                    portal_reach::fill_held_portal_seat(self, client_id, view, vp, pid)
                {
                    return flow;
                }
                // Locate the leaf anywhere in the session, then view+focus it.
                let target = self.session.find_pane(focus_pid).map(|(sid, ti)| {
                    (
                        sid,
                        self.session.squad(sid).expect("live squad").tabs[ti].id,
                    )
                });
                match target {
                    Some((sid, tid)) => {
                        self.set_view(client_id, sid, tid);
                        if let Some(tab) = self.viewed_tab_mut((sid, tid)) {
                            tab.focus = focus_pid;
                        }
                        // AC1-HP: focusing a `Done` pane clears its
                        // unseen bit.
                        self.mark_seen_if_done(focus_pid);
                        self.push_layout(true);
                    }
                    // The pane exited racing the click; fail-closed like the
                    // other catalog-named commands.
                    None => self.notice(client_id, "no such pane"),
                }
                Flow::Continue
            }
            Command::AttachAgent { id, placement } => {
                // The portal reach routes BEFORE the attach-shaped
                // gates: `id` is an 8-hex attach id for a claude row but a
                // registry NAME for every other harness (Follow/Locate rows
                // carry no attach id), so the hex gate and the attach catalog
                // gate do not apply. The reach resolves its own row,
                // fail-closed.
                //
                // THE DECODE EDGE. `portal_target` folds the
                // deprecated `thread_pane` bool into the index here, once, so
                // nothing past this point sees two fields that overlap: a
                // pre-v64 client's `thread_pane: true` lands in portal 0,
                // exactly where it always did.
                //
                // The geometry refusal no longer lives here. This
                // edge cannot see whether index N is occupied - only the slot
                // lookup in `reach_portal` learns that - so one check here
                // answered two cases that deserve opposite answers. The
                // decision moved into the reach: a repoint keeps owning its
                // geometry (ignored, visibly), a fresh open honors the
                // caller's placement. This edge only resolves WHICH index.
                if placement.wants_portal() {
                    // An explicit index wins over "any". `portal_new` names no
                    // index BECAUSE the caller must not choose one: allocating
                    // here, where reaches are handled one at a time, is what
                    // makes two clients opening a portal at once safe.
                    let portal = match placement.portal_target() {
                        Some(idx) => idx,
                        // A full space refuses visibly. Repointing an
                        // occupied index is the silent wrong action the other
                        // placement refusals exist to prevent.
                        None => match self.next_free_portal() {
                            Some(idx) => idx,
                            None => {
                                self.notice(
                                    client_id,
                                    "all 256 portal indices are live; close one first",
                                );
                                return Flow::Continue;
                            }
                        },
                    };
                    // `portal` names an index the CALLER chose;
                    // `thread_pane` folds to 0 without the caller naming any,
                    // and that default reach is allowed to go home to a held
                    // seat that names the row.
                    let portal_explicit = placement.portal.is_some();
                    return self.reach_portal(
                        client_id,
                        view,
                        vp,
                        portal,
                        &id,
                        &placement,
                        portal_explicit,
                    );
                }
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
                // roster-synthesized foreign row (: mux None, !exited,
                // attach_id set) satisfies this unchanged - it is exactly the
                // watch-only shape the gate was built for.
                if !self.attachable_agent(&id) {
                    self.notice(client_id, "no such agent");
                    return Flow::Continue;
                }
                // Open-here is inherently "the focused pane of my current view", so a split or a
                // non-CurrentRoute target contradicts it - refuse pre-spawn (AC2-ERR).
                if placement.here
                    && (placement.split.is_some()
                        || !matches!(placement.target, PaneTarget::CurrentRoute))
                {
                    self.notice(client_id, "open-here takes no split or target");
                    return Flow::Continue;
                }
                // Reconcile: an id already mapped to a LIVE pane focuses
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
                // Open-here: repoint the focused viewer pane at B (not a tab/split). Runs after
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
                    // A viewer displaces (swap, unchanged). A non-viewer is accepted only
                    // when it is a lone idle shell - take over the empty tab, the reported bug.
                    // Reaping a shell mints no `attached` entry (no detached session to preserve),
                    // so the spawn-first / replace_leaf / reap-last dance below is otherwise identical.
                    if displaced.is_none() {
                        let leaf_count = self
                            .viewed_tab(view)
                            .map(|t| tree::leaves(&t.root).len())
                            .unwrap_or(0);
                        let takeover = self.panes.get(&focus).is_some_and(|p| {
                            idle_shell_takeover(
                                leaf_count,
                                p.cmd.as_deref(),
                                p.vt.is_pristine_idle_shell(),
                            )
                        });
                        if !takeover {
                            self.notice(client_id, "tab is not empty - use split or new tab");
                            return Flow::Continue;
                        }
                    }
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
                    // The argv comes from the canonical re-entry plan
                    // for a claude row; `None` means the plan is resolving and
                    // this gesture re-enters with the verdict staged.
                    let Some((argv, cd)) = self.attach_gesture_argv(client_id, &id, &placement)
                    else {
                        return Flow::Continue;
                    };
                    let pane_count = self
                        .viewed_tab(view)
                        .map(|tab| tree::leaves(&tab.root).len().saturating_sub(1))
                        .unwrap_or(0);
                    let permit =
                        match crate::process_admission::admit_pane(pane_count, placement.max_panes)
                        {
                            Ok(permit) => permit,
                            Err(error) => {
                                self.notice(client_id, format!("attach failed: {error}"));
                                return Flow::Continue;
                            }
                        };
                    let new_pid = match self
                        .spawn_pane_cmd_with_permit(&argv, rows, cols, &spawn_cwd, permit)
                    {
                        Ok(p) => p,
                        Err(e) => {
                            self.notice(client_id, format!("attach failed: {e}"));
                            return Flow::Continue;
                        }
                    };
                    self.name_attached_pane(new_pid, &id, cd.as_deref());
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
                    self.attached.insert(id.clone(), new_pid);
                    // Reap-last (Locked 4): F's viewer dies but the displaced session keeps running detached
                    // and resurfaces watch-only (external-lifecycle - viewport moved, nothing killed).
                    self.reap_pane(focus);
                    // An explicit open-here onto a portal seat
                    // repurposed its geometry for an ordinary attach: that
                    // portal no longer describes what the pane shows. Drop it
                    // so a later reach opens fresh instead of trusting an
                    // entry that names the wrong row. Only the portal
                    // seated on THIS pane is dropped; the rest are untouched.
                    if let Some(idx) = self
                        .portals
                        .iter()
                        .find(|(_, portal)| portal.seat == focus)
                        .map(|(idx, _)| *idx)
                    {
                        self.portals.remove(&idx);
                    }
                    // Persist B as a member of the viewed squad so it survives a
                    // restart pane-hosted (US2); the take-over already succeeded.
                    self.persist_attached_member(view.0, &id);
                    match &displaced {
                        Some(did) => self.notice(
                            client_id,
                            format!("opened here; {did} detached (watch-only)"),
                        ),
                        // Take-over: the reaped shell had no detached session to resurface.
                        None => self.notice(client_id, "took over tab"),
                    }
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
                let owner = self.session.find_by_cwd(&row_cwd).unwrap_or(view.0);
                // (G3) An anchored drop ("attach beside THIS pane") names a
                // concrete pane the operator can see, which overrides owner
                // routing: the pane lands in the anchor's OWN tab, resolved from
                // the anchor's live location, so the gesture places where it was
                // dropped rather than in the agent's home squad. A stale anchor is
                // refused pre-spawn, not mis-placed. A non-anchored attach (the
                // sideline click, `at: None`) keeps owner routing and the pre-v41
                // whole-tab placement untouched.
                //
                // An explicit UI target otherwise overrides owner routing for a
                // fresh attach (Locked 7); a stale/unknown target fails closed
                // with no spawn (AC4). Owner is always live, so resolution yields Some.
                let (dest, effective) = if let Some(anchor) = placement.at {
                    let Some((sid, ti)) = self.session.find_pane(anchor) else {
                        self.notice(client_id, "stale drop: that pane is gone");
                        return Flow::Continue;
                    };
                    let anchor_tid = self.session.squad(sid).expect("find_pane live").tabs[ti].id;
                    let mut p = placement.clone();
                    p.tab = Some(TabSel::Id(anchor_tid));
                    (Some(sid), p)
                } else {
                    let dest = match self.resolve_placement_target(&placement.target, Some(owner)) {
                        Ok(d) => d,
                        Err(e) => {
                            self.notice(client_id, e);
                            return Flow::Continue;
                        }
                    };
                    (dest, placement.clone())
                };
                let spawn_cwd = if row_cwd.is_empty() {
                    self.session
                        .squad(owner)
                        .map(|s| s.canonical_cwd().to_string())
                        .unwrap_or_default()
                } else {
                    row_cwd
                };
                // The argv comes from the canonical re-entry plan for
                // a claude row; `None` means the plan is resolving and this
                // gesture re-enters with the verdict staged.
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                let Some((argv, cd)) = self.attach_gesture_argv(client_id, &id, &placement) else {
                    return Flow::Continue;
                };
                let permit = match crate::process_admission::admit_pane(
                    self.placement_pane_count(dest, &effective),
                    effective.max_panes,
                ) {
                    Ok(permit) => permit,
                    Err(error) => {
                        self.notice(client_id, format!("attach failed: {error}"));
                        return Flow::Continue;
                    }
                };
                let pid =
                    match self.spawn_pane_cmd_with_permit(&argv, rows, cols, &spawn_cwd, permit) {
                        Ok(p) => p,
                        Err(e) => {
                            self.notice(client_id, format!("attach failed: {e}"));
                            return Flow::Continue;
                        }
                    };
                self.name_attached_pane(pid, &id, cd.as_deref());
                // Place through the shared v41 helper: it honors the anchored
                // drop's `tab`/`at` (a split beside the exact drop pane), and
                // otherwise falls through to place_spawned_pane's whole-tab
                // placement unchanged (`at: None` -> a new tab or a split beside
                // the selected squad's active-tab focus). A refusal reaps the pane
                // and leaves the row watch-only (AC7); the mapping is recorded
                // ONLY after placement succeeds.
                let (sid, tid, fell_back) = match self.place_with(dest, &spawn_cwd, pid, &effective)
                {
                    Ok(landing) => landing,
                    Err((_code, e)) => {
                        self.notice(client_id, e);
                        return Flow::Continue;
                    }
                };
                self.attached.insert(id.clone(), pid);
                // Persist the attached session as a member of its landing squad
                // so restore rebuilds its pane and the row renders pane-hosted
                // next session (US2) - the placement above already succeeded.
                self.persist_attached_member(sid, &id);
                self.set_view(client_id, sid, tid);
                if fell_back {
                    self.notice(client_id, "tab full - opened as tab");
                }
                self.push_layout(true);
                Flow::Continue
            }
            Command::ResumeAgent { name } => {
                // Resume a paneless row through its own harness.
                // Catalog gate first, the same posture as AttachAgent: the
                // name must match a surfaced row, and the SERVER re-derives
                // resumability (the client's Layout can be stale) - a row that
                // gained a live pane between publish and click is refused
                // here, never double-spawned. The gate walk lives in
                // [`Core::resume_one`], shared with the bulk restore
                // driver so the two surfaces cannot drift.
                let (rows, cols) = self
                    .clients
                    .iter()
                    .find(|c| c.id == client_id)
                    .map(|c| c.dims)
                    .unwrap_or((vp.rows, vp.cols));
                match self.resume_one(&name, None, client_id, view, (rows, cols), false) {
                    ResumeOutcome::Focused { pane, squad, tab } => {
                        self.set_view(client_id, squad, tab);
                        if let Some(tab) = self.viewed_tab_mut((squad, tab)) {
                            tab.focus = pane;
                        }
                        self.notice(client_id, "already resumed; focused existing pane");
                        self.push_layout(true);
                    }
                    ResumeOutcome::Refused { reason } => {
                        self.notice(client_id, reason);
                    }
                    // The claude re-entry plan fired off-loop; the
                    // ReentryPlanReady replay re-enters this command with the
                    // verdict staged. Nothing to report yet.
                    ResumeOutcome::PlanPending => {}
                    ResumeOutcome::Resumed {
                        pane,
                        squad,
                        tab,
                        notice,
                    } => {
                        if let Some(notice) = notice {
                            self.notice(client_id, notice);
                        }
                        self.set_view(client_id, squad, tab);
                        if let Some(tab) = self.viewed_tab_mut((squad, tab)) {
                            tab.focus = pane;
                        }
                        self.push_layout(true);
                        self.notice(client_id, format!("resumed {name}"));
                    }
                    // The gesture never dry-runs.
                    ResumeOutcome::Planned => {}
                }
                Flow::Continue
            }
            Command::DispatchNode { node, account } => {
                self.dispatch_card(client_id, node, account, false)
            }
            Command::DispatchPlan { node, account } => {
                self.dispatch_card(client_id, node, account, true)
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
                // Explicit tab rename. A stale/unknown id (the tab
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
                        let tid = t.id;
                        // Blank-after-sanitize CLEARS the rename back to the
                        // derived label (Locked 2: "reset to auto" is a
                        // meaningful rename target).
                        t.name = (!clean.is_empty()).then_some(clean);
                        self.push_layout(true);
                        // (US4) Persist so the chosen tab name survives a
                        // restart; a no-op for an unnamed/untracked squad.
                        self.persist_squad(sid);
                        // persist_squad preserves tab_specs byte-for-byte,
                        // so a template tab's stored spec would keep the OLD name
                        // key across this overlay rename - the same re-persist
                        // tab_rename does for the wire-API path. Guarded so a
                        // non-template tab causes no store churn.
                        if self.template_specs.contains_key(&tid) {
                            self.persist_template_specs(sid);
                        }
                    }
                    None => self.notice(client_id, "no such tab"),
                }
                Flow::Continue
            }
            Command::RenameSquad { squad, name } => {
                // Explicit squad rename. A stale/unknown id (the squad
                // died racing the overlay) is refused fail-closed with a notice,
                // like SelectSquad - no mutation.
                match self.session.squad(squad) {
                    Some(sq) => {
                        let clean = sanitize_name(&name, MAX_SQUAD_NAME);
                        // Blank-after-sanitize CLEARS back to the derived label.
                        if clean.is_empty() && self.clear_name_refused(squad, &sq.origins) {
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
                        // Write-through only for persisted (tracked) squads. A
                        // rename between two names is ONE atomic delete-old +
                        // upsert-new (so a concurrent restore never sees a window
                        // with neither). A CLEAR turns the workspace unnamed, but
                        // every squad persists now: drop the old NAME-keyed entry,
                        // then re-persist the squad under its ORIGINS key (an
                        // origin-less squad has no key, so `persist_squad` skips it
                        // and the clear was already refused above).
                        if let Some(members) = tracked_members {
                            match (old_name, new_name) {
                                (Some(old), Some(new)) => {
                                    let origins = self
                                        .session
                                        .squad(squad)
                                        .map(|s| s.origins.clone())
                                        .unwrap_or_default();
                                    let result = crate::squad_store::rename_with_generations(
                                        Some(&self.store_generations),
                                        &old,
                                        &new,
                                        &origins,
                                        &members,
                                    );
                                    self.persist_result(result);
                                }
                                (Some(old), None) => {
                                    self.persist_remove(&old, "");
                                    self.persist_squad(squad);
                                }
                                (None, Some(_)) => {
                                    // Unnamed -> named is a rename of an identity,
                                    // not a first persist: drop the old
                                    // key-keyed row, clear the live key, then
                                    // persist under the name. Falling through to a
                                    // plain persist upserted with the new name AND
                                    // the old key - `same_squad` matches the named
                                    // row by name alone, so the retain never removed
                                    // the old key row, and one rename left two live
                                    // rows, the new one violating the store's own
                                    // "named squads key by name" contract.
                                    let old_key = self
                                        .session
                                        .squad(squad)
                                        .map(|s| s.key.clone())
                                        .unwrap_or_default();
                                    if !old_key.is_empty() {
                                        self.persist_remove("", &old_key);
                                    }
                                    if let Some(s) = self.session.squad_mut(squad) {
                                        s.key = String::new();
                                    }
                                    self.persist_squad(squad);
                                }
                                // No old name and no new name: still unnamed, so
                                // just persist under the durable key.
                                (None, None) => self.persist_squad(squad),
                            }
                        }
                    }
                    None => self.notice(client_id, "no such squad"),
                }
                Flow::Continue
            }
            Command::RemoveSquad(id) => {
                // Close a whole workspace: reap every pane across all
                // its tabs, drop the squad, re-anchor views. Destructiveness is
                // gated client-side by a confirm; the server just executes.
                let Some(pos) = self.session.squads.iter().position(|s| s.id == id) else {
                    self.notice(client_id, "no such squad");
                    return Flow::Continue;
                };
                // De-persist the whole squad up front (user dismissed it - it
                // must not return at restart). Keyed by name when named, else by
                // the durable key (an unnamed lane persists too now). Reaping its
                // member panes below then no-ops on the store (entry + tracking gone).
                // UNCONDITIONAL: `squad_members` tracks recruited members, not
                // store presence - a squad the store holds but the session never
                // tracked (e.g. skipped by restore's per-squad isolation) took
                // the false branch and its row survived the dismiss.
                self.squad_members.remove(&id);
                if let Some((name, key)) = self.squad_identity(id) {
                    self.persist_remove(&name, &key);
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
                // Reorder the sideline. Pure presentation move: clamp
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
                // Re-home a whole tab into another squad. move_tab does
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
                let Some((current_squad, _)) = self.session.find_tab(tab) else {
                    self.notice(client_id, "no such tab");
                    return Flow::Continue;
                };
                if current_squad != squad {
                    self.notice(client_id, "tab moved to another workspace");
                    return Flow::Continue;
                }
                if self.reorder_tab(squad, tab, delta) {
                    self.push_layout(true);
                }
                Flow::Continue
            }
            Command::RecruitAgents { squad, ids } => {
                // Bulk recruit (US3). The server is the authoritative
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
                // The picker is N spawns under one gesture, so its
                // claude rows plan as a batch keyed by attach id - never the
                // single-verdict slot, which one replay could spend on the
                // wrong member. Every gate below is an idempotent read, so
                // the replay revalidates and skips whatever a partial earlier
                // pass already recruited. An off-axis-only selection plans
                // nothing and takes the legacy loop with no spawn.
                let wanted: Vec<(String, String)> = ids
                    .iter()
                    .filter_map(|id| self.attach_claude_row(id).map(|n| (id.clone(), n)))
                    .collect();
                if !wanted.is_empty()
                    && wanted
                        .iter()
                        .any(|(id, _)| !self.batch_plans.contains_key(id))
                {
                    self.resolve_plan_batch(
                        client_id,
                        wanted,
                        BatchReplay::Recruit {
                            squad: name.clone(),
                            ids: ids.clone(),
                        },
                    );
                    return Flow::Continue;
                }
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
                    let (acct, cd) = self.attach_account_ctx(id);
                    // A staged plan supplies the argv; a staged
                    // refusal skips the id with the reason named.
                    let (argv, dir) =
                        match self.staged_batch_argv(id, acct.as_deref(), cd.as_deref()) {
                            Ok(pair) => pair,
                            Err(reason) => {
                                skipped.push(format!("{id} ({reason})"));
                                continue;
                            }
                        };
                    let pid = match self.spawn_pane_cmd(&argv, rows, cols, &cwd) {
                        Ok(p) => p,
                        Err(e) => {
                            skipped.push(format!("{id} ({e})"));
                            continue;
                        }
                    };
                    self.name_attached_pane(pid, id, dir.as_deref());
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
                            tombstone_reason: None,
                            detached: false,
                            // persist_squad below re-derives the hosting tab name
                            // and the pane cwd.
                            tab_name: None,
                            cwd: None,
                            worker: None,
                            harness: None,
                            harness_session_id: None,
                            pane_id: None,
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
                // A member skipped at a gate above never consumed its
                // staged plan. Draining here keeps that verdict from being
                // consumed by a LATER recruit gesture as fresh (the
                // contains_key check would treat it as just-resolved).
                for id in &ids {
                    self.batch_plans.remove(id);
                }
                Flow::Continue
            }
            Command::DismissMember { squad, attach_id } => {
                // Dismiss a TOMBSTONED member from a persisted workspace (
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
            Command::StopAgent {
                name,
                harness_session_id,
                pane_id,
            } => {
                // Stop a live sideline row. Stop ONLY: this is the
                // menu's stop-only path; the one-gesture stop-and-remove
                // composition lives on RemoveAgent (scope b).
                // `resolve_lifecycle_target` validates against THIS server's
                // catalog fail-closed, identity first (v67). The subprocess
                // gets the row's CURRENT label, so a harness-side rename
                // between capture and keypress still reaches the right row.
                let resolved =
                    self.resolve_lifecycle_full(&name, harness_session_id.as_deref(), pane_id);
                match resolved {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(lifecycle_target::LifecycleTarget::Registry(owned)) => {
                        self.agent_action(client_id, "stop", owned)
                    }
                    Ok(lifecycle_target::LifecycleTarget::Pane(pid)) => {
                        self.stop_pane_child(client_id, pid, &name)
                    }
                }
                Flow::Continue
            }
            Command::RemoveAgent {
                name,
                harness_session_id,
                pane_id,
                measure: _,
            } => {
                // The operator states the intent ONCE: remove. The
                // server shells rm alone - the daemon's rm ends a live row's
                // process itself (law d-81c6da7e), and its refusal text is
                // what an unremovable row looks like. `measure` stays on the
                // wire for the protocol floor; the server no longer reads it.
                // Same resolution as StopAgent: the subprocess gets the
                // resolved row's current label.
                let resolved =
                    self.resolve_lifecycle_full(&name, harness_session_id.as_deref(), pane_id);
                match resolved {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(lifecycle_target::LifecycleTarget::Registry(owned)) => {
                        self.remove_agent_action(client_id, owned);
                    }
                    Ok(lifecycle_target::LifecycleTarget::Pane(pid)) => {
                        return self.remove_pane_row(client_id, pid, &name)
                    }
                }
                Flow::Continue
            }
            Command::RenameAgent { name, new_name } => {
                // Grammar first: a hostile token never reaches a subprocess
                // argv. The resolver then refuses unknown/external/ambiguous
                // rows like StopAgent; live AND exited rows are renamable.
                if !crate::registry_label::valid_agent_label(&new_name) {
                    self.notice(
                        client_id,
                        "label must be 1-64 letters, numbers, underscores, or hyphens",
                    );
                    return Flow::Continue;
                }
                match self.resolve_lifecycle_target(&name, None) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(_row) => self.agent_rename_action(client_id, name, new_name),
                }
                Flow::Continue
            }
            Command::PeekAgent { name, seq } => {
                // Read-only transcript fetch: shell `fno agents peek`
                // off-loop and reply to this client only. No catalog validation -
                // an unknown name returns peek's own exit-13 body, which the
                // overlay renders (fail-open, never a refusal notice).
                self.peek_agent(client_id, name, seq);
                Flow::Continue
            }
            Command::ReapAgents => {
                // Bulk-reap every exited fno-agent registry row. The
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
            Command::SweepDead => {
                // The sideline menu's global bulk action re-folds its target
                // set here, after confirmation, so a row that became live is
                // retained and a row that became unknown is not guessed dead.
                self.sweep_dead_sideline(client_id);
                Flow::Continue
            }
            Command::StopExternal { attach_id, name: _ } => {
                // Stop a live external claude-daemon row (or retry a failed/unknown
                // tombstone) by stable attach id. Validate the id names
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
                // Remove a STOPPED external tombstone by attach id. No
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
                // Free-text reply from peek (`m`). Resolve fail-closed
                // (mail to an EXITED row is legal - it queues durable; an external
                // row is refused), sanitize the text (blank/over-cap refused,
                // never truncated), then shell `fno agents mail send` off-loop.
                match self.resolve_lifecycle_target(&name, None) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(_) => match sanitize_mail_text(&text) {
                        Err(msg) => self.notice(client_id, msg),
                        Ok(clean) => self.mail_agent(client_id, name, clean),
                    },
                }
                Flow::Continue
            }
            Command::RespawnAgent { name } => {
                // Keep the target check fail-closed, then let the shared resume
                // door own its route and any row-state race.
                match self.resolve_lifecycle_target(&name, None) {
                    Err(msg) => self.notice(client_id, msg),
                    Ok(_) => self.resume_agent(client_id, name),
                }
                Flow::Continue
            }
            Command::BacklogVerb { node, verb } => {
                // A reorder verb from the Backlog section. Fail closed on
                // a node the server's own card set does not hold: a card that
                // raced out between menu-open and dispatch must launch no
                // subprocess (the same stale-target stance as the lifecycle
                // verbs). The argv is fixed by the `verb` enum, so nothing from
                // the wire composes a command line.
                if !self.backlog.iter().any(|c| c.id == node) {
                    self.notice(client_id, format!("{node}: no longer in the backlog"));
                    return Flow::Continue;
                }
                self.backlog_verb(client_id, node, verb);
                Flow::Continue
            }
            Command::CopySelection => {
                // Keyboard copy (prefix+y): the focused pane's selection, else the
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
            Command::RedrawPane { pane } => {
                // The repaint gesture for a garbled pane: nudge the
                // child's winsize so a SIGWINCH-respecting renderer repaints at
                // the settled size, then re-seed the pane's frame to every
                // viewer - the same flush-then-re-emit the push_layout reemit
                // pass does, scoped to one pane. `None` is the sender's viewed
                // tab's focus; a named pane is resolved session-wide and a
                // stale id is refused fail-closed, like FocusPane.
                let pid = match pane {
                    Some(p) => p,
                    None => match self.viewed_tab(view) {
                        Some(tab) => tab.focus,
                        None => return Flow::Continue,
                    },
                };
                let Some(entry) = self.panes.get(&pid) else {
                    self.notice(client_id, format!("{pid}: no such pane"));
                    return Flow::Continue;
                };
                let (rows, cols) = entry.vt.size();
                let frame = entry.vt.frame();
                entry.pty.nudge_winch(rows, cols);
                let mut seeded = 0usize;
                for c in &mut self.clients {
                    if c.visible.contains(&pid) {
                        // One composite per enqueue, matching broadcast_pane's
                        // per-viewing-client convention.
                        entry
                            .stats
                            .frames_composited
                            .fetch_add(1, Ordering::Relaxed);
                        let mut d = c.dirty.lock().unwrap();
                        d.insert(pid, frame.clone());
                        drop(d);
                        c.notify.notify_one();
                        seeded += 1;
                    }
                }
                e2e_log(format_args!(
                    "redraw pane {pid}: nudged {rows}x{cols}, re-seeded to {seeded} viewer(s)"
                ));
                self.notice(client_id, format!("pane {pid} repaint requested"));
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
        // Read-only enforcement at the server: drop any PTY/tree-mutating
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
            //, the same shared-state mutation as BlockNav; a read-only
            // observer must never jump everyone's viewport.
            | CoreMsg::Search { id, .. }
            // PaneAnswer injects a keystroke into a pane PTY; a
            // read-only observer must never reach it (same class as Input).
            | CoreMsg::PaneAnswer { id, .. }
            // DispatchNext spawns a real worker pane; a passive
            // web-bridge observer must never start work (Invariant).
            // DispatchResult is NOT gated here - it originates from the trusted
            // off-loop task, not a client. AgentLaunch (v83) spawns a real
            // worker the same way, so it gates identically.
            | CoreMsg::DispatchNext { id, .. }
            | CoreMsg::AgentLaunch { id, .. } => Some(*id),
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
                    // A submit key past the relay guard is a human pressing
                    // Enter: one operator_submit witness row (human_input).
                    if human_input::is_submit(&bytes) {
                        self.witness_submit(focus);
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
                    // Resize so it cannot enter the clamp (Invariant).
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
            CoreMsg::LinkHover {
                id,
                pane,
                row,
                col,
                seq,
            } => {
                self.link_hover(id, pane, row, col, seq);
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
                self.dispatch_next(id, None, account, false);
                Flow::Continue
            }
            CoreMsg::DispatchResult { id, notice } => {
                if !notice.is_empty() {
                    self.notice(id, notice);
                }
                Flow::Continue
            }
            CoreMsg::AgentLaunch { id, request } => {
                self.agent_launch(id, request);
                Flow::Continue
            }
            CoreMsg::AgentLaunchUpdate { id, update, retry } => {
                self.agent_launch_update(id, update, retry);
                Flow::Continue
            }
            // A gesture's canonical re-entry verdict landed. A
            // refusal is a one-line notice and nothing spawns; a resolution
            // re-dispatches the SAME command with the verdict staged, so every
            // gate re-runs against live state before the pane spawns.
            CoreMsg::ReentryPlanReady {
                id,
                request,
                verdict,
            } => {
                // A parked control-door reach finishes with its own
                // verdict: take it first, let the replay run on the observer
                // that stayed registered, then answer the held reply. Taken
                // with a restore, not a drop: a verdict for a DIFFERENT
                // client (a TUI gesture resolving while a reach is parked)
                // puts the park back.
                let parked = self.pending_thread_reply.take().and_then(|p| {
                    if p.client == id {
                        Some(p)
                    } else {
                        self.pending_thread_reply = Some(p);
                        None
                    }
                });
                match verdict {
                    Err(reason) => self.notice(id, reason),
                    Ok(verdict) => {
                        // Stage exactly one verdict; the receiving arm takes
                        // it at its argv construction. A vanished client
                        // (detached mid-resolution) drops the replay - the
                        // re-dispatch's command read refuses it.
                        self.reentry_verdict = Some(verdict);
                        match *request {
                            ReentrySpawnRequest::Attach {
                                attach_id,
                                placement,
                            } => {
                                self.command(
                                    id,
                                    Command::AttachAgent {
                                        id: attach_id,
                                        placement,
                                    },
                                );
                            }
                            ReentrySpawnRequest::Resume { name } => {
                                self.command(id, Command::ResumeAgent { name });
                            }
                            ReentrySpawnRequest::FocusHeld { pid } => {
                                self.command(id, Command::FocusPane(pid));
                            }
                        }
                        self.reentry_verdict = None;
                    }
                }
                if let Some(pending) = parked {
                    self.finish_pending_thread_reply(pending);
                }
                Flow::Continue
            }
            // A non-claude gesture's resolved argv landed. A refusal
            // is a one-line notice and nothing spawns; a resolution (or its
            // fail-open declared-form fallback, flagged `degraded`) re-runs
            // the SAME command with the argv staged, so every gate re-runs
            // against live state before the pane spawns. The degradation
            // notice fires even when the replay later refuses: the operator
            // asked for a resume and deserves the grant-loss news regardless.
            CoreMsg::RevivalGateAnswered {
                id,
                name,
                verdict,
                replay,
            } => {
                self.on_revival_gate_answered(id, name, verdict, replay);
                Flow::Continue
            }
            CoreMsg::ResumeArgvReady { id, argv, replay } => {
                let parked = self.pending_thread_reply.take().and_then(|p| {
                    if p.client == id {
                        Some(p)
                    } else {
                        self.pending_thread_reply = Some(p);
                        None
                    }
                });
                match argv {
                    Err(reason) => self.notice(id, reason),
                    Ok((argv, degraded)) => {
                        if degraded {
                            self.notice(
                                id,
                                "resume: resuming without the writable-roots grant \
                                 (resume-argv unavailable); a linked-worktree commit \
                                 may fail",
                            );
                        }
                        self.staged_resume_argv = Some(argv);
                        match *replay {
                            ResumeReplay::Gesture { name } => {
                                self.command(id, Command::ResumeAgent { name });
                            }
                            ResumeReplay::Held { pid } => {
                                self.command(id, Command::FocusPane(pid));
                            }
                        }
                        self.staged_resume_argv = None;
                    }
                }
                if let Some(pending) = parked {
                    self.finish_pending_thread_reply(pending);
                }
                Flow::Continue
            }
            // A batch's plans landed: stage them keyed by attach id
            // and re-enter the loop that asked. A refused entry keeps its
            // row and starts no pane (the consuming loop's own Err handling).
            CoreMsg::BatchPlansReady { id, plans, replay } => {
                self.batch_plans = plans;
                match *replay {
                    BatchReplay::Restore {
                        home_sid,
                        rows,
                        cols,
                    } => {
                        self.restore_squads(rows, cols, home_sid);
                        self.restore_pending = false;
                        // The external-tombstone reconcile runs
                        // AFTER restore, as the synchronous path orders it.
                        self.reconcile_external_lifecycle();
                        // A member vanished mid-resolution leaves its
                        // staged plan unconsumed; drop it so a later batch or
                        // recruit gesture resolves fresh, never stale.
                        self.batch_plans.clear();
                    }
                    BatchReplay::Recruit { squad, ids } => {
                        self.command(id, Command::RecruitAgents { squad, ids });
                    }
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
                //. A vanished client (detached before the read finished)
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
                // An off-loop external action / reconcile finished: swap
                // in the fresh render snapshot, re-push the sideline, and surface
                // the outcome (to one client for an action, to all for a
                // reconcile). This is the ONLY writer of `external_lifecycle` on
                // the core loop, so a stale action's late sync just re-renders
                // the durable truth it already re-read.
                //
                // Same shape as `AgentRows`/`BacklogCards` below: only
                // sideline data moved, rects are unchanged, so push without a
                // frame re-emit. `push_layout(true)` here flushed and reseeded
                // every visible pane's frame on every sync - a resize-storm-
                // shaped redraw across every viewed pane on a routine poll,
                // even when nothing in the record set actually changed.
                self.external_lifecycle = records;
                self.push_layout(false);
                for n in notices {
                    match to {
                        Some(cid) => self.notice(cid, n),
                        None => {
                            self.notice_all(n);
                        }
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
            CoreMsg::ServerStats { reply } => {
                let _ = reply.send(crate::server_stats::answer(
                    self.touch_emit_failures.load(Ordering::Relaxed),
                    &self.started_at,
                ));
                Flow::Continue
            }
            CoreMsg::Kill => {
                // Notify clients, then let the shared shutdown choke point
                // capture before killing non-keeper children. Keeper-held
                // panes outlive this server and are re-adopted by the next.
                self.bye_all("killed");
                Flow::Shutdown
            }
            CoreMsg::WorkspaceRestore {
                dry_run,
                harness,
                reply,
            } => {
                // The persisted squads reach memory only on the first real
                // attach (restore_squads). Answering before that would report
                // "nothing to restore" for a store that was never read - the
                // empty-success shape - so name the precondition instead.
                if !self.restored {
                    let _ = reply.send(ServerMsg::Err {
                        code: crate::proto::err_code::RESTORE_NOT_RUN,
                        msg: "startup restore has not run in this session yet: attach once \
                              (its first real attach reads the persisted workspace), then \
                              re-run"
                            .into(),
                    });
                    return Flow::Continue;
                }
                self.workspace_restore_start(dry_run, harness, reply);
                Flow::Continue
            }
            CoreMsg::WorkspaceRestoreApply {
                dry_run,
                harness,
                plans,
                headroom,
                reply,
            } => {
                self.workspace_restore_apply(dry_run, harness, plans, headroom, reply);
                Flow::Continue
            }
            CoreMsg::SquadReload { reply } => {
                self.handle_squad_reload(reply);
                Flow::Continue
            }
            CoreMsg::RetireSession {
                harness,
                session_id,
                reply,
            } => self.handle_retire_session(harness, session_id, reply),
            CoreMsg::Gone(id) => {
                // Gone is a geometry event (Locked 5, AC1-ERR): a vanished
                // constraining client releases its clamp, so the tab regrows
                // for the survivors in this same pass - Detach and an abrupt
                // socket death take the identical path.
                e2e_log(format_args!("client {id} gone"));
                self.clients.retain(|c| c.id != id);
                if self.clients.is_empty() {
                    // The last client just left: the push_layout below
                    // must flush the topology unconditionally, not on the
                    // debounce - there may be no later tick before shutdown.
                    self.last_topology_flush = None;
                }
                self.push_layout(true);
                Flow::Continue
            }
            // -- v4 control verbs. Each answers on its oneshot; a dropped
            // receiver (client vanished mid-verb) makes the send a no-op.
            CoreMsg::PaneLs { agents, reply } => {
                let _ = reply.send(self.pane_ls_from_fresh_agents(agents.as_deref()));
                Flow::Continue
            }
            CoreMsg::PaneRead {
                pane,
                lines,
                block,
                agents,
                reply,
            } => {
                let msg = match self.panes.get(&pane) {
                    Some(entry) => {
                        let pane_name = entry.name.clone();
                        let registry_fno_id = self
                            .fno_id_for_pane_with_agents(pane, agents.as_deref().unwrap_or(&[]));
                        match block {
                            // Block mode: `lines` is ignored; an unanswerable block
                            // is BLOCK_UNAVAILABLE, never empty/stale text.
                            Some(sel) => match entry.vt.read_block(sel) {
                                Ok(read) => ServerMsg::PaneText {
                                    pane_id: pane,
                                    text: read.text.clone(),
                                    block: Some(read.meta()),
                                    pane_name,
                                    registry_fno_id,
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
                                    pane_name,
                                    registry_fno_id,
                                }
                            }
                        }
                    }
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
                worker,
                reply,
            } => {
                let rows = rows.unwrap_or(vt::DEFAULT_ROWS);
                let cols = cols.unwrap_or(vt::DEFAULT_COLS);
                // Capture the exact-placement intent before `placement` moves
                // into run_pane, so the receipt can echo the committed context.
                // ANY selector placement (tab or anchor) now gets the
                // receipt, not just `--at current`: the bounded pane lane
                // verifies placement by re-reading `pane ls`, and this receipt
                // is the only record of where the server actually committed
                // the pane. Wire shape is unchanged (`placement` was already
                // `Option<ResolvedPlacement>`).
                let wants_receipt = placement.tab.is_some() || placement.at.is_some();
                let (anchor, direction, fallback_policy) =
                    (placement.at, placement.split, placement.fallback);
                let msg = match self
                    .run_pane(squad_key, cwd, argv, rows, cols, claim, placement, worker)
                {
                    Ok(pane_id) => {
                        let resolved = if wants_receipt {
                            // The new pane now sits in its committed squad+tab;
                            // read its real location back.
                            let (sid, tid, tab_name, tab_ordinal) = self
                                .session
                                .find_pane(pane_id)
                                .and_then(|(sid, ti)| {
                                    self.session
                                        .squad(sid)
                                        .and_then(|s| s.tab_dict(ti))
                                        .map(|d| (sid, d.tab_id, d.name, Some(d.ordinal)))
                                })
                                .unwrap_or((0, 0, None, None));
                            Some(ResolvedPlacement {
                                anchor: anchor.unwrap_or(0),
                                direction: direction.unwrap_or(Dir::Down),
                                fallback: fallback_policy,
                                squad: sid,
                                tab: tid,
                                tab_name,
                                tab_ordinal,
                            })
                        } else {
                            None
                        };
                        ServerMsg::PaneSpawned {
                            pane_id,
                            placement: resolved,
                        }
                    }
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneSend {
                pane,
                bytes,
                guarded,
                expected_identity,
                agents,
                reply,
            } => {
                let msg =
                    self.pane_send(pane, &bytes, guarded, expected_identity.as_deref(), agents);
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
            CoreMsg::PaneKill {
                pane,
                hand_off_to,
                reply,
            } => {
                if !self.panes.contains_key(&pane) {
                    let _ = reply.send(dead_pane(pane));
                    return Flow::Continue;
                }
                // A hand-off is the opposite of a kill: it RELEASES the pane
                // so its keeper keeps the child. It refuses before touching
                // the layout, so a failed rename leaves the pane as it was.
                if let Some(target) = hand_off_to {
                    let _ = reply.send(self.hand_off_pane(pane, &target));
                    return Flow::Continue;
                }
                // Reply Ok BEFORE propagating a possible session-ending
                // Shutdown, so the client always learns the kill landed even
                // when it closed the last pane.
                let flow = self.close_pane_reasoned(pane, "killed");
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
            // -- v41 layout script verbs. All reply inline; none moves
            //    a viewer's focus (a script split's no_focus defaults true). --
            CoreMsg::PaneSplit {
                pane,
                direction,
                no_focus,
                reply,
            } => {
                let msg = match self.split_pane_script(pane, direction, no_focus) {
                    Ok(pane_id) => ServerMsg::PaneSpawned {
                        pane_id,
                        placement: None,
                    },
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabLs { squad, reply } => {
                let msg = match self.tab_ls(&squad) {
                    Ok(tabs) => ServerMsg::TabList { tabs },
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabCreate { squad, name, reply } => {
                let msg = match self.tab_create(&squad, name) {
                    Ok(pane_id) => ServerMsg::PaneSpawned {
                        pane_id,
                        placement: None,
                    },
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabRename {
                squad,
                tab,
                name,
                reply,
            } => {
                let msg = match self.tab_rename(&squad, &tab, name) {
                    Ok(()) => ServerMsg::Ok,
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabReorder {
                squad,
                tab,
                to,
                reply,
            } => {
                let msg = match self.tab_reorder(&squad, &tab, &to) {
                    Ok(()) => ServerMsg::Ok,
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabClose {
                squad,
                tab,
                force,
                agents,
                reply,
            } => {
                let result = self.tab_close(&squad, &tab, force, agents.as_deref());
                let (msg, flow) = match result {
                    Ok((tab_id, pane_ids, outcome)) => (
                        ServerMsg::TabClosed {
                            tab_id,
                            pane_ids,
                            forced: force,
                        },
                        if matches!(outcome, RemoveOutcome::SessionEmpty) {
                            Flow::Shutdown
                        } else {
                            Flow::Continue
                        },
                    ),
                    Err((code, msg)) => (ServerMsg::Err { code, msg }, Flow::Continue),
                };
                let _ = reply.send(msg);
                flow
            }
            CoreMsg::LayoutGet {
                scope,
                workers,
                agents,
                reply,
            } => {
                // A worker join the caller asked for cannot silently degrade
                // to "every pane empty" when the registry read failed: that
                // would print the same receipt as a session full of idle
                // panes (the absence-versus-answer trap). Refuse instead.
                let msg = if workers && agents.is_none() {
                    ServerMsg::Err {
                        code: err_code::REGISTRY_UNAVAILABLE,
                        msg: "agent registry unavailable".into(),
                    }
                } else {
                    let agents = if workers { agents.as_deref() } else { None };
                    match self.layout_get(&scope, agents) {
                        Ok(squads) => ServerMsg::LayoutTree { squads },
                        Err((code, msg)) => ServerMsg::Err { code, msg },
                    }
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::AgentRowsGet { agents, reply } => {
                //  The receipt is the row set AS DERIVED, plus the
                // paint verdict per row. Never an empty-success: with no
                // in-memory rows AND an unreadable registry, the refusal says
                // so instead of a zero-row receipt.
                let msg = if self.agents.is_empty() && agents.is_none() {
                    ServerMsg::Err {
                        code: err_code::REGISTRY_UNAVAILABLE,
                        msg: "agent registry unavailable; no row set to publish".into(),
                    }
                } else {
                    ServerMsg::AgentRowsReceipt {
                        rows: self.agent_rows_receipt(),
                    }
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneWhere {
                fno_id,
                agents,
                reply,
            } => {
                let msg = self.pane_where_from_fresh_agents(&fno_id, agents.as_deref());
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabWhere {
                squad,
                sel,
                agents,
                reply,
            } => {
                let msg = self.tab_where_from_fresh_agents(&sel, &squad, agents.as_deref());
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneBreak { pane, name, reply } => {
                let msg = match self.pane_break(pane, name) {
                    Ok(tab_id) => {
                        let dict = self.session.find_tab(tab_id).and_then(|(sid, ti)| {
                            self.session.squad(sid).and_then(|s| s.tab_dict(ti))
                        });
                        ServerMsg::TabSpawned {
                            tab_id,
                            tab_name: dict.as_ref().and_then(|d| d.name.clone()),
                            tab_ordinal: dict.map(|d| d.ordinal),
                        }
                    }
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::PaneFocus { pane, reply } => {
                let msg = self.pane_focus(pane);
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::ThreadPane {
                name,
                portal,
                placement,
                agents,
                reply,
            } => {
                self.portal_ctl(&name, portal, placement, agents, reply);
                Flow::Continue
            }
            CoreMsg::ReseatPane {
                pane,
                portal,
                reply,
            } => {
                let msg = self.reseat_pane_into_portal(pane, portal);
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::TabJoin {
                src_tab,
                anchor_pane,
                direction,
                reply,
            } => {
                let msg = match self.tab_join(&src_tab, anchor_pane, direction) {
                    Ok(()) => ServerMsg::Ok,
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::LayoutApply {
                squad,
                tab,
                spec,
                focus,
                reply,
            } => {
                let msg = match self.layout_apply(&squad, &tab, &spec, focus) {
                    Ok(results) => ServerMsg::LayoutApplied { results },
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::LayoutGraft {
                squad,
                anchor,
                spec,
                focus,
                reply,
            } => {
                let msg = match self.layout_graft(&squad, anchor, &spec, focus) {
                    Ok(msg) => msg,
                    Err((code, msg)) => ServerMsg::Err { code, msg },
                };
                let _ = reply.send(msg);
                Flow::Continue
            }
            CoreMsg::AgentTails { tails, ctx } => {
                self.tail_by_session = tails;
                self.ctx_by_session = ctx;
                self.push_layout(false);
                Flow::Continue
            }
            CoreMsg::AgentTruth { map, seq } => {
                // A late-arriving older probe must not clobber a fresher one
                // (review finding: probes run concurrently off-loop with no
                // ordering guarantee between completions).
                if seq <= self.truth_seq {
                    return Flow::Continue;
                }
                self.truth_seq = seq;
                self.truth_by_name = map;
                // Only the attention order and the age column moved: re-push
                // the layout without re-emitting frames.
                self.push_layout(false);
                Flow::Continue
            }
            CoreMsg::AgentRows {
                rows,
                branches,
                tails,
                ctx,
                read_ok,
            } => {
                self.agents_read_ok = read_ok;
                let identity_published = self.worker_identity_published(&rows);
                // Discoverability, once per server lifetime: the
                // first paneless live row to appear with no portal open names
                // the gesture. "Explicit and lazy" must not mean
                // "undiscoverable". A notice is not state - the latch only
                // mutes repetition, it never gates behavior. The
                // notice names both doors, because opening a SECOND portal is
                // the gesture an operator cannot guess from the first.
                if !self.portal_noticed
                    && self.portals.is_empty()
                    && rows.iter().any(|a| a.mux.is_none() && !a.exited)
                {
                    // Latch on DELIVERY, not on the attempt: workers
                    // register before an operator attaches in the ordinary
                    // daemon startup, so a latch set with no client attached
                    // would burn the once-per-lifetime notice on nobody.
                    if self.notice_all(
                        "thread row present: reach a row (Enter or click) to open portal 0; P opens another portal beside it",
                    ) {
                        self.portal_noticed = true;
                    }
                }
                self.agents = rows;
                self.branch_by_cwd = branches;
                self.tail_by_session = tails;
                self.ctx_by_session = ctx;
                // Row changes are the journal's change signal: a
                // spawn or removal writes both. Refresh the cached scan here,
                // off the per-push paths that read it.
                self.journal.refresh();
                if identity_published {
                    // A registry row can publish after a worker pane was
                    // recorded. Force the existing debounce funnel to flush
                    // the newly authoritative identity instead of waiting for
                    // another topology mutation.
                    self.mark_topology_dirty();
                }
                // The debounce flush for topology captures: a drag's
                // events mark dirty without writing; the 1s registry tick is
                // the coalescing timer that turns them into one store write.
                self.flush_topology();
                // The registry just refreshed: a queued template restore
                // whose fno bindings now resolve gets applied here, binding live
                // sessions instead of shells.
                self.drain_template_restores();
                // Rects are unchanged; only the Layout's agent rows moved -
                // push without re-emitting frames (AC1-UI: visible within one
                // layout push; AC2-UI: the read happened off-loop).
                self.push_layout(false);
                Flow::Continue
            }
            CoreMsg::BacklogCards {
                cards,
                lanes,
                stale,
                holders,
                prs,
                drivers,
            } => {
                // Same as AgentRows: only sideline data moved, so push the
                // Layout without a frame re-emit.
                self.backlog = cards;
                self.backlog_lanes = lanes;
                self.backlog_stale = stale;
                self.backlog_holders = holders;
                self.backlog_pr = prs;
                self.backlog_driver = drivers;
                self.push_layout(false);
                Flow::Continue
            }
            CoreMsg::PaneStatsTick => {
                self.emit_pane_stats();
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

    /// Kill every pane child EXCEPT keeper-hosted ones. Serve's shared
    /// shutdown choke point owns this call so every graceful exit captures
    /// first. PtyShell has no Drop that kills its child, so skipping this
    /// leaves pane children to SIGHUP; a worker that ignores it keeps running.
    ///
    /// The keeper carve-out is the load-bearing line: a keeper-hosted pane's
    /// child outlives this server BY DESIGN, and a shutdown sweep that kills
    /// it silently converts every restart back into the crashes the keeper
    /// exists to stop. Deliberate close still kills - that is `reap_pane`'s
    /// `pty.kill()`, which the Keeper variant honors. Any future reaper that
    /// sweeps pane children on shutdown must keep this exact shape: plain
    /// panes die, keeper-hosted panes survive for the next server to
    /// re-adopt.
    fn kill_all_panes(&self) {
        for entry in self.panes.values() {
            if matches!(entry.pty, PtyShell::Keeper(_)) {
                continue;
            }
            entry.pty.kill();
        }
    }
}

/// (US4) Join a resolved branch and a cwd tail into a sideline subline.
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

/// The harness's own title for the session joins the subline when it
/// differs from the row's label: the sideline keeps showing fno's label as the
/// row name (a Ctrl+R rename never rewrites it), and the title rides the same
/// dim slot so the rename is visible where the worker works.
fn subline_with_title(a: &agents_view::RegistryAgent, base: Option<String>) -> Option<String> {
    match a.harness_title.as_deref().filter(|t| *t != a.name) {
        Some(t) => Some(match base {
            Some(b) => format!("{t} · {b}"),
            None => t.to_string(),
        }),
        // No title, or the title already equals the label: the base subline
        // stands alone rather than a filter swallowing the whole slot.
        None => base,
    }
}

/// (US3) The cwd basename carried on EVERY agent row (not just orphans),
/// so the sideline can flag a foreign-cwd join client-side by comparing it to
/// the squad's project basename. `None` for an empty cwd (no subline is
/// fabricated - the AC4-EDGE "absent cwd" case); a path with no final component
/// falls back to the whole cwd, matching the pre-change orphan extraction.
fn cwd_basename(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    Some(
        Path::new(cwd)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or(cwd)
            .to_string(),
    )
}

/// The canonical `Err` for a pane id no live pane owns (read/send/wait/kill).
fn dead_pane(pane: u64) -> ServerMsg {
    ServerMsg::Err {
        code: err_code::DEAD_PANE,
        msg: format!("no such pane: {pane}"),
    }
}

/// The prefix+y copy source precedence (epic Locked 6 / cv-4ac072b6): the active
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
                        if done_watch.baseline.is_none_or(|b| seq > b) {
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

fn drain_pty_output(
    core: &mut Core,
    out_rx: &mut mpsc::Receiver<(u64, PaneChunk)>,
    first: Option<(u64, PaneChunk)>,
    e2e_first_out: &mut HashSet<u64>,
) -> bool {
    let mut drained = false;
    let mut touched = HashSet::new();
    {
        let mut feed = |pid: u64, chunk: PaneChunk| {
            drained = true;
            match chunk {
                PaneChunk::Output(bytes) => {
                    if e2e_first_out.insert(pid) {
                        e2e_log(format_args!(
                            "core loop: first output from pane {pid} ({} bytes)",
                            bytes.len()
                        ));
                    }
                    if let Some(entry) = core.panes.get_mut(&pid) {
                        let t0 = Instant::now();
                        entry.vt.feed(&bytes);
                        entry.last_output = t0;
                        entry
                            .stats
                            .bytes_in
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        entry.stats.grid_updates.fetch_add(1, Ordering::Relaxed);
                        entry
                            .stats
                            .cpu_ns
                            .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        touched.insert(pid);
                    }
                }
                PaneChunk::Resized(rows, cols) => {
                    // The keeper's resize round trip landed: apply the VT
                    // dimension change here, at its exact point in this
                    // pane's own ordered channel, never eagerly when the
                    // resize was issued (server.rs's push_layout). Any
                    // trailing pre-resize output is necessarily ahead of
                    // this marker in the same channel, so it is always fed
                    // before the resize lands.
                    if let Some(entry) = core.panes.get_mut(&pid) {
                        entry.vt.resize(rows, cols);
                        touched.insert(pid);
                    }
                }
            }
        };
        if let Some((pid, chunk)) = first {
            feed(pid, chunk);
        }
        while let Ok((pid, chunk)) = out_rx.try_recv() {
            feed(pid, chunk);
        }
    }
    for pid in touched {
        core.broadcast_pane(pid);
        core.note_pane_output(pid);
    }
    if drained {
        core.sync_focused_modes();
    }
    drained
}

async fn serve(
    listener: std::os::unix::net::UnixListener,
    socket: &Path,
    session_name: String,
    pane_children: PaneChildRoster,
    mut signal_rx: mpsc::Receiver<CoreMsg>,
    shutdown_complete: Arc<AtomicBool>,
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
    let (out_tx, mut out_rx) = mpsc::channel::<(u64, PaneChunk)>(256);
    let (exit_tx, mut exit_rx) = mpsc::channel::<u64>(64);
    let (core_tx, mut core_rx) = mpsc::channel::<CoreMsg>(256);
    // Attached-client count for the periodic readers: Core owns the
    // sender; each reader holds a receiver as its work gate + 0->1 wakeup.
    let (client_count_tx, client_count_rx) = watch::channel(0usize);
    let persisted_pane_floor = crate::squad_store::load().next_pane_id;
    let initial_agents = read_guard_agents().await;

    let mut core = Core {
        session: Session::default(),
        panes: HashMap::new(),
        pane_watch: HashMap::new(),
        pane_stats: Arc::new(RwLock::new(HashMap::new())),
        pane_stats_emit_failures: Arc::new(AtomicU64::new(0)),
        pane_children,
        clients: Vec::new(),
        next_pane_id: pane_id_floor(
            persisted_pane_floor,
            initial_agents.as_deref().unwrap_or(&[]),
        ),
        next_squad_id: 1,
        tab_areas: HashMap::new(),
        session_name,
        shells: shell_candidates(std::env::var_os("SHELL").as_deref()),
        out_tx,
        exit_tx,
        self_tx: core_tx.clone(),
        agents: Vec::new(),
        agents_read_ok: false,
        journal: crate::spawn_journal::JournalCache::load(),
        launch_desk: Default::default(),
        branch_by_cwd: HashMap::new(),
        tail_by_session: HashMap::new(),
        ctx_by_session: HashMap::new(),
        truth_by_name: HashMap::new(),
        truth_seq: 0,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
        backlog_holders: HashMap::new(),
        backlog_pr: HashMap::new(),
        backlog_driver: HashMap::new(),
        claim_eligible: HashSet::new(),
        claims: HashMap::new(),
        touch_last_emit: HashMap::new(),
        wheel_gate: HashMap::new(),
        touch_emit_failures: Arc::new(AtomicU64::new(0)),
        started_at: crate::server_stats::stamp_now(),
        client_count: client_count_tx,
        seen: HashSet::new(),
        attached: HashMap::new(),
        worker_pane: HashMap::new(),
        worker_session_pane: HashMap::new(),
        held_workers: HashMap::new(),
        detached_panes: HashMap::new(),
        diff_pane: None,
        portals: BTreeMap::new(),
        portal_noticed: false,
        squad_members: HashMap::new(),
        template_specs: HashMap::new(),
        pending_template_restores: Vec::new(),
        external_lifecycle: Vec::new(),
        persist_degraded_notified: false,
        shared_identity_notified: HashSet::new(),
        restored: false,
        restore_pending: false,
        store_generations: HashMap::new(),
        pre_restore_squads: HashSet::new(),
        topology_dirty: false,
        last_topology_flush: None,
        reentry_verdict: None,
        staged_resume_argv: None,
        revival_admission: None,
        batch_plans: HashMap::new(),
        pending_thread_reply: None,
        keeper_adopted: Vec::new(),
        shell_rc_dirs: HashMap::new(),
        portal_session_guards: BTreeMap::new(),
    };

    // The off-loop registry reader (4a-G2): a 1s interval task stats/reads
    // BOTH the fno-agents registry AND claude's daemon roster on the
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
            // Carried across ticks so the tail pass can run even when
            // the row set did not move: the uuid set to look up, and the last
            // map pushed, so an unchanged result stays off the wire.
            let mut last_uuids: Vec<(String, Option<String>)> = Vec::new();
            let mut last_tails: HashMap<String, String> = HashMap::new();
            let mut last_ctx: HashMap<String, String> = HashMap::new();
            // Shared so the path cache survives across blocking-pool passes.
            let tail_reader = std::sync::Arc::new(std::sync::Mutex::new(
                crate::transcript_tail::TailReader::new(),
            ));
            let mut last_truth = Instant::now();
            // Shared with the detached probe tasks: clear means no probe is
            // in flight. Held by [`TruthProbeLatch`], which clears it on drop.
            let truth_in_flight = Arc::new(AtomicBool::new(false));
            // Logged ONCE on the first daemon miss, never per
            // tick: the fallback is a fact about the environment, and a fleet
            // with no daemon must not pay one line per second for it.
            let mut fallback_logged = false;
            // Same once-only discipline for the wedged-probe skip below.
            let mut latch_wedge_logged = false;
            // (v48) Launch order for AgentTruth probes, so an out-of-order
            // completion cannot clobber a fresher result (see CoreMsg::AgentTruth).
            let mut truth_probe_seq: u64 = 0;
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
                // Gate the registry+roster read on an attached client.
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
                // (v48) Reachability evidence, one `fno agents list --json`
                // process for the whole fleet on a slow sub-interval. Each
                // probe runs as its own task so a slow CLI start never stalls
                // the 1s registry tick; a failed probe sends nothing and the
                // last good map stands. The latch skips a tick whose
                // predecessor still runs, so a probe slower than the interval
                // stacks no second interpreter. Ages are measured at probe
                // time, so between probes a row's displayed age lags by at
                // most this interval - invisible next to the 600s threshold
                // it feeds.
                if last_truth.elapsed() >= TRUTH_PROBE_EVERY {
                    match TruthProbeLatch::begin(&truth_in_flight) {
                        Some(latch) => {
                            last_truth = Instant::now();
                            truth_probe_seq += 1;
                            let seq = truth_probe_seq;
                            let tx = core_tx.clone();
                            tokio::spawn(async move {
                                let probe = tokio::task::spawn_blocking(probe_truth_map)
                                    .await
                                    .ok()
                                    .flatten();
                                drop(latch);
                                if let Some(map) = probe {
                                    let _ = tx.send(CoreMsg::AgentTruth { map, seq }).await;
                                }
                            });
                        }
                        None => {
                            // A probe still running a full interval after it
                            // started is wedged, not slow. The old code healed
                            // that by stacking a new probe; the latch cannot,
                            // so say so once instead of silently freezing
                            // every row's age at the last good reading.
                            if !latch_wedge_logged {
                                latch_wedge_logged = true;
                                eprintln!(
                                    "fno mux: a fleet truth probe has run over {TRUTH_PROBE_EVERY:?}; \
                                     skipping probes until it exits"
                                );
                            }
                        }
                    }
                }
                // The registry leg subscribes to the daemon
                // when its socket answers (AC12): rows are SERVED, the stamp
                // domain is the same (mtime, len) the file scan gated with, and
                // an unchanged answer costs one stat server-side and one small
                // frame here - no file read on either side. When nothing
                // answers, the file scan below stands (AC13, the supported
                // no-daemon shape), logged once.
                let (reg_stamp, reg_raw) = match agents_view::watch_registry(
                    state.reg_stamp(),
                    &agents_view::supervisor_sock_path(),
                )
                .await
                {
                    Ok(answer) => answer,
                    Err(_) => {
                        if !fallback_logged {
                            fallback_logged = true;
                            eprintln!(
                                "fno mux: no agent daemon answering at {}; reading the registry file directly (degraded fallback)",
                                agents_view::supervisor_sock_path().display()
                            );
                        }
                        scan(reg_path.clone(), state.reg_stamp()).await
                    }
                };
                let (roster_stamp, roster_raw) =
                    scan(roster_path.clone(), state.roster_stamp()).await;
                // Each registered isolated account's roster.json, folded
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
                let changed = state.tick(
                    reg_stamp,
                    move || reg_raw,
                    roster_stamp,
                    move || roster_raw,
                    isolated,
                    now,
                );
                if let Some(rows) = &changed {
                    // The tail key is the row's transcript identity: the claude
                    // uuid where one exists, else the harness session id (the
                    // rollout uuid a codex row carries). log_path rides beside
                    // it - a row naming its own transcript is read directly.
                    last_uuids = rows
                        .iter()
                        .filter_map(|r| {
                            let uuid = r
                                .claude_session_uuid
                                .clone()
                                .or_else(|| r.harness_session_id.clone())?;
                            Some((uuid, r.log_path.clone()))
                        })
                        .collect();
                }
                // Tails resolve on EVERY tick (a transcript grows
                // with no registry change); TailReader re-reads known paths
                // each tick and paces only the discovery walk, which at 1Hz
                // was most of this loop's CPU.
                let uuids = last_uuids.clone();
                let reader = tail_reader.clone();
                let (tails, ctx) = tokio::task::spawn_blocking(move || {
                    // Poison-recover rather than expect: a panic in one pass
                    // must not blank the column on every later tick (the
                    // cache is a HashMap, safe to reuse mid-poison).
                    reader
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .tails_and_ctx(&uuids)
                })
                .await
                .unwrap_or_default();
                if let Some(rows) = changed {
                    // (US4) Resolve the git branch per UNIQUE row cwd,
                    // off the core loop, on the blocking pool - bounded file
                    // reads only, per-cwd degradation on failure (AC1-FR). This
                    // rides the change-gated emit: a branch only moves when the
                    // row set does, so the reads stay off idle ticks.
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
                    last_tails = tails.clone();
                    last_ctx = ctx.clone();
                    if core_tx
                        .send(CoreMsg::AgentRows {
                            rows,
                            branches,
                            tails,
                            ctx,
                            read_ok: state.read_ok(),
                        })
                        .await
                        .is_err()
                    {
                        return; // core loop gone; the server is shutting down
                    }
                } else if tails != last_tails || ctx != last_ctx {
                    // Rows unchanged but somebody said something: push the tails
                    // alone rather than forcing a whole row set through.
                    last_tails = tails.clone();
                    last_ctx = ctx.clone();
                    if core_tx
                        .send(CoreMsg::AgentTails { tails, ctx })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }

    crate::board_reader::spawn(core_tx.clone(), client_count_rx.clone());

    // The per-pane counter cadence: a fixed 30s tick telling the core to
    // snapshot and emit. Delay (not Burst) on a missed tick - counters are
    // monotonic totals, so one late sample costs nothing a difference needs.
    {
        let core_tx = core_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PANE_STATS_CADENCE);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if core_tx.send(CoreMsg::PaneStatsTick).await.is_err() {
                    return; // Core dropped; server shutting down
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
    let accept_stats = core.pane_stats.clone();
    let conns_alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accept_conns = conns_alive.clone();
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
                    let conn_core_tx = accept_core_tx.clone();
                    let conn_resolver = resolver.clone();
                    let conn_stats = accept_stats.clone();
                    // Count from the accept itself, not the task's first poll:
                    // a scheduler-starved newborn task would otherwise leave
                    // the reaper a mid-verb window with conns_alive == 0.
                    let alive = ConnAlive::new(&accept_conns);
                    tokio::spawn(async move {
                        let _alive = alive;
                        handle_client(stream, conn_core_tx, conn_resolver, id, conn_stats).await;
                    });
                }
                Err(e) => {
                    eprintln!("fno mux: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    eprintln!("fno mux: serving {}", socket.display());

    // Re-adopt surviving keeper panes BEFORE any attach can restore: this
    // runs synchronously before the loop drains its first CoreMsg, so an
    // early attach's restore sees adopted panes as already-live members.
    core.keeper_readopt();

    // FNO_E2E idle-exit reaper (Fix 2): the ONLY reaper that survives
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
    let mut pane_reap_tick = tokio::time::interval(Duration::from_secs(1));
    pane_reap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval`'s first tick is immediate; consume it so the first scan lands
    // after one interval instead of duplicating startup's known-live state.
    pane_reap_tick.tick().await;

    // Build-drift retirement: the watch stats its own executable
    // off-loop every 5th tick and retires through Flow::Shutdown only on a
    // drifted verdict at a fully quiet tick. The machinery lives in
    // server/drift_retire.rs.
    let mut drift_watch = drift_retire::RetireWatch::new();

    // diagnostics: which panes' output the CORE LOOP has seen. Pairs
    // with the pty reader thread's own first-chunk line to split "shell never
    // spoke" from "core loop never drained it".
    let mut e2e_first_out: HashSet<u64> = HashSet::new();
    // Explicit e2e-only fault seam: after a real pane registration, park the
    // core-loop thread forever so SIGTERM cannot be handled by this loop.
    let core_wedge_e2e = std::env::var_os("FNO_E2E_CORE_WEDGE").is_some();
    let mut core_wedge_armed = false;

    let flow = loop {
        tokio::select! {
            chunk = out_rx.recv() => {
                // out_tx lives in Core, so recv never yields None.
                let Some((pid, item)) = chunk else { break Flow::Shutdown };
                drain_pty_output(
                    &mut core,
                    &mut out_rx,
                    Some((pid, item)),
                    &mut e2e_first_out,
                );
                // Pane output is a liveness signal: re-arm the idle
                // reaper so a working client-less script session never reaps.
                if idle_exit_e2e {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
            }
            exited = exit_rx.recv() => {
                let Some(pid) = exited else { break Flow::Shutdown };
                e2e_log(format_args!("pane {pid} child exited"));
                // The reader sends this only after enqueuing every output
                // chunk. Drain that channel before removing the pane so final
                // bytes cannot lose a select race between separate receivers.
                if drain_pty_output(&mut core, &mut out_rx, None, &mut e2e_first_out)
                    && idle_exit_e2e
                {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
                // A worker that died on its own (churn) tombstones its member
                // BEFORE the reap clears the mapping (AC4-EDGE).
                let ctx = core.member_ctx(pid);
                core.reconcile_member_close(ctx, true);
                if core.close_viewer_died(pid, "viewer exited") == Flow::Shutdown {
                    e2e_log(format_args!("last pane gone; shutting down"));
                    break Flow::Shutdown;
                }
            }
            _ = pane_reap_tick.tick() => {
                // Deferred repaint requests ride the same 1s pass.
                core.fire_due_nudges();
                core.follow_portal_viewer_titles();
                // Snapshot first: reader completion guarantees all output for
                // these panes was enqueued before this point. Drain it, then
                // close exactly the snapshot even if another reader finishes
                // concurrently.
                let dead = core.dead_children_ready_to_reap();
                if !dead.is_empty()
                    && drain_pty_output(&mut core, &mut out_rx, None, &mut e2e_first_out)
                    && idle_exit_e2e
                {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
                if core.reap_dead_children(dead) == Flow::Shutdown {
                    e2e_log(format_args!("last dead pane reaped; shutting down"));
                    break Flow::Shutdown;
                }
                // Drift retirement: a drifted verdict at a fully
                // quiet tick (no panes, clients, or connections) retires the
                // server so the next attach spawns the installed build. The
                // stat runs off-loop in server/drift_retire.rs.
                if let Some((running, on_disk)) = drift_watch.tick(|| {
                    core.panes.is_empty()
                        && *core.client_count.borrow() == 0
                        && conns_alive.load(Ordering::Acquire) == 0
                }) {
                    eprintln!(
                        "fno mux: stale-build retire: on-disk binary changed ({} -> {}); \
                         no panes, clients, or connections; retiring so the next \
                         attach spawns the installed build",
                        running.path.display(),
                        on_disk.path.display()
                    );
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
                if core_wedge_e2e && !core_wedge_armed && !core.panes.is_empty() {
                    core_wedge_armed = true;
                    e2e_log(format_args!("core wedge armed"));
                    std::thread::park();
                }
            }
            signal = signal_rx.recv() => {
                let Some(signal) = signal else { break Flow::Shutdown };
                if core.handle(signal) == Flow::Shutdown {
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
            // The reaper: enabled only under FNO_E2E. On grace with no
            // activity, reap iff no client is attached — a still-attached
            // session is in use, so re-arm and keep serving. Replicate the
            // CoreMsg::Kill teardown (kill every pane PTY + Flow::Shutdown so
            // SocketGuard unlinks); NEVER std::process::exit, which would
            // orphan the pane shells and leak the socket file.
            _ = tokio::time::sleep_until(idle_deadline), if idle_exit_e2e => {
                if *idle_count_rx.borrow() == 0
                    && conns_alive.load(std::sync::atomic::Ordering::Acquire) == 0
                {
                    eprintln!("fno mux: idle-exit (FNO_E2E): no client for grace window");
                    break Flow::Shutdown;
                }
                idle_deadline = tokio::time::Instant::now() + idle_grace;
            }
        }
        // Loop-tail choke point: the out_rx/exit_rx arms mutate
        // `clients` via the dead-client sweeps (broadcast_pane /
        // sync_focused_modes / close_pane) without a `handle()` call, so the
        // handle-tail publish alone would leave the count stale on exactly
        // the orphan path the readers gate on.
        core.publish_client_count();
    };
    if flow == Flow::Shutdown {
        // Capture only from a safe restore state and current store generation.
        core.capture_topology_now();
        core.kill_all_panes();
        core.bye_all("session ended");
        // Give writer tasks a beat to flush the Byes; a lost Bye reads as
        // "session ended (server closed)" client-side, so this is best-effort.
        tokio::time::sleep(BYE_FLUSH).await;
    }
    shutdown_complete.store(true, Ordering::Release);
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

/// The classified twin of [`read_guard_agents`], for the guarded-`PaneSend`
/// lane only: same io, but the raw read goes through
/// [`classify_guard_registry`], so a row-level malformation refuses the send
/// instead of reading as "no row for that pane". The tolerant twin stays as-is
/// on purpose - the display verbs render what they can read and must not blank
/// a whole sideline over one bad row.
async fn read_guard_agents_for_send() -> Result<Vec<RegistryAgent>, &'static str> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let read =
        tokio::task::spawn_blocking(|| std::fs::read_to_string(agents_view::registry_path())).await;
    match read {
        Ok(Ok(raw)) => classify_guard_registry(&raw, now),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Ok(Err(_)) => Err("agents registry unreadable - target agent state unknown"),
        Err(_) => Err("agents registry read failed - target agent state unknown"),
    }
}

/// Return every pane/worker pair that blocks an unforced tab close. Matching
/// is exact on the server session and pane id, and every joined row whose
/// liveness is not positively `Dead` is destructive-risk evidence. Rows that
/// have not received an effective identity yet use their registry name in the
/// diagnostic instead of being silently treated as empty.
fn tab_close_blockers(session: &str, pane_ids: &[u64], rows: &[RegistryAgent]) -> Vec<String> {
    let mut blockers = Vec::new();
    for &pane_id in pane_ids {
        for row in rows.iter().filter(|row| {
            row.mux.as_ref().is_some_and(|(row_session, row_pane)| {
                row_session == session && *row_pane == pane_id
            })
        }) {
            if row.liveness != agents_view::Liveness::Dead {
                let (label, value) = match row.effective_identity() {
                    Some(identity) => ("fno_id", identity),
                    None => ("worker", row.name.as_str()),
                };
                blockers.push(format!(
                    "pane {pane_id} {label}={value} liveness={:?}",
                    row.liveness
                ));
            }
        }
    }
    blockers
}

fn pane_id_floor(persisted: u64, agents: &[RegistryAgent]) -> u64 {
    let registry_floor = agents
        .iter()
        .filter_map(|agent| agent.mux.as_ref().map(|(_, pane)| pane.saturating_add(1)))
        .max()
        .unwrap_or(1);
    persisted.max(registry_floor).max(1)
}

/// Does registry row `a` carry `id` as a FULL `session_id` or `harness_session_id`?
fn identity_exact(a: &RegistryAgent, id: &str) -> bool {
    a.session_id.as_deref() == Some(id) || a.harness_session_id.as_deref() == Some(id)
}

/// Does `id` PREFIX either of row `a`'s identity spellings ? The `where`
/// convenience; the caller rejects an ambiguous prefix (2+ distinct identities).
fn identity_prefix(a: &RegistryAgent, id: &str) -> bool {
    let hit = |s: &Option<String>| s.as_deref().is_some_and(|v| v.starts_with(id));
    hit(&a.session_id) || hit(&a.harness_session_id)
}

/// Substitute slot-index leaves for real pane ids: `templates::topology`
/// returns a `Node` whose leaves ARE slot indices; this maps each to its
/// resolved pane. `map` is a bijection over the slots present, so the result has
/// unique leaves (the tree invariant).
fn substitute_leaves(shape: &Node, map: &std::collections::HashMap<u64, u64>) -> Node {
    match shape {
        Node::Leaf(slot) => Node::Leaf(*map.get(slot).unwrap_or(slot)),
        Node::Branch { axis, children } => Node::Branch {
            axis: *axis,
            children: children
                .iter()
                .map(|(r, c)| (*r, substitute_leaves(c, map)))
                .collect(),
        },
    }
}

/// A flat evenly-weighted row of panes (shell-spawn-failure fallback): a
/// single pane is a bare leaf, else a horizontal branch. Ratios sum to exactly
/// 1.0 (last absorbs the float remainder).
fn flat_row(panes: &[u64]) -> Node {
    if panes.len() == 1 {
        return Node::Leaf(panes[0]);
    }
    let n = panes.len();
    let each = 1.0 / n as f32;
    let mut ratios = vec![each; n];
    let rest: f32 = ratios[..n - 1].iter().sum();
    ratios[n - 1] = 1.0 - rest;
    Node::Branch {
        axis: Axis::Horizontal,
        children: ratios
            .into_iter()
            .zip(panes)
            .map(|(r, &p)| (r, Node::Leaf(p)))
            .collect(),
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
        ControlVerb::PaneLs => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::PaneLs {
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneRead { pane, lines, block } => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::PaneRead {
                    pane,
                    lines,
                    block,
                    agents,
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
            worker,
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
                    worker,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneSend {
            pane,
            bytes,
            guarded,
            expected_identity,
        } => {
            // A guarded send reads the agents registry FRESH here, off the core
            // loop: the server's own overlay cache (`self.agents`) is parked
            // whenever no client is attached, so a headless one-shot block-pipe
            // would otherwise guard against a stale/empty snapshot and inject
            // into a busy agent. Reading on the server (its own registry path)
            // is what closes the client/server HOME-divergence gap; passing the
            // snapshot into the core loop keeps the check + inject atomic.
            let agents = if guarded || expected_identity.is_some() {
                // The classified read: an unattributable row refuses here,
                // before the core loop, instead of reading as "no agent".
                read_guard_agents_for_send().await
            } else {
                // Every script send still consults DND. A plain read suffices:
                // an unreadable registry maps to no rows observed, and a raw
                // send with no positive DND marker stays unguarded.
                Ok(read_guard_agents().await.unwrap_or_default())
            };
            core_tx
                .send(CoreMsg::PaneSend {
                    pane,
                    bytes,
                    guarded,
                    expected_identity,
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
        ControlVerb::PaneKill { pane, hand_off_to } => {
            core_tx
                .send(CoreMsg::PaneKill {
                    pane,
                    hand_off_to,
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
        // -- v41 layout script verbs --
        ControlVerb::PaneSplit {
            pane,
            direction,
            no_focus,
        } => {
            core_tx
                .send(CoreMsg::PaneSplit {
                    pane,
                    direction,
                    no_focus,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabLs { squad } => {
            core_tx
                .send(CoreMsg::TabLs {
                    squad,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabCreate { squad, name } => {
            core_tx
                .send(CoreMsg::TabCreate {
                    squad,
                    name,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabRename { squad, tab, name } => {
            core_tx
                .send(CoreMsg::TabRename {
                    squad,
                    tab,
                    name,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabReorder { squad, tab, to } => {
            core_tx
                .send(CoreMsg::TabReorder {
                    squad,
                    tab,
                    to,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabClose { squad, tab, force } => {
            let agents = if force {
                None
            } else {
                read_guard_agents().await
            };
            core_tx
                .send(CoreMsg::TabClose {
                    squad,
                    tab,
                    force,
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::LayoutGet { scope, workers } => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::LayoutGet {
                    scope,
                    workers,
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::AgentRowsGet => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::AgentRowsGet {
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneWhere { fno_id } => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::PaneWhere {
                    fno_id,
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabWhere { squad, sel } => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::TabWhere {
                    squad,
                    sel,
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneBreak { pane, name } => {
            core_tx
                .send(CoreMsg::PaneBreak {
                    pane,
                    name,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::WorkspaceRestore { dry_run, harness } => {
            core_tx
                .send(CoreMsg::WorkspaceRestore {
                    dry_run,
                    harness,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::SquadReload => core_tx.send(CoreMsg::SquadReload { reply: reply_tx }).await,
        ControlVerb::ServerStats => core_tx.send(CoreMsg::ServerStats { reply: reply_tx }).await,
        ControlVerb::RetireSession {
            harness,
            session_id,
        } => {
            core_tx
                .send(CoreMsg::RetireSession {
                    harness,
                    session_id,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::PaneFocus { pane } => {
            core_tx
                .send(CoreMsg::PaneFocus {
                    pane,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::ThreadPane {
            name,
            portal,
            placement,
        } => {
            let agents = read_guard_agents().await;
            core_tx
                .send(CoreMsg::ThreadPane {
                    name,
                    // An absent index is portal 0, where every
                    // pre-v64 caller landed.
                    portal: portal.unwrap_or(0),
                    placement,
                    agents,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::ThreadReseat { pane, portal } => {
            core_tx
                .send(CoreMsg::ReseatPane {
                    pane,
                    portal,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::TabJoin {
            src_tab,
            anchor_pane,
            direction,
        } => {
            core_tx
                .send(CoreMsg::TabJoin {
                    src_tab,
                    anchor_pane,
                    direction,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::LayoutApply {
            squad,
            tab,
            spec,
            focus,
        } => {
            core_tx
                .send(CoreMsg::LayoutApply {
                    squad,
                    tab,
                    spec,
                    focus,
                    reply: reply_tx,
                })
                .await
        }
        ControlVerb::LayoutGraft {
            squad,
            anchor,
            spec,
            focus,
        } => {
            core_tx
                .send(CoreMsg::LayoutGraft {
                    squad,
                    anchor,
                    spec,
                    focus,
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

/// Recover enough of an undecodable `Control` envelope to answer a newer
/// floor-compatible client instead of closing silently on an unknown verb.
fn unknown_control_refusal(value: &serde_json::Value) -> Option<ServerMsg> {
    let control = value.get("Control")?.as_object()?;
    let proto = u32::try_from(control.get("proto")?.as_u64()?).ok()?;
    let build = control.get("build")?.as_str()?;
    if let Err(reason) = check_attach_version(proto, build) {
        return Some(ServerMsg::Err {
            code: err_code::VERSION_SKEW,
            msg: reason,
        });
    }
    let verb = control
        .get("verb")
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.keys().next())
        .map(String::as_str)
        .unwrap_or("unknown");
    Some(ServerMsg::Err {
        code: err_code::BAD_REQUEST,
        msg: format!(
            "unknown control verb {verb:?}; server {} speaks wire v{}",
            crate::proto::BUILD_VERSION,
            crate::proto::PROTO_VERSION
        ),
    })
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
    stats: PaneStats,
) {
    let first = tokio::time::timeout(
        ATTACH_TIMEOUT,
        read_msg::<_, serde_json::Value>(&mut stream),
    )
    .await;
    let first = match first {
        Ok(Ok(value)) => match <ClientMsg as serde::Deserialize>::deserialize(&value) {
            Ok(message) => message,
            Err(error) => {
                if let Some(reply) = unknown_control_refusal(&value) {
                    let _ = write_msg(&mut stream, &reply).await;
                } else {
                    eprintln!("fno mux: initial client message failed to decode: {error}");
                    // The refusal above recovers every envelope shape this
                    // build knows. An envelope it cannot interpret at all
                    // must not restore the silent close this branch removed:
                    // even a hopeless decode gets a loud refusal, so drift
                    // surfaces as a client-side error, never a dead socket.
                    let _ = write_msg(
                        &mut stream,
                        &ServerMsg::Err {
                            code: err_code::BAD_REQUEST,
                            msg: format!(
                                "undecodable first message; server {} speaks wire v{}",
                                crate::proto::BUILD_VERSION,
                                crate::proto::PROTO_VERSION
                            ),
                        },
                    )
                    .await;
                }
                return;
            }
        },
        _ => return,
    };
    let (rows, cols, cwd) = match first {
        ClientMsg::Attach {
            proto,
            build,
            rows,
            cols,
            cwd,
        } => {
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
        ClientMsg::Query => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if core_tx.send(CoreMsg::Query(reply_tx)).await.is_ok() {
                if let Ok(info) = reply_rx.await {
                    let _ = write_msg(&mut stream, &info).await;
                }
            }
            return;
        }
        ClientMsg::KillServer => {
            let _ = core_tx.send(CoreMsg::Kill).await;
            return;
        }
        // A v4 one-shot control connection (`fno mux pane ...`): versioned
        // like Attach, answered with exactly one reply, then closed. Never
        // registers a client, never splits into reader/writer tasks.
        ClientMsg::Control { proto, build, verb } => {
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
    tokio::spawn(client_writer(write_half, reliable_rx, dirty, notify, stats));
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
            Ok(ClientMsg::LinkHover {
                pane,
                row,
                col,
                seq,
            }) => {
                if core_tx
                    .send(CoreMsg::LinkHover {
                        id,
                        pane,
                        row,
                        col,
                        seq,
                    })
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
            Ok(ClientMsg::AgentLaunch(request)) => {
                if core_tx
                    .send(CoreMsg::AgentLaunch { id, request })
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

/// Count one `Frame` that actually crossed a client wire. A frame dropped by
/// the newest-wins dirty map never reaches here - that asymmetry against
/// `frames_composited` is the measurement. A miss (pane reaped mid-drain)
/// skips silently: that pane's counters row is already gone by design.
fn count_frame_emitted(stats: &PaneStats, pane_id: u64) {
    if let Some(c) = stats.read().unwrap().get(&pane_id) {
        c.frames_emitted.fetch_add(1, Ordering::Relaxed);
    }
}

async fn write_reliable<W>(
    w: &mut W,
    msg: &ServerMsg,
    dirty: &DirtyMap,
    stats: &PaneStats,
) -> Result<bool, ProtoError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let is_bye = matches!(msg, ServerMsg::Bye { .. });
    if is_bye {
        let mut pending: Vec<(u64, Frame)> = dirty.lock().unwrap().drain().collect();
        pending.sort_unstable_by_key(|(pane_id, _)| *pane_id);
        for (pane_id, frame) in pending {
            write_msg(w, &ServerMsg::Frame { pane_id, frame }).await?;
            count_frame_emitted(stats, pane_id);
        }
    }
    write_msg(w, msg).await?;
    if let ServerMsg::Frame { pane_id, .. } = msg {
        count_frame_emitted(stats, *pane_id);
    }
    Ok(is_bye)
}

/// The per-client writer: reliable messages FIRST (biased select - a Layout
/// is never stuck behind a frame burst), then the droppable dirty map. `Bye`
/// is the exception: it flushes the final dirty frames before ending the
/// stream. A write failure exits this half; the reader owns deregistration so
/// commands already on the socket stay ordered before its `Gone` message.
async fn client_writer(
    mut w: OwnedWriteHalf,
    mut reliable_rx: mpsc::Receiver<ServerMsg>,
    dirty: DirtyMap,
    notify: Arc<Notify>,
    stats: PaneStats,
) {
    loop {
        tokio::select! {
            biased;
            msg = reliable_rx.recv() => {
                let Some(msg) = msg else { break }; // deregistered by the core
                match write_reliable(&mut w, &msg, &dirty, &stats).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => break,
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
                    while let Ok(msg) = reliable_rx.try_recv() {
                        match write_reliable(&mut w, &msg, &dirty, &stats).await {
                            Ok(true) => return,
                            Ok(false) => {}
                            Err(_) => return,
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
                        return;
                    }
                    count_frame_emitted(&stats, pane_id);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
