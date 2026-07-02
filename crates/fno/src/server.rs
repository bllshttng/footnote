//! The mux server: owns one PTY-backed shell + its VT grid, streams
//! self-contained frames to attached clients over the session socket.
//!
//! Concurrency shape (the epic's locked channel discipline):
//! - client -> server input/control rides bounded mpsc channels that are
//!   AWAITED - never dropped. Backpressure flows to the socket, then the
//!   client.
//! - server -> client render frames ride a per-client `watch` slot: capacity
//!   1, newest-wins, each frame self-contained. A slow client coalesces
//!   frames; it can never stall the core loop or its peers (AC4-ERR).
//! - The PTY master is blocking, so reads live on a dedicated thread
//!   (`pty.rs`) feeding the core loop's channel; tokio stays at the edges.
//!
//! The server is the single source of truth for the grid. It outlives every
//! client: attach/detach/kill -9 of a client never touches the PTY (AC4-HP).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use crate::proto::{
    bind_or_probe, check_attach_version, read_msg, write_msg, BindOutcome, ClientMsg, Frame,
    ServerMsg,
};
use crate::pty::{shell_candidates, PtyShell};
use crate::vt::{Pane, DEFAULT_COLS, DEFAULT_ROWS};

/// A silent connection (e.g. a liveness probe) gets this long to Attach
/// before the server closes it.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// What connected clients register with the core loop.
enum CoreMsg {
    Attach {
        id: u64,
        rows: u16,
        cols: u16,
        frame_tx: watch::Sender<Option<Frame>>,
    },
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    Gone(u64),
}

struct Client {
    id: u64,
    frame_tx: watch::Sender<Option<Frame>>,
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

    // The pane: one shell on one PTY, emulated into one grid.
    let (pty_tx, mut pty_rx) = mpsc::channel::<Vec<u8>>(256);
    let candidates = shell_candidates(std::env::var_os("SHELL").as_deref());
    let pty = match PtyShell::spawn(&candidates, DEFAULT_ROWS, DEFAULT_COLS, pty_tx) {
        Ok(p) => p,
        Err(e) => {
            // AC1-ERR terminal case: nothing spawnable. Exit non-zero with a
            // one-line cause; the client's failed attach surfaces it.
            eprintln!("fno mux: cannot start a shell: {e}");
            return 1;
        }
    };
    let mut vt = Pane::new(DEFAULT_ROWS, DEFAULT_COLS);

    let (core_tx, mut core_rx) = mpsc::channel::<CoreMsg>(256);

    // Accept loop: handshake each connection off the core loop's back.
    let accept_core_tx = core_tx.clone();
    tokio::spawn(async move {
        let ids = Arc::new(AtomicU64::new(1));
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let id = ids.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(handle_client(stream, accept_core_tx.clone(), id));
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

    let mut clients: Vec<Client> = Vec::new();
    let mut child_exited = false;

    eprintln!("fno mux: serving {}", socket.display());
    loop {
        tokio::select! {
            chunk = pty_rx.recv(), if !child_exited => {
                match chunk {
                    Some(bytes) => {
                        vt.feed(&bytes);
                        // Coalesce whatever is already queued into one frame.
                        while let Ok(more) = pty_rx.try_recv() {
                            vt.feed(&more);
                        }
                        broadcast(&vt, &clients);
                    }
                    None => {
                        // Reader thread ended: the child is gone. Render the
                        // exited state into the grid (AC2-ERR) and idle-exit
                        // once no client is watching.
                        child_exited = true;
                        vt.feed(b"\r\n[fno: pane exited]\r\n");
                        broadcast(&vt, &clients);
                        if clients.is_empty() {
                            break;
                        }
                    }
                }
            }
            msg = core_rx.recv() => {
                // core_tx lives in the accept loop, so recv never yields None.
                let Some(msg) = msg else { break };
                match msg {
                    CoreMsg::Attach { id, rows, cols, frame_tx } => {
                        // Last attach wins geometry (Phase 1: one client at a
                        // time; multi-client polish is Phase 3).
                        if !child_exited {
                            log_resize_err(pty.resize(rows, cols, 0, 0));
                        }
                        vt.resize(rows, cols);
                        // Full-state resync: the new client's first frame is
                        // the complete current screen (AC3-UI).
                        frame_tx.send_replace(Some(vt.frame()));
                        clients.push(Client { id, frame_tx });
                        broadcast(&vt, &clients);
                    }
                    CoreMsg::Input(bytes) => {
                        // Fail closed once the child is gone: dropped, never a
                        // panic (AC2-ERR). A write error means the child just
                        // exited mid-keystroke - same policy.
                        if !child_exited {
                            let _ = pty.write_input(&bytes);
                        }
                    }
                    CoreMsg::Resize { rows, cols } => {
                        if !child_exited {
                            log_resize_err(pty.resize(rows, cols, 0, 0));
                        }
                        vt.resize(rows, cols);
                        broadcast(&vt, &clients);
                    }
                    CoreMsg::Gone(id) => {
                        clients.retain(|c| c.id != id);
                        if child_exited && clients.is_empty() {
                            break;
                        }
                    }
                }
            }
            _ = async { sigterm.as_mut().unwrap().recv().await }, if sigterm.is_some() => break,
            _ = async { sigint.as_mut().unwrap().recv().await }, if sigint.is_some() => break,
        }
    }
    0
}

/// A live-child resize failure must not be silent: the VT grid resizes
/// anyway, so kernel winsize and grid would disagree and full-screen programs
/// render garbled with nothing in the log to correlate.
fn log_resize_err(res: Result<(), crate::pty::PtyError>) {
    if let Err(e) = res {
        eprintln!("fno mux: pty resize failed: {e}");
    }
}

/// Snapshot once, fan out to every client's newest-frame slot. `send_replace`
/// never blocks: a client that has not drained the previous frame simply
/// never sees it (droppable, self-contained).
fn broadcast(vt: &Pane, clients: &[Client]) {
    if clients.is_empty() {
        return;
    }
    let frame = vt.frame();
    for c in clients {
        c.frame_tx.send_replace(Some(frame.clone()));
    }
}

/// Handshake a fresh connection, then split it into the reader loop (this
/// task) and the writer task.
async fn handle_client(mut stream: UnixStream, core_tx: mpsc::Sender<CoreMsg>, id: u64) {
    let attach = tokio::time::timeout(ATTACH_TIMEOUT, read_msg::<_, ClientMsg>(&mut stream)).await;
    let (rows, cols) = match attach {
        Ok(Ok(ClientMsg::Attach {
            proto,
            build,
            rows,
            cols,
            // Squad selection consumes this (squad.rs, task 2.4); the
            // Phase-1-shaped single-pane loop has no squads yet.
            cwd: _,
        })) => {
            if let Err(reason) = check_attach_version(proto, &build) {
                // Refuse loudly with both versions; the client relays it.
                let _ = write_msg(&mut stream, &ServerMsg::Bye { reason }).await;
                return;
            }
            (rows, cols)
        }
        // Liveness probes connect and vanish; malformed first messages and
        // timeouts close the same way: without touching the pane.
        _ => return,
    };

    let (frame_tx, frame_rx) = watch::channel(None);
    if core_tx
        .send(CoreMsg::Attach {
            id,
            rows,
            cols,
            frame_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    let (read_half, write_half) = stream.into_split();
    tokio::spawn(client_writer(write_half, frame_rx, core_tx.clone(), id));
    client_reader(read_half, core_tx, id).await;
}

/// Reliable inbound path: every message is awaited into the core channel.
/// Any read error (including an abruptly killed client) deregisters the
/// client and leaves the pane untouched (AC4-HP).
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
            Ok(ClientMsg::Command(_)) => {
                // Wired into the core loop by the pane registry (task 2.3).
                // Until then a command is dropped fail-closed, never a crash.
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

/// Droppable outbound path: waits on the newest-frame slot, writes whatever
/// is current when it wakes. A write failure drops THIS client only.
async fn client_writer(
    mut w: OwnedWriteHalf,
    mut frame_rx: watch::Receiver<Option<Frame>>,
    core_tx: mpsc::Sender<CoreMsg>,
    id: u64,
) {
    loop {
        if frame_rx.changed().await.is_err() {
            break; // core dropped the sender: client was deregistered
        }
        let frame = frame_rx.borrow_and_update().clone();
        if let Some(frame) = frame {
            // pane_id 0: the Phase-1-shaped loop still runs its single pane;
            // the pane registry (task 2.3) replaces this with real ids.
            if write_msg(&mut w, &ServerMsg::Frame { pane_id: 0, frame })
                .await
                .is_err()
            {
                // AC4-ERR: one bad client never takes down the server - or
                // its peers. Deregister and stop writing.
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
        }
    }
}
