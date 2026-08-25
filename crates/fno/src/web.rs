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
use axum::http::{header, StatusCode};
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
    graph_html: PathBuf,
    /// Fires on Ctrl-C so every ws loop ends and axum's graceful shutdown can
    /// complete: an open browser tab holds a connection that never closes on
    /// its own, so without this arm the bridge hangs past the signal and the
    /// state-file Drop the hook exists to guarantee never runs.
    shutdown: tokio::sync::watch::Receiver<bool>,
}

/// The bridge's live-state marker (x-b80d): `web-<session>.json` beside the
/// session socket, holding the bind/port/token the bind-time print showed
/// once. Written 0600 (the token is the only URL guard); removed by `Drop`
/// on every exit path. A SIGKILLed bridge leaves it behind - the reader
/// (`mux_cli::print_pane_url`) probes the TCP port, so a corpse file reads
/// as "no bridge", never as a dead URL.
struct WebStateFile(PathBuf);

impl WebStateFile {
    fn write(socket: &Path, bind: &str, port: u16, token: &str) -> Option<Self> {
        let session = socket
            .file_stem()
            .and_then(|s| s.to_str())
            // socket_path() names the file <session>.sock; the stem is the name.
            .unwrap_or(proto::DEFAULT_SESSION);
        let path = socket.parent()?.join(format!("web-{session}.json"));
        // `pid` makes the file's ownership checkable: two bridges may share a
        // session on different ports, the later bind owns the file, and an
        // exiting OLDER bridge must not delete the newer one's state (codex P2).
        let body = serde_json::json!({
            "bind": bind,
            "port": port,
            "token": token,
            "pid": std::process::id(),
        });
        let wrote = {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut f: std::fs::File| f.write_all(body.to_string().as_bytes()))
        };
        match wrote {
            Ok(()) => Some(WebStateFile(path)),
            Err(e) => {
                eprintln!("fno mux serve --web: cannot record bridge state: {e}");
                None
            }
        }
    }
}

impl Drop for WebStateFile {
    fn drop(&mut self) {
        // Remove only the file that still names THIS process. A newer bridge
        // for the same session overwrote it at its own bind; deleting that
        // one would make a live bridge read as absent.
        let still_ours = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
            == Some(std::process::id() as u64);
        if still_ours {
            let _ = std::fs::remove_file(&self.0);
        }
    }
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
    // (x-b80d) Record the live bridge so `mux view <selector> --url` can
    // recover the URL after this one print. A write failure only costs the
    // --url door (it reports "no web bridge"), never the bridge itself; a
    // file left behind by a killed bridge is inert because the reader probes
    // the port before trusting it. Removed on every exit path via Drop.
    let _state_guard = WebStateFile::write(&socket, &args.bind, args.port, &token);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let state = AppState {
        tx,
        snap,
        token,
        graph_html: crate::backlog_view::graph_path().with_file_name("graph.html"),
        shutdown: shutdown_rx,
    };
    let app = Router::new()
        .route("/", get(page))
        .route("/backlog", get(backlog))
        .route("/ws", get(ws_handler))
        .with_state(state);

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // Ctrl-C is the NORMAL way a bridge ends. Without this hook the
            // process dies straight to the signal, no Drop runs, and the
            // state file outlives its bridge (x-b80d). Firing the watch makes
            // every ws loop close its tab first, so graceful shutdown can
            // actually complete instead of waiting out an open connection.
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown_tx.send(true);
        })
        .await
    {
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

async fn backlog(Query(q): Query<WsQuery>, State(st): State<AppState>) -> Response {
    backlog_response(&st.graph_html, q.t.as_deref(), &st.token).await
}

async fn backlog_response(path: &Path, supplied: Option<&str>, expected: &str) -> Response {
    let authorized =
        supplied.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "invalid or missing token",
        )
            .into_response();
    }
    match std::fs::read_to_string(path) {
        Ok(body) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "backlog not rendered; run FNO_NO_OPEN=1 fno backlog view".to_string(),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("backlog unreadable: {err}"),
        )
            .into_response(),
    }
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
        let mut shutdown = st.shutdown.clone();
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
            _ = shutdown.changed() => {
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::NORMAL,
                        reason: "bridge shutting down".into(),
                    })))
                    .await;
                return;
            }
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

    /// The served page must stay pinch-zoomable (x-ce65). A pane grid is as wide
    /// as its terminal, so on a phone zooming is the only way to read a grid
    /// wider than the screen; `maximum-scale` / `user-scalable=no` take that away.
    /// Anchored on the meta line itself, so a page that lost the tag entirely
    /// fails here rather than passing on an absence.
    #[test]
    fn served_page_leaves_pinch_zoom_enabled() {
        let meta = PAGE
            .lines()
            .find(|l| l.contains(r#"name="viewport""#))
            .expect("the served page declares a viewport meta");
        assert!(
            meta.contains("width=device-width"),
            "viewport still maps the layout to the device: {meta}"
        );
        assert!(
            !meta.contains("maximum-scale"),
            "maximum-scale disables pinch-zoom on iOS: {meta}"
        );
        assert!(
            !meta.contains("user-scalable"),
            "user-scalable=no disables pinch-zoom on iOS: {meta}"
        );
    }

    /// Lift one top-level `function <name>(` body out of the served page.
    /// Brace-balanced, which is exact for the page as written (no brace lives
    /// inside a string or comment in the lifted functions) and is the ceiling:
    /// a future `"{"` inside one of them would cut the slice short, and the
    /// node run then fails loudly on a syntax error rather than passing.
    fn lift_js_fn(name: &str) -> String {
        let head = format!("function {name}(");
        let start = PAGE
            .find(&head)
            .unwrap_or_else(|| panic!("the served page defines {name}()"));
        let rest = &PAGE[start..];
        let open = rest.find('{').expect("a function body opens");
        let mut depth = 0usize;
        for (i, c) in rest[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return rest[..open + i + c.len_utf8()].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces lifting {name}()");
    }

    /// The retention diff (x-ce65) decides how many rows scrolled off between
    /// two frames, and getting it wrong silently corrupts what the operator
    /// reads as recent output. Exercise the SHIPPED source, not a Rust
    /// re-implementation, by running the lifted function under node.
    ///
    /// node is preinstalled on the CI runners. Where it is absent the test
    /// cannot assert anything, so it says so on stdout instead of passing
    /// quietly - a skip that looks like a pass is the failure mode here.
    #[test]
    fn evicted_row_count_tracks_the_grid_shift() {
        let asserts = r#"
const eq = (got, want, what) => {
  if (got !== want) { console.error(`FAIL ${what}: got ${got}, want ${want}`); process.exit(1); }
};
const g = (...rows) => rows;

// --- proven scrolls: a non-negative row count ---
// An unmoved grid evicts nothing, so redrawing one frame stays idempotent.
eq(evictedRowCount(g("a","b","c"), g("a","b","c")), 0, "identical");
// Scrolled up by one: the top row left, a new row arrived at the bottom.
eq(evictedRowCount(g("a","b","c"), g("b","c","d")), 1, "shift by 1");
eq(evictedRowCount(g("a","b","c","d"), g("c","d","e","f")), 2, "shift by 2");
eq(evictedRowCount(g("a","b","c","d"), g("d","w","x","y")), 3, "shift by 3");
// A blank screen staying blank is unmoved, never a screenful of evictions.
eq(evictedRowCount(g("","",""), g("","","")), 0, "blank stays blank");
// Repeated rows leave the shift ambiguous. The smallest fit wins, which is
// the conservative read: an ambiguous shift retains fewer rows rather than
// inventing evictions that never happened.
eq(evictedRowCount(g("x","x","x","p"), g("x","x","p","q")), 1, "ambiguous shift takes the smallest fit");

// --- edited in place: 0 rows evicted, and NO gap ---
// A terminal edits rows in place constantly, and none of it is a scroll. These
// are the cases that made an earlier version dump a duplicate of the visible
// screen into the retained region on every single frame. Reading the last rows
// as a pinned tail answers 0 here, which is stronger than "unprovable": nothing
// scrolled off, so nothing is retained AND no gap is claimed.
eq(evictedRowCount(g("hdr","body","* work"), g("hdr","body","- work")), 0, "spinner tick on the last row");
eq(evictedRowCount(g("$ ls","a.txt","",""), g("$ ls","a.txt","$ ","")), 0, "prompt appears on an unfilled screen");

// --- not provably a scroll: -1, and the caller must retain NOTHING ---
// A live HEADER is not covered: only a pinned TAIL is looked past, because that
// is where an agent pane puts its input box. Under-reporting, never a guess.
eq(evictedRowCount(g("t=1","a","b"), g("t=2","b","c")), -1, "body scrolls under a live header");
// A whole new screen shares nothing: a repaint, or output outrunning the
// frame rate. Unprovable either way, so it is never treated as a shift.
eq(evictedRowCount(g("a","b","c"), g("x","y","z")), -1, "no overlap");
// Geometry changed under us. Note this is the SHRINK direction, which an
// earlier version silently concatenated because it compared the shift against
// the old row count rather than the new one.
eq(evictedRowCount(g("a","b","c","d","e"), g("c","d","e")), -1, "terminal shrank");
eq(evictedRowCount(g("a","b"), g("a","b","c")), -1, "terminal grew");
eq(evictedRowCount(null, g("a","b","c")), -1, "no previous frame");

// --- a pinned bottom region: the pane an operator actually opens this for ---
// An agent pane keeps an input box or a footer line on its last rows. Requiring
// the WHOLE grid to shift made retention permanently inert on every one of them,
// and silently, because the note only appears once a row is retained.
// A static footer, body scrolled by one.
eq(evictedRowCount(g("a","b","c","foot"), g("b","c","d","foot")), 1, "static footer, body scrolls");
// A LIVE footer, body scrolled by one: the tail differs too, so nothing about
// the last row can be assumed, only that it is not part of the scrolling body.
eq(evictedRowCount(g("a","b","c","spin |"), g("b","c","d","spin /")), 1, "live footer, body scrolls");
// A live footer over a body that did NOT move evicts nothing. This is the
// spinner case again, one row deeper, and it must not read as a scroll.
eq(evictedRowCount(g("a","b","c","spin |"), g("a","b","c","spin /")), 0, "live footer, body still");
// Two-row footer, body scrolled by two.
eq(evictedRowCount(g("a","b","c","f1","f2"), g("c","d","e","f1","f2")), 2, "two-row footer");
// The tail we look past is bounded, so a grid that is mostly footer is still
// reported honestly rather than explained away by an ever-deeper tail.
eq(evictedRowCount(g("a","b","c","d","e","f","g","h"), g("z","y","x","w","v","u","t","s")), -1, "unrelated beats any tail");
console.log("evictedRowCount: 18 cases ok");
"#;
        let src = format!(
            "{}\n{}\n{}\n{}",
            // MAX_FIXED_TAIL is a const the lifted function closes over.
            PAGE.lines()
                .find(|l| l.contains("const MAX_FIXED_TAIL"))
                .expect("the page bounds the fixed tail it looks past"),
            lift_js_fn("scrollWithin"),
            lift_js_fn("evictedRowCount"),
            asserts
        );
        let path = std::env::temp_dir().join(format!("fno-evicted-{}.mjs", std::process::id()));
        std::fs::write(&path, src).expect("temp dir writable");
        let out = std::process::Command::new("node").arg(&path).output();
        let _ = std::fs::remove_file(&path);
        match out {
            Err(e) => {
                // On CI a missing node means the assertions never ran, and a
                // skip that reads as a pass is exactly the failure this guard
                // exists to prevent. The runners ship node, so demand it there.
                assert!(
                    std::env::var_os("CI").is_none(),
                    "node is required on CI to exercise the shipped evictedRowCount: {e}"
                );
                println!(
                    "SKIPPED evicted_row_count_tracks_the_grid_shift: node not runnable ({e}); \
                     nothing was asserted"
                );
            }
            Ok(o) => {
                // The end-of-harness marker is the whole verdict, and it is
                // strictly stronger than the exit code: node printing it means
                // every case passed, and nothing else prints it. A failed case,
                // a syntax error, and a node that somehow exits 0 without
                // running all fail the same way, with both streams shown.
                let stdout = String::from_utf8_lossy(&o.stdout);
                assert!(
                    stdout.contains("evictedRowCount: 18 cases ok"),
                    "the shipped evictedRowCount did not clear every case:\n{}{}",
                    stdout,
                    String::from_utf8_lossy(&o.stderr)
                );
            }
        }
    }

    /// The page must keep calling retention what it is (x-ce65). A protocol
    /// history request is unreachable while `writer.forget()` stands, so the
    /// visible label must not promise scrollback the wire never carries.
    #[test]
    fn served_page_does_not_advertise_scrollback_to_the_operator() {
        let note = PAGE
            .lines()
            .find(|l| l.contains("const KEPT_NOTE ="))
            .expect("the page names the retained region");
        assert!(
            note.contains("not terminal scrollback"),
            "the retained region disclaims scrollback: {note}"
        );
    }

    /// Fit-to-width is client-side only (x-ce65). The bridge attaches passive
    /// with rows==0/cols==0 so it never shrinks a PTY, and `writer.forget()`
    /// leaves no upstream handle. A page that learned to ask for a resize would
    /// collapse every terminal user's pane to phone width.
    ///
    /// Anchored on the WIRE vocabulary and on sending, not on the bare word
    /// "Resize". A bare match also banned `ResizeObserver`, which is a local DOM
    /// API and the right way to refit when the screen box changes without a
    /// window resize event - the guard would have refused it with a message
    /// about PTY geometry it has nothing to do with.
    #[test]
    fn served_page_never_asks_for_a_resize() {
        assert!(
            PAGE.contains("new WebSocket("),
            "the page still opens the read-only socket"
        );
        assert!(
            !PAGE.contains(".send("),
            "the page sends nothing upstream at all (Locked Decision 5)"
        );
        assert!(
            !PAGE.contains(r#""Resize""#),
            "the page never names the Resize message: a passive observer must not drive PTY geometry"
        );
        assert!(
            !PAGE.contains("ClientMsg"),
            "the page never builds an upstream message of any kind"
        );
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

    #[tokio::test]
    async fn backlog_requires_token_and_serves_private_file_without_cache() {
        let dir =
            std::env::temp_dir().join(format!("fno-web-backlog-{}-serve", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("graph.html");
        std::fs::write(&path, "PRIVATE-BACKLOG-MARKER").unwrap();
        let response = backlog_response(&path, Some("right"), "right").await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("PRIVATE-BACKLOG-MARKER"));

        let denied = backlog_response(&path, Some("wrong"), "right").await;
        assert_eq!(denied.status(), axum::http::StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(denied.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("PRIVATE-BACKLOG-MARKER"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_backlog_names_the_render_action() {
        let dir =
            std::env::temp_dir().join(format!("fno-web-backlog-{}-missing", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let response = backlog_response(&dir.join("graph.html"), Some("right"), "right").await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("FNO_NO_OPEN=1 fno backlog view"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_preserves_the_token_in_the_backlog_link() {
        assert!(PAGE.contains("id=\"backlog-link\""));
        assert!(PAGE.contains("/backlog?t=${encodeURIComponent(token)}"));
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
