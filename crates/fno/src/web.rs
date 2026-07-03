//! `fno mux serve --web` (x-6a14): the read-only web bridge.
//!
//! A pure client. It attaches to a running mux session over the same per-session
//! unix socket the native TUI uses, as an OBSERVER (`Attach { rows: 0, cols: 0 }`,
//! which the server marks passive: excluded from the smallest-client clamp, and
//! fed EVERY pane's frames so a browser can view any pane without an upstream
//! message). It re-fans the `ServerMsg` broadcast to browser WebSocket
//! connections as JSON, unmodified. The browser paints the structured cells
//! directly (see `web_page.html`).
//!
//! Read-only is structural (Locked Decision 5): after sending `Attach` the bridge
//! `forget()`s the socket's write half, so no code path can forward a browser
//! byte upstream. The browser also never drives - it drops every inbound WS
//! message and only picks which already-arriving frame to draw locally.
//!
//! Data flow, one direction only:
//!   vt::Pane --composite--> Frame --broadcast--> bridge --WS/JSON--> browser

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::{broadcast, oneshot};

use crate::proto::{self, ClientMsg, ServerMsg, BUILD_VERSION, PROTO_VERSION};

/// The served page, vendored inline (no CDN) so the strict CSP holds offline.
const PAGE: &str = include_str!("web_page.html");
/// The browser drives nothing, so anything it sends is dropped - but cap it so
/// a hostile client cannot OOM the bridge with one giant frame.
const INBOUND_WS_CAP: usize = 64 * 1024;
/// Per-subscriber buffered frames before a slow phone starts lag-dropping stale
/// ones (each browser drops independently; one slow viewer never blocks another).
const BROADCAST_CAP: usize = 256;
/// Max panes retained in the replay snapshot. Pane ids are monotonic and never
/// reused, and the wire has no "pane closed" signal, so a dead pane's last frame
/// would otherwise linger forever. Bounded by evicting the least-recently-updated
/// pane - a dead pane stops updating, so it ages out first (ponytail: fixed cap;
/// a proto-level pane-closed signal is the real fix, deferred with Locked 4).
const MAX_SNAPSHOT_PANES: usize = 128;
/// Reconnect backoff bounds (Errors: preserve the view on upstream EOF).
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Connect bound for the upstream attach. A wedged server (never accepts)
/// turns into a reconnect-with-backoff instead of blocking the bridge task
/// forever mid-loop.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection must stay up at least this long before its drop resets the
/// backoff. An accept-then-EOF flap stays below it, so the backoff keeps growing
/// (and each quick drop logs) instead of spinning a silent 250ms reconnect loop.
const MIN_HEALTHY_UPTIME: Duration = Duration::from_secs(2);

/// Parsed `fno mux serve --web` arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct WebArgs {
    pub session: String,
    /// Bind address; loopback by default (Locked Decision 6). `--bind` widens it;
    /// remote reach is delegated to tailscale / a reverse proxy, not in-process TLS.
    pub bind: String,
    pub port: u16,
}

impl Default for WebArgs {
    fn default() -> Self {
        WebArgs {
            session: proto::DEFAULT_SESSION.to_string(),
            bind: "127.0.0.1".to_string(),
            port: 8722,
        }
    }
}

/// The latest server state, replayed to each freshly-connected browser so it
/// paints immediately instead of waiting for the next broadcast.
#[derive(Default)]
struct Snapshot {
    upstream_up: bool,
    /// Latest `Layout` JSON (the pane/agent catalog for the picker).
    layout: Option<String>,
    /// pane_id -> (update seq, latest `Frame` JSON). The seq drives LRU eviction
    /// so a dead pane (which stops updating) ages out of the replay set first.
    frames: HashMap<u64, (u64, String)>,
    frame_seq: u64,
}

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<String>,
    snap: Arc<Mutex<Snapshot>>,
    token: Arc<str>,
}

/// Entry point for the `mux serve --web` role. Owns its own runtime like the
/// server role; returns the process exit code.
pub fn serve(args: WebArgs) -> i32 {
    let socket = match proto::socket_path(&args.session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno mux serve --web: {e}");
            return 2;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno mux serve --web: cannot start runtime: {e}");
            return 1;
        }
    };
    runtime.block_on(run(args, socket))
}

async fn run(args: WebArgs, socket: PathBuf) -> i32 {
    let token: Arc<str> = match mint_token() {
        Some(t) => t.into(),
        None => {
            eprintln!("fno mux serve --web: cannot read /dev/urandom to mint an auth token");
            return 1;
        }
    };

    let (tx, _rx0) = broadcast::channel::<String>(BROADCAST_CAP);
    let snap: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(Snapshot::default()));
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

    // Attach upstream FIRST, then accept browsers (Concurrency: a browser that
    // connects before we are attached gets the disconnected banner, never a
    // half-open stream). The upstream task owns reconnect-with-backoff.
    {
        let (tx, snap, socket) = (tx.clone(), snap.clone(), socket.clone());
        tokio::spawn(async move { upstream_loop(socket, tx, snap, ready_tx).await });
    }
    match ready_rx.await {
        Ok(Ok(())) => {}
        // Startup failure (no server / refused attach): fail loud, and note that
        // NO HTTP listener was ever opened (AC1-ERR).
        Ok(Err(e)) => {
            eprintln!("fno mux serve --web: {e}");
            return 1;
        }
        Err(_) => {
            eprintln!("fno mux serve --web: upstream task exited before attaching");
            return 1;
        }
    }

    let addr = bind_addr(&args.bind, args.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fno mux serve --web: cannot bind {addr}: {e}");
            return 1;
        }
    };

    let wide = args.bind == "0.0.0.0" || args.bind == "::";
    let host = if wide {
        "<this-host>"
    } else {
        args.bind.as_str()
    };
    println!(
        "fno mux web (read-only): http://{host}:{}/?t={}",
        args.port, token
    );
    if wide {
        println!(
            "  bound to all interfaces - reach it over tailscale/LAN; the URL token is the only guard."
        );
    }

    let state = AppState { tx, snap, token };
    let app = Router::new()
        .route("/", get(page))
        .route("/ws", get(ws_handler))
        .with_state(state);

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("fno mux serve --web: server error: {e}");
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// Upstream: attach as an observer, forward frames, reconnect on EOF
// ---------------------------------------------------------------------------

async fn upstream_loop(
    socket: PathBuf,
    tx: broadcast::Sender<String>,
    snap: Arc<Mutex<Snapshot>>,
    ready_tx: oneshot::Sender<Result<(), String>>,
) {
    let mut ready_tx = Some(ready_tx);
    let mut backoff = BACKOFF_START;
    loop {
        match connect_attach(&socket).await {
            Ok((reader, preamble)) => {
                let started = Instant::now();
                if let Some(rt) = ready_tx.take() {
                    let _ = rt.send(Ok(()));
                }
                snap.lock().unwrap().upstream_up = true;
                let _ = tx.send(bridge_status("connected"));
                forward(preamble, &tx, &snap);
                read_stream(reader, &tx, &snap).await;
                // Upstream dropped: mark stale so the last frame under the amber
                // banner is never presented as live (Errors invariant).
                snap.lock().unwrap().upstream_up = false;
                let _ = tx.send(bridge_status("disconnected"));
                // Only a session that stayed up a while resets the backoff; a
                // quick accept-then-EOF flap keeps growing it and logs, so a
                // misbehaving server never spins a silent tight reconnect loop.
                if started.elapsed() >= MIN_HEALTHY_UPTIME {
                    backoff = BACKOFF_START;
                } else {
                    eprintln!(
                        "fno mux serve --web: upstream dropped after {:?}; backing off",
                        started.elapsed()
                    );
                }
            }
            Err(e) => {
                // First attempt failing is a startup error the caller reports and
                // exits on. A later failure just retries - the browser already
                // shows the disconnected banner.
                if let Some(rt) = ready_tx.take() {
                    let _ = rt.send(Err(e));
                    return;
                }
                eprintln!("fno mux serve --web: upstream reconnect failed: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Connect, send the observer `Attach`, relinquish the write half, and read the
/// first reply. A `Bye` here is a refused/skewed attach (`Err`); anything else
/// means the attach took, and the message is returned as preamble to forward.
async fn connect_attach(socket: &Path) -> Result<(OwnedReadHalf, ServerMsg), String> {
    let stream = tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| {
            format!(
                "cannot connect to session socket {}: connect timed out (wedged server?)",
                socket.display()
            )
        })?
        .map_err(|e| {
            format!(
                "cannot connect to session socket {}: {e}\n  is the mux server running? list sessions with `fno mux ls`.",
                socket.display()
            )
        })?;
    let (reader, mut writer) = stream.into_split();

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    proto::write_msg(
        &mut writer,
        &ClientMsg::Attach {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            // (0,0) is the observer sentinel: excluded from the clamp, never
            // resizes a PTY, fed every pane's frames (server.rs passive path).
            rows: 0,
            cols: 0,
            cwd,
        },
    )
    .await
    .map_err(|e| format!("attach write failed: {e}"))?;
    // READ-ONLY (Locked Decision 5): drop all write ability. `forget()` releases
    // the write half WITHOUT the shutdown-on-drop that would half-close the
    // socket and make the server detach us; after this there is no handle that
    // could ever write a browser byte upstream.
    writer.forget();

    let mut reader = reader;
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        proto::read_msg::<_, ServerMsg>(&mut reader),
    )
    .await
    .map_err(|_| "server did not answer the attach within 10s (wedged?); `fno mux ls`".to_string())?
    .map_err(|e| format!("attach read failed: {e}"))?;

    if let ServerMsg::Bye { reason } = &first {
        // Version skew or an immediate refusal - surface it, never hang or spin.
        return Err(format!("server refused the attach: {reason}"));
    }
    Ok((reader, first))
}

async fn read_stream(
    mut reader: OwnedReadHalf,
    tx: &broadcast::Sender<String>,
    snap: &Arc<Mutex<Snapshot>>,
) {
    loop {
        match proto::read_msg::<_, ServerMsg>(&mut reader).await {
            Ok(msg) => forward(msg, tx, snap),
            // EOF or a malformed message: return to reconnect. A malformed frame
            // is never forwarded as a half-grid.
            Err(_) => return,
        }
    }
}

/// Serialize one `ServerMsg` to its wire JSON, update the replay snapshot, and
/// broadcast it. A `Frame` failing `geometry_ok` at this trust boundary is
/// dropped (AC5-FR): the previous good frame stays drawn.
fn forward(msg: ServerMsg, tx: &broadcast::Sender<String>, snap: &Arc<Mutex<Snapshot>>) {
    if let ServerMsg::Frame { frame, .. } = &msg {
        if !frame.geometry_ok() {
            return;
        }
    }
    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(_) => return,
    };
    {
        let mut s = snap.lock().unwrap();
        match &msg {
            ServerMsg::Frame { pane_id, .. } => {
                s.frame_seq += 1;
                let seq = s.frame_seq;
                s.frames.insert(*pane_id, (seq, json.clone()));
                if s.frames.len() > MAX_SNAPSHOT_PANES {
                    // Evict the least-recently-updated pane (lowest seq = the one
                    // that has gone quiet longest - a dead pane).
                    if let Some(oldest) = s
                        .frames
                        .iter()
                        .min_by_key(|(_, (seq, _))| *seq)
                        .map(|(&pid, _)| pid)
                    {
                        s.frames.remove(&oldest);
                    }
                }
            }
            ServerMsg::Layout { .. } => s.layout = Some(json.clone()),
            _ => {}
        }
    }
    // Err only means no browser is subscribed yet - the snapshot already holds it.
    let _ = tx.send(json);
}

/// A bridge-injected control line (not a `ServerMsg`) the browser reads to drive
/// its connection banner.
fn bridge_status(state: &str) -> String {
    format!("{{\"_bridge\":{{\"state\":\"{state}\"}}}}")
}

/// `host:port`, bracketing an IPv6 literal so `[::1]:8722` parses - a bare
/// `::1:8722` does not (the colons are ambiguous).
fn bind_addr(bind: &str, port: u16) -> String {
    if bind.contains(':') {
        format!("[{bind}]:{port}")
    } else {
        format!("{bind}:{port}")
    }
}

// ---------------------------------------------------------------------------
// HTTP + WebSocket
// ---------------------------------------------------------------------------

async fn page() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE)
}

#[derive(serde::Deserialize)]
struct WsQuery {
    t: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(st): State<AppState>,
) -> Response {
    let ok =
        q.t.as_deref()
            .is_some_and(|t| constant_time_eq(t.as_bytes(), st.token.as_bytes()));
    if !ok {
        // AC4-ERR. A browser cannot read the HTTP status of a FAILED WebSocket
        // handshake: `onclose` fires 1006 for any handshake failure, so a
        // 401-at-upgrade is indistinguishable from a network drop and would
        // retry forever instead of painting the auth-failed banner. Complete the
        // upgrade, then immediately close with 1008 (policy violation) + reason,
        // so the browser's `onclose.code === 1008` fires reliably. No ServerMsg
        // frame is ever sent, so "no frame on a bad token" still holds.
        return ws.on_upgrade(|mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::POLICY,
                    reason: "invalid token".into(),
                })))
                .await;
        });
    }
    ws.max_message_size(INBOUND_WS_CAP)
        .on_upgrade(move |socket| ws_conn(socket, st))
}

async fn ws_conn(mut socket: WebSocket, st: AppState) {
    // Subscribe BEFORE snapshotting so no frame slips through the gap between
    // reading the snapshot and joining the live stream (a duplicate is harmless;
    // a gap would leave a stale grid).
    let mut rx = st.tx.subscribe();
    {
        let preamble: Vec<String> = {
            let s = st.snap.lock().unwrap();
            let mut p = Vec::with_capacity(s.frames.len() + 2);
            p.push(bridge_status(if s.upstream_up {
                "connected"
            } else {
                "disconnected"
            }));
            if let Some(l) = &s.layout {
                p.push(l.clone());
            }
            p.extend(s.frames.values().map(|(_, j)| j.clone()));
            p
        };
        for m in preamble {
            if socket.send(Message::Text(m.into())).await.is_err() {
                return;
            }
        }
    }

    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(json) => {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        return;
                    }
                }
                // Slow phone: skip the stale frames it missed, keep streaming.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            r = socket.recv() => match r {
                // Read-only: drop every inbound browser message. A Close frame,
                // an error, or EOF ends the connection.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// 32 random bytes from `/dev/urandom` as hex. `None` if the device is
/// unreadable (fail closed - never mint a guessable token).
fn mint_token() -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time equality over the compared bytes. The token length is not
/// secret (always 64 hex chars), so an early length mismatch is fine; the byte
/// comparison itself never short-circuits.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"abc123", b"abc12")); // length mismatch
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn mint_token_is_64_hex_chars_and_fresh() {
        let a = mint_token().expect("/dev/urandom readable in test env");
        let b = mint_token().expect("/dev/urandom readable in test env");
        assert_eq!(a.len(), 64, "32 bytes -> 64 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two mints must differ (astronomically)");
    }

    #[test]
    fn bridge_status_is_valid_json_the_browser_keys_on() {
        let s = bridge_status("disconnected");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["_bridge"]["state"], "disconnected");
    }

    #[test]
    fn bind_addr_brackets_ipv6_only() {
        assert_eq!(bind_addr("127.0.0.1", 8722), "127.0.0.1:8722");
        assert_eq!(bind_addr("0.0.0.0", 80), "0.0.0.0:80");
        assert_eq!(bind_addr("::1", 8722), "[::1]:8722");
        assert_eq!(bind_addr("::", 8722), "[::]:8722");
    }

    #[test]
    fn default_web_args_bind_loopback() {
        let a = WebArgs::default();
        assert_eq!(a.bind, "127.0.0.1");
        assert_eq!(a.session, proto::DEFAULT_SESSION);
    }

    fn tiny_frame() -> proto::Frame {
        proto::Frame {
            rows: 1,
            cols: 1,
            cells: vec![proto::Cell::default()],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            scroll_offset: 0,
        }
    }

    fn feed(snap: &Arc<Mutex<Snapshot>>, pane_id: u64) {
        let (tx, _rx) = broadcast::channel::<String>(16);
        forward(
            ServerMsg::Frame {
                pane_id,
                frame: tiny_frame(),
            },
            &tx,
            snap,
        );
    }

    #[test]
    fn forward_drops_a_malformed_frame() {
        let (tx, _rx) = broadcast::channel::<String>(16);
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        // rows*cols == 4 but only one cell: geometry_ok() is false.
        let bad = proto::Frame {
            rows: 2,
            cols: 2,
            cells: vec![proto::Cell::default()],
            ..tiny_frame()
        };
        forward(
            ServerMsg::Frame {
                pane_id: 7,
                frame: bad,
            },
            &tx,
            &snap,
        );
        assert!(
            snap.lock().unwrap().frames.is_empty(),
            "a geometry-inconsistent frame is dropped, never stored"
        );
    }

    #[test]
    fn snapshot_bounds_to_the_cap_evicting_stalest() {
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        for pid in 0..(MAX_SNAPSHOT_PANES as u64 + 5) {
            feed(&snap, pid);
        }
        let s = snap.lock().unwrap();
        assert_eq!(s.frames.len(), MAX_SNAPSHOT_PANES, "bounded to the cap");
        assert!(!s.frames.contains_key(&0), "the stalest pane was evicted");
        assert!(
            s.frames.contains_key(&(MAX_SNAPSHOT_PANES as u64 + 4)),
            "the newest pane is retained"
        );
    }

    #[test]
    fn snapshot_retains_a_pane_that_keeps_updating() {
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        feed(&snap, 0);
        for pid in 1..(MAX_SNAPSHOT_PANES as u64) {
            feed(&snap, pid);
        }
        feed(&snap, 0); // touch pane 0 again -> now the freshest
        for pid in MAX_SNAPSHOT_PANES as u64..(MAX_SNAPSHOT_PANES as u64 + 5) {
            feed(&snap, pid);
        }
        assert!(
            snap.lock().unwrap().frames.contains_key(&0),
            "a pane that keeps updating survives the eviction sweep"
        );
    }
}
