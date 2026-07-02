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
use tokio::sync::{mpsc, Notify};

use crate::proto::{
    bind_or_probe, check_attach_version, read_msg, write_msg, BindOutcome, ClientMsg, Command,
    Frame, ServerMsg, SquadMeta, TabMeta,
};
use crate::pty::{shell_candidates, PtyShell};
use crate::squad::{self, RemoveOutcome, Resolver, Session};
use crate::tree::{self, Axis, Node, Rect, Tab};
use crate::vt::{self, Modes};

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

/// What connected clients register with the core loop.
enum CoreMsg {
    Attach {
        id: u64,
        rows: u16,
        cols: u16,
        /// Already resolved to the canonical squad key by `handle_client`'s
        /// own task - the blocking git run never touches the core loop.
        squad_key: String,
        reliable_tx: mpsc::Sender<ServerMsg>,
        dirty: DirtyMap,
        notify: Arc<Notify>,
    },
    /// Raw bytes for the FOCUSED pane (server-global focus).
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    Command {
        id: u64,
        cmd: Command,
    },
    Gone(u64),
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
    runtime.block_on(serve(listener, &socket))
}

// ---------------------------------------------------------------------------
// Core state + mutations (all on the core loop)
// ---------------------------------------------------------------------------

struct Core {
    session: Session,
    panes: HashMap<u64, PaneEntry>,
    clients: Vec<Client>,
    /// Monotonic, never reused (Locked Decision 6).
    next_pane_id: u64,
    next_squad_id: u64,
    /// Last-attach/resize-wins content-area geometry (per-client views are
    /// Phase 3).
    viewport: (u16, u16),
    /// Panes of the active tab: the frame-emission gate. Inactive tabs' grids
    /// are still fed; their frames never cross the wire (AC5-EDGE).
    visible: HashSet<u64>,
    shells: Vec<OsString>,
    out_tx: mpsc::Sender<(u64, Vec<u8>)>,
    exit_tx: mpsc::Sender<u64>,
}

impl Core {
    fn viewport_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            rows: self.viewport.0,
            cols: self.viewport.1,
        }
    }

    fn spawn_pane(&mut self, rows: u16, cols: u16) -> Result<u64, String> {
        let id = self.next_pane_id;
        let pty = PtyShell::spawn(
            &self.shells,
            rows,
            cols,
            id,
            self.out_tx.clone(),
            self.exit_tx.clone(),
        )
        .map_err(|e| e.to_string())?;
        self.next_pane_id += 1;
        self.panes.insert(
            id,
            PaneEntry {
                pty,
                vt: vt::Pane::new(rows, cols),
            },
        );
        Ok(id)
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

    fn attach(
        &mut self,
        id: u64,
        rows: u16,
        cols: u16,
        key: String,
        reliable_tx: mpsc::Sender<ServerMsg>,
        dirty: DirtyMap,
        notify: Arc<Notify>,
    ) {
        self.viewport = (rows, cols);
        match self.session.find_by_cwd(&key) {
            Some(sid) => {
                // Existing squad: the attach lands IN it (AC6-HP, worktree
                // rollup) and makes it active (last-wins, like geometry).
                self.session.active_squad = Some(sid);
            }
            None => {
                // Fresh squad: PTY spawn FIRST (Locked 7), then the model.
                match self.spawn_pane(rows, cols) {
                    Ok(pid) => {
                        let sid = self.next_squad_id;
                        self.next_squad_id += 1;
                        self.session.add_squad(
                            sid,
                            key,
                            Tab {
                                root: Node::Leaf(pid),
                                focus: pid,
                            },
                        );
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
        }
        self.clients.push(Client {
            id,
            reliable_tx,
            dirty,
            notify,
            synced_modes: Modes::default(),
        });
        self.push_layout(true);
    }

    /// The layout-change protocol (Locked 4): resize PTYs/grids to the new
    /// rects, then per client: ModeSync (if the focused pane's modes differ
    /// from what that client's terminal last saw), flush stale frame slots,
    /// send `Layout`, and (when `reemit`) queue full frames for every visible
    /// pane. Focus-only changes pass `reemit: false` - rects are unchanged,
    /// so queued frames stay valid.
    fn push_layout(&mut self, reemit: bool) {
        let vp = self.viewport_rect();
        let Some(active_tab) = self.session.active_tab() else {
            return;
        };
        let rects = tree::layout(&active_tab.root, vp);
        let focus = active_tab.focus;

        // Rect-driven pane sizing: only geometry that actually changed hits
        // the PTY, so a resize storm's no-op tail is free (AC3-FR's bounded-
        // update half; the storm's head is coalesced at the channel).
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
        self.visible = rects.iter().map(|(pid, _)| *pid).collect();

        let layout_msg = self.layout_msg(&rects, focus);
        let focused_modes = self
            .panes
            .get(&focus)
            .map(|e| e.vt.modes())
            .unwrap_or_default();

        let mut dead = Vec::new();
        for c in &mut self.clients {
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
            if c.reliable_tx.try_send(layout_msg.clone()).is_err() {
                dead.push(c.id);
                continue;
            }
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
    }

    fn layout_msg(&self, rects: &[(u64, Rect)], focus: u64) -> ServerMsg {
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
                tabs: (1..=s.tabs.len())
                    .map(|i| TabMeta {
                        name: i.to_string(),
                    })
                    .collect(),
                active_tab: s.active_tab,
            })
            .collect();
        ServerMsg::Layout {
            squads,
            active_squad: self.session.active_squad.unwrap_or(0),
            panes: rects.to_vec(),
            focus,
        }
    }

    /// Fan one pane's fresh frame out to every client's dirty slot - but only
    /// when the pane is on the visible (active) tab.
    fn broadcast_pane(&self, pid: u64) {
        if !self.visible.contains(&pid) || self.clients.is_empty() {
            return;
        }
        let Some(entry) = self.panes.get(&pid) else {
            return;
        };
        let frame = entry.vt.frame();
        for c in &self.clients {
            c.dirty.lock().unwrap().insert(pid, frame.clone());
            c.notify.notify_one();
        }
    }

    /// Live mode changes in the FOCUSED pane (vim toggling mouse reporting
    /// mid-session) must reach client terminals now, not at the next focus
    /// change. Cheap: a flag read per output burst, bytes only on a diff.
    fn sync_focused_modes(&mut self) {
        let Some(tab) = self.session.active_tab() else {
            return;
        };
        let Some(entry) = self.panes.get(&tab.focus) else {
            return;
        };
        let modes = entry.vt.modes();
        let mut dead = Vec::new();
        for c in &mut self.clients {
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
    }

    /// Close one pane: kill+reap its PTY, remove it from the tree (collapse +
    /// focus re-anchor inside `tree::close`), cascade empty tab -> squad ->
    /// session (Locked 8). Idempotent: an unknown pane (double-close race,
    /// AC4-ERR) is a no-op.
    fn close_pane(&mut self, pid: u64) -> Flow {
        let Some((sid, ti)) = self.session.find_pane(pid) else {
            // Unknown to the tree; still reap a stray registry entry so a
            // half-created pane can never leak a child process.
            if let Some(entry) = self.panes.remove(&pid) {
                entry.pty.kill();
            }
            return Flow::Continue;
        };
        if let Some(entry) = self.panes.remove(&pid) {
            entry.pty.kill();
        }
        let vp = self.viewport_rect();
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
                self.push_layout(true);
                Flow::Continue
            }
        }
    }

    fn command(&mut self, client_id: u64, cmd: Command) -> Flow {
        let vp = self.viewport_rect();
        match cmd {
            Command::SplitH | Command::SplitV => {
                let axis = if matches!(cmd, Command::SplitH) {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let Some(tab) = self.session.active_tab() else {
                    return Flow::Continue;
                };
                // Spawn at the focused pane's current size; the layout pass
                // right after resizes both halves to their real rects.
                let (rows, cols) = self
                    .panes
                    .get(&tab.focus)
                    .map(|e| e.vt.size())
                    .unwrap_or(self.viewport);
                let pid = match self.spawn_pane(rows, cols) {
                    Ok(p) => p,
                    Err(e) => {
                        // AC1-ERR: nothing mutated yet - the tree is
                        // untouched by construction (spawn-first ordering).
                        self.notice(client_id, format!("split failed: {e}"));
                        return Flow::Continue;
                    }
                };
                let Some(tab) = self.session.active_tab_mut() else {
                    return Flow::Continue;
                };
                match tree::split(tab, vp, axis, pid) {
                    Ok(()) => self.push_layout(true),
                    Err(e) => {
                        // AC1-EDGE: refused split reaps the pre-spawned
                        // shell; the tree was never touched.
                        if let Some(entry) = self.panes.remove(&pid) {
                            entry.pty.kill();
                        }
                        self.notice(client_id, e.to_string());
                    }
                }
                Flow::Continue
            }
            Command::ClosePane => {
                let Some(tab) = self.session.active_tab() else {
                    return Flow::Continue;
                };
                self.close_pane(tab.focus)
            }
            Command::FocusDir(dir) => {
                let Some(tab) = self.session.active_tab_mut() else {
                    return Flow::Continue;
                };
                match tree::navigate(&tab.root, vp, tab.focus, dir) {
                    Some(next) => {
                        tab.focus = next;
                        self.push_layout(false);
                    }
                    None => self.notice(client_id, "no pane in that direction"),
                }
                Flow::Continue
            }
            Command::ResizeDir(dir) => {
                let Some(tab) = self.session.active_tab_mut() else {
                    return Flow::Continue;
                };
                if tree::resize(tab, vp, dir, tree::RESIZE_STEP) {
                    self.push_layout(true);
                } else {
                    // AC3-ERR/EDGE: BEL only when nothing changed.
                    self.notice(client_id, "cannot resize further");
                }
                Flow::Continue
            }
            Command::NewTab => {
                let (rows, cols) = self.viewport;
                let pid = match self.spawn_pane(rows, cols) {
                    Ok(p) => p,
                    Err(e) => {
                        // AC5-ERR: stay on the current tab, error visible.
                        self.notice(client_id, format!("new tab failed: {e}"));
                        return Flow::Continue;
                    }
                };
                let Some(squad) = self.session.active_mut() else {
                    return Flow::Continue;
                };
                squad.tabs.push(Tab {
                    root: Node::Leaf(pid),
                    focus: pid,
                });
                squad.active_tab = squad.tabs.len() - 1;
                self.push_layout(true);
                Flow::Continue
            }
            Command::SelectTab(i) => {
                let Some(squad) = self.session.active_mut() else {
                    return Flow::Continue;
                };
                if i < squad.tabs.len() {
                    squad.active_tab = i;
                    self.push_layout(true);
                } else {
                    // Fail-closed no-op with feedback (Boundaries).
                    self.notice(client_id, "no such tab");
                }
                Flow::Continue
            }
            Command::NextTab | Command::PrevTab => {
                let Some(squad) = self.session.active_mut() else {
                    return Flow::Continue;
                };
                let n = squad.tabs.len();
                if n < 2 {
                    self.notice(client_id, "no other tab");
                    return Flow::Continue;
                }
                squad.active_tab = if matches!(cmd, Command::NextTab) {
                    (squad.active_tab + 1) % n
                } else {
                    (squad.active_tab + n - 1) % n
                };
                self.push_layout(true);
                Flow::Continue
            }
            Command::CloseTab => {
                let Some(squad) = self.session.active() else {
                    return Flow::Continue;
                };
                let (sid, ti) = (squad.id, squad.active_tab);
                let pids = tree::leaves(&squad.tabs[ti].root);
                for pid in pids {
                    if let Some(entry) = self.panes.remove(&pid) {
                        entry.pty.kill();
                    }
                }
                match self.session.remove_tab(sid, ti) {
                    RemoveOutcome::SessionEmpty => Flow::Shutdown,
                    _ => {
                        self.push_layout(true);
                        Flow::Continue
                    }
                }
            }
            Command::SelectSquad(id) => {
                if self.session.squad(id).is_some() {
                    self.session.active_squad = Some(id);
                    self.push_layout(true);
                } else {
                    // AC6-FR: a stale id (squad died racing the selector) is
                    // refused fail-closed; the client re-anchors off the next
                    // Layout it already received.
                    self.notice(client_id, "no such squad");
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
                squad_key,
                reliable_tx,
                dirty,
                notify,
            } => {
                self.attach(id, rows, cols, squad_key, reliable_tx, dirty, notify);
                Flow::Continue
            }
            CoreMsg::Input(bytes) => {
                // Fail closed when there is no live focused pane: dropped,
                // never a panic. A write error means the child just exited
                // mid-keystroke - same policy; the exit signal follows.
                if let Some(tab) = self.session.active_tab() {
                    if let Some(entry) = self.panes.get(&tab.focus) {
                        let _ = entry.pty.write_input(&bytes);
                    }
                }
                Flow::Continue
            }
            CoreMsg::Resize { rows, cols } => {
                self.viewport = (rows, cols);
                self.push_layout(true);
                Flow::Continue
            }
            CoreMsg::Command { id, cmd } => self.command(id, cmd),
            CoreMsg::Gone(id) => {
                self.clients.retain(|c| c.id != id);
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

async fn serve(listener: std::os::unix::net::UnixListener, socket: &Path) -> i32 {
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
        clients: Vec::new(),
        next_pane_id: 1,
        next_squad_id: 1,
        viewport: (crate::vt::DEFAULT_ROWS, crate::vt::DEFAULT_COLS),
        visible: HashSet::new(),
        shells: shell_candidates(std::env::var_os("SHELL").as_deref()),
        out_tx,
        exit_tx,
    };

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
                // Coalesce resize storms: only the final geometry hits the
                // PTYs (AC3-FR). Other messages drained here run after, in
                // arrival order.
                if let CoreMsg::Resize { mut rows, mut cols } = msg {
                    let mut pending = Vec::new();
                    while let Ok(m) = core_rx.try_recv() {
                        match m {
                            CoreMsg::Resize { rows: r, cols: c } => { rows = r; cols = c; }
                            other => pending.push(other),
                        }
                    }
                    let mut flow = core.handle(CoreMsg::Resize { rows, cols });
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
        // Liveness probes connect and vanish; malformed first messages and
        // timeouts close the same way: without touching any pane.
        _ => return,
    };

    // Resolve the squad key HERE, on this connection's task: the git run
    // blocks (bounded 2s), and the core loop must never wait on it. Cache
    // check first; a miss runs off the async threads via spawn_blocking.
    // Two racing misses on one cwd both resolve and insert the same
    // idempotent answer - cheaper than a lock held across a subprocess.
    // The guard must drop before the await below (a match-scrutinee
    // temporary would live across it and un-Send the future).
    let cached = resolver.lock().unwrap().cached(&cwd);
    let squad_key = match cached {
        Some(hit) => hit,
        None => {
            let owned = cwd.clone();
            let key = tokio::task::spawn_blocking(move || squad::resolve_key(&owned))
                .await
                .unwrap_or_else(|_| cwd.clone());
            resolver.lock().unwrap().insert(cwd, key.clone());
            key
        }
    };

    let (reliable_tx, reliable_rx) = mpsc::channel::<ServerMsg>(RELIABLE_CAP);
    let dirty: DirtyMap = Arc::default();
    let notify = Arc::new(Notify::new());
    if core_tx
        .send(CoreMsg::Attach {
            id,
            rows,
            cols,
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
                if core_tx.send(CoreMsg::Input(bytes)).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Resize { rows, cols }) => {
                if core_tx.send(CoreMsg::Resize { rows, cols }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Command(cmd)) => {
                if core_tx.send(CoreMsg::Command { id, cmd }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Detach) => {
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
            // A second Attach on a live connection is a protocol violation:
            // log it (this stderr is the session log) and close rather than
            // acting on a confused stream.
            Ok(ClientMsg::Attach { .. }) => {
                eprintln!("fno mux: client {id} sent Attach on a live connection; dropping it");
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
