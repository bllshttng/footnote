//! `fno mux serve --web` : the read-only web bridge.
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

use crate::client::humanize_ago;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

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
    /// `--stop`: kill the running bridge for this session instead of serving
    /// one. The state file names the pid; the pid's start token names the
    /// process.
    pub stop: bool,
    /// `--status`: read the state file and report who is serving (pid, bind,
    /// port, binary, build rev, start time, launching session id), flagging a
    /// bridge whose build rev predates the installed binary.
    pub status: bool,
}

impl Default for WebArgs {
    fn default() -> Self {
        WebArgs {
            session: proto::DEFAULT_SESSION.to_string(),
            bind: "127.0.0.1".to_string(),
            port: 8722,
            stop: false,
            status: false,
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
    reign_html: PathBuf,
    fleet_html: PathBuf,
    /// Fires on Ctrl-C so every ws loop ends and axum's graceful shutdown can
    /// complete: an open browser tab holds a connection that never closes on
    /// its own, so without this arm the bridge hangs past the signal and the
    /// state-file Drop the hook exists to guarantee never runs.
    shutdown: tokio::sync::watch::Receiver<bool>,
}

fn graph_html_path_from_state_root(state_root: &Path) -> PathBuf {
    state_root.join("graph.html")
}

fn graph_html_path() -> PathBuf {
    #[cfg(not(test))]
    {
        graph_html_path_from_state_root(&crate::proto::mux_sidecar_root())
    }
    #[cfg(test)]
    {
        let graph = crate::backlog_view::graph_path();
        graph_html_path_from_state_root(graph.parent().unwrap_or_else(|| Path::new(".")))
    }
}

fn reign_html_path_from_state_root(state_root: &Path) -> PathBuf {
    state_root.join("reign.html")
}

fn reign_html_path() -> PathBuf {
    #[cfg(not(test))]
    {
        reign_html_path_from_state_root(&crate::proto::mux_sidecar_root())
    }
    #[cfg(test)]
    {
        let graph = crate::backlog_view::graph_path();
        reign_html_path_from_state_root(graph.parent().unwrap_or_else(|| Path::new(".")))
    }
}

fn fleet_html_path_from_state_root(state_root: &Path) -> PathBuf {
    state_root.join("fleet.html")
}

fn fleet_html_path() -> PathBuf {
    #[cfg(not(test))]
    {
        fleet_html_path_from_state_root(&crate::proto::mux_sidecar_root())
    }
    #[cfg(test)]
    {
        let graph = crate::backlog_view::graph_path();
        fleet_html_path_from_state_root(graph.parent().unwrap_or_else(|| Path::new(".")))
    }
}

/// The bridge's live-state marker: `web-<session>.json` beside the
/// session socket, holding the bind/port/token the bind-time print showed
/// once. Written 0600 (the token is the only URL guard); removed by `Drop`
/// on every exit path. A SIGKILLed bridge leaves it behind - the reader
/// (`mux_cli::print_pane_url`) probes the TCP port, so a corpse file reads
/// as "no bridge", never as a dead URL. `serve --web --stop` is the other
/// reader: it kills the recorded pid, and the `started` token is what lets
/// it refuse a recycled pid instead of signalling a stranger.
struct WebStateFile(PathBuf);

/// Path of the bridge's live-state marker beside a session socket. The one
/// construction for writer and readers, so the file name cannot drift.
fn web_state_path(socket: &Path) -> Option<PathBuf> {
    let session = socket
        .file_stem()
        .and_then(|s| s.to_str())
        // socket_path() names the file <session>.sock; the stem is the name.
        .unwrap_or(proto::DEFAULT_SESSION);
    socket
        .parent()
        .map(|p| p.join(format!("web-{session}.json")))
}

/// The state-file path from a session name; the one lookup for readers
/// outside this module (`--stop` resolves the socket itself).
pub(crate) fn web_state_path_for_session(session: &str) -> Option<PathBuf> {
    web_state_path(&proto::socket_path(session).ok()?)
}

/// Read a session's web-bridge state file and build the pasteable
/// per-pane URL. The bridge writes `web-<session>.json` at bind; a file whose
/// port no longer answers is a corpse, not a bridge, so the TCP probe - not
/// the file's existence - decides liveness. Exit codes follow the mux verbs'
/// (0 printed, 1 no usable bridge).
pub(crate) fn print_pane_url(verb: &str, session: &str, pane: u64) -> i32 {
    let hint = format!(
        "no web bridge for session {session}; start one with: fno mux serve --web --session {session}"
    );
    let Some(path) = web_state_path_for_session(session) else {
        eprintln!("{verb}: {hint}");
        return 1;
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        eprintln!("{verb}: {hint}");
        return 1;
    };
    let state: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("{verb}: {hint}");
            return 1;
        }
    };
    let bind = state
        .get("bind")
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    let port = match state.get("port").and_then(|v| v.as_u64()) {
        Some(p) => p,
        None => {
            eprintln!("{verb}: {hint}");
            return 1;
        }
    };
    let token = state
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // A wide bind is reachable locally too; the pasteable URL says where THIS
    // machine finds it, mirroring the bind-time print's host hint.
    let host = if bind == "0.0.0.0" || bind == "::" {
        "127.0.0.1"
    } else {
        bind
    };
    // Probe liveness for ANY spelling the operator may have bound (codex P2):
    // a corpse file must not read as a bridge. Same probe --status uses.
    if !tcp_answers(host, port as u16) {
        eprintln!("{verb}: {hint}");
        return 1;
    }
    // Bracket a literal IPv6 host; a bare::1 in a URL truncates at the colon.
    let url_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    println!("http://{url_host}:{port}/?t={token}&pane={pane}");
    0
}

impl WebStateFile {
    fn write(socket: &Path, bind: &str, port: u16, token: &str) -> Option<Self> {
        let path = web_state_path(socket)?;
        // `pid` makes the file's ownership checkable: two bridges may share a
        // session on different ports, the later bind owns the file, and an
        // exiting OLDER bridge must not delete the newer one's state (codex P2).
        // `started` is the pid's start-time token, so `--stop` can tell the
        // bridge it names from a process that later reused the pid.
        // `bin`/`rev`/`started_at`/`session` answer "who started this and how
        // old is its build": rev is the crates/ subtree rev this binary
        // baked, so --status can flag a bridge running pre-deploy code.
        let body = serde_json::json!({
            "bind": bind,
            "port": port,
            "token": token,
            "pid": std::process::id(),
            "started": proto::pid_start_time(std::process::id()),
            "bin": std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into()),
            "rev": env!("FNO_MUX_CRATES_REV"),
            "started_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "session": launching_session_id(),
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

/// How long a SIGINT'd bridge gets to exit before `--stop` escalates to
/// SIGKILL. SIGINT is the bridge's own Ctrl-C path (browser sockets close,
/// the state file's Drop runs), so a healthy bridge is well inside this; a
/// wedged one is why the rung exists.
const WEB_STOP_GRACE: Duration = Duration::from_secs(3);

/// `serve --web --stop`: kill the running bridge for a session. The state
/// file names the pid; the pid's start token names the process. Exit 0 for
/// every "nothing to stop" shape - no state file, a corpse file, a recycled
/// pid - so an "ensure stopped" script converges, and 1 only for a failure
/// to act (unreadable state, a refused signal).
fn stop_web(session: &str, socket: &Path) -> i32 {
    let Some(state) = web_state_path(socket) else {
        eprintln!("fno mux serve --web: cannot place the bridge state beside {socket:?}");
        return 1;
    };
    let raw = match std::fs::read_to_string(&state) {
        Ok(r) => r,
        Err(_) => {
            println!("no web bridge recorded for session {session:?}");
            return 0;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fno mux serve --web: bridge state is not JSON: {e}");
            return 1;
        }
    };
    let Some(pid) = v
        .get("pid")
        .and_then(|p| p.as_u64())
        .map(|p| p as libc::pid_t)
    else {
        // A state file that names no pid cannot be stopped or validated;
        // it is a corpse by definition.
        let _ = std::fs::remove_file(&state);
        println!(
            "no web bridge recorded for session {session:?} (state file named no pid; removed)"
        );
        return 0;
    };
    // Identity: a dead or zombie pid is gone whatever the file says, and a
    // recorded start token that no longer matches proves the pid was reused
    // after the bridge died - signalling it would hit a stranger.
    let recorded = v.get("started").and_then(|s| s.as_u64());
    let stale = proto::pid_confirmed_dead(pid)
        || proto::pid_is_zombie(pid)
        || recorded.is_some_and(|r| proto::pid_start_time(pid as u32).is_none_or(|now| now != r));
    if stale {
        let _ = std::fs::remove_file(&state);
        println!("no live web bridge for session {session:?} (stale state file removed)");
        return 0;
    }
    // SIGINT, not SIGTERM: the bridge's graceful-shutdown arm listens for
    // Ctrl-C (tokio::signal::ctrl_c), so the same signal closes every browser
    // socket, lets the state file's Drop run, and a SIGTERM would bypass all
    // of it.
    if unsafe { libc::kill(pid, libc::SIGINT) } != 0 {
        let e = std::io::Error::last_os_error();
        eprintln!("fno mux serve --web: cannot signal pid {pid}: {e}");
        return 1;
    }
    // The pid's start token at signal time; if it stops matching during the
    // grace window, the bridge was reaped and the pid reused - the bridge is
    // gone, and the newcomer must not inherit the escalation.
    let signalled_start = proto::pid_start_time(pid as u32);
    let deadline = Instant::now() + WEB_STOP_GRACE;
    while Instant::now() < deadline {
        let gone = proto::pid_confirmed_dead(pid)
            || proto::pid_is_zombie(pid)
            || signalled_start
                .is_some_and(|s| proto::pid_start_time(pid as u32).is_none_or(|now| now != s));
        if gone {
            println!("web bridge for session {session:?} stopped (pid {pid})");
            return 0;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    println!(
        "web bridge for session {session:?} ignored SIGINT; killed (pid {pid}); \
         the state file it could not remove reads as no bridge"
    );
    0
}

// ---------------------------------------------------------------------------
// --status and port-collision naming
// ---------------------------------------------------------------------------

/// The launching agent's harness session id, when the environment carries one.
/// `FNO_HARNESS_SESSION_ID` is what fno stamps; the rest are the per-harness
/// fallbacks the claim system recognizes.
fn launching_session_id() -> Option<String> {
    [
        "FNO_HARNESS_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
    ]
    .iter()
    .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

/// A state-file record whose pid is alive under its recorded start token.
/// Dead, zombie, or a reused pid all read as not-live, so a corpse file never
/// passes as a bridge.
fn recorded_pid_is_live(v: &serde_json::Value) -> bool {
    let Some(pid) = v
        .get("pid")
        .and_then(|p| p.as_u64())
        .map(|p| p as libc::pid_t)
    else {
        return false;
    };
    let recorded = v.get("started").and_then(|s| s.as_u64());
    !proto::pid_confirmed_dead(pid)
        && !proto::pid_is_zombie(pid)
        && recorded.is_none_or(|r| proto::pid_start_time(pid as u32).is_some_and(|now| now == r))
}

/// Does something answer TCP on host:port right now?
fn tcp_answers(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
        .is_some_and(|addr| {
            std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
        })
}

/// True when a bridge was built from a different, known crates/ rev than the
/// binary this verb runs from, i.e. it is serving pre-deploy code. "unknown"
/// revs (a hand-built or stripped binary) are never called stale - we cannot
/// know, so we do not claim to.
fn rev_is_stale(recorded: &str) -> bool {
    recorded != "unknown" && recorded != env!("FNO_MUX_CRATES_REV")
}

/// Parse a `web-*.json` state file into (session, pid, port, bind, rev,
/// started_at, bin, launched-by). None for an unreadable or non-JSON file.
fn read_state_record(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Every live bridge recorded beside this mux's sockets: (session, record).
/// Same-dir scan only: sockets of one server live here, so this sees every
/// bridge that server's sessions started. A bridge under a DIFFERENT server's
/// socket dir is out of sight (ponytail: the collision note says so).
fn live_bridge_records(socket_dir: &Path) -> Vec<(String, serde_json::Value)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(socket_dir) else {
        return found;
    };
    let mut names: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(session) = name
            .strip_prefix("web-")
            .and_then(|s| s.strip_suffix(".json"))
        else {
            continue;
        };
        if let Some(v) = read_state_record(&path) {
            if recorded_pid_is_live(&v) {
                found.push((session.to_string(), v));
            }
        }
    }
    found
}

/// `serve --status`: report who is serving for a session - pid, bind, port,
/// binary, build rev, start time, launching session id - flagging a bridge
/// whose rev predates the installed binary. Exit 0 live, 1 none.
fn status_web(session: &str, socket: &Path) -> i32 {
    let Some(state) = web_state_path(socket) else {
        eprintln!("fno mux serve --web: cannot resolve the state-file path");
        return 1;
    };
    let record = read_state_record(&state).filter(|v| {
        // A live record owns the file; a stale one is a corpse from a killed
        // bridge (stop removes those; status only refuses to report it).
        recorded_pid_is_live(v)
    });
    let Some(v) = record else {
        println!("no live web bridge for session {session:?}");
        return 1;
    };
    let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
    let bind = v.get("bind").and_then(|b| b.as_str()).unwrap_or("unknown");
    let port = v.get("port").and_then(|p| p.as_u64()).unwrap_or(0) as u16;
    println!("web bridge for session {session:?}: live (pid {pid})");
    println!("  bind:    {bind}:{port}");
    println!(
        "  binary:  {}",
        v.get("bin").and_then(|b| b.as_str()).unwrap_or("unknown")
    );
    let rev = v.get("rev").and_then(|r| r.as_str()).unwrap_or("unknown");
    if rev_is_stale(rev) {
        println!(
            "  build:   {rev} - STALE, installed build is {}; restart the bridge to serve current code",
            env!("FNO_MUX_CRATES_REV")
        );
    } else {
        println!("  build:   {rev}");
    }
    if let Some(at) = v.get("started_at").and_then(|s| s.as_u64()) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        println!(
            "  started: {at} ({} ago)",
            humanize_ago(now.saturating_sub(at))
        );
    }
    println!(
        "  started by session: {}",
        v.get("session")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("(none recorded)")
    );
    if !tcp_answers(bind, port) {
        println!("  note:    port {port} is not answering (bridge may be mid-start or wedged)");
    }
    // Sibling sessions: the measured pain was TWO bridges nobody could find.
    // One line each for the other sessions this server's socket dir records.
    let dir = socket.parent().map(|p| p.to_path_buf());
    if let Some(dir) = dir {
        for (other, r) in live_bridge_records(&dir) {
            if other == session {
                continue;
            }
            println!(
                "  also live: session {other:?} pid {} port {}",
                r.get("pid").and_then(|p| p.as_u64()).unwrap_or(0),
                r.get("port").and_then(|p| p.as_u64()).unwrap_or(0)
            );
        }
    }
    0
}

/// On a taken port, name the recorded bridge holding it, so a second start
/// points at the process to stop instead of a bare EADDRINUSE. Only state
/// files beside THIS session's socket are checked; a port held by a bridge
/// under another server's socket dir (or by a non-fno process) falls through
/// to the generic error.
fn port_holder_note(socket: &Path, port: u16) {
    let Some(dir) = socket.parent() else { return };
    let holders: Vec<_> = live_bridge_records(dir)
        .into_iter()
        .filter(|(_, r)| r.get("port").and_then(|p| p.as_u64()) == Some(port as u64))
        .collect();
    if holders.is_empty() {
        eprintln!(
            "  no recorded web bridge names port {port}; another process likely holds it \
             (try: lsof -i :{port})"
        );
        return;
    }
    for (session, r) in holders {
        let pid = r.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
        let who = r
            .get("session")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| format!("started by session {s}"))
            .unwrap_or_else(|| "starter session not recorded".into());
        eprintln!(
            "  port {port} is held by the web bridge of session {session:?} (pid {pid}, {who}); \
             stop it with: fno mux serve --stop --server {session}"
        );
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
    if args.status {
        return status_web(&args.session, &socket);
    }
    if args.stop {
        return stop_web(&args.session, &socket);
    }
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
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("fno mux serve --web: cannot bind {addr}: {e}");
            port_holder_note(&socket, args.port);
            return 1;
        }
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
    // Record the live bridge so `mux view <selector> --url` can
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
        graph_html: graph_html_path(),
        reign_html: reign_html_path(),
        fleet_html: fleet_html_path(),
        shutdown: shutdown_rx,
    };
    let app = router(state);

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // Ctrl-C is the NORMAL way a bridge ends. Without this hook the
            // process dies straight to the signal, no Drop runs, and the
            // state file outlives its bridge. Firing the watch makes
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

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/backlog", get(backlog))
        .route("/crown", get(crown))
        .route("/fleet", get(fleet))
        .route("/ws", get(ws_handler))
        .with_state(state);
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
    let body = PAGE.replacen("<!--fno-nav-->", &nav_fragment(NavPage::Live), 1);
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
}

#[derive(serde::Deserialize)]
struct WsQuery {
    t: Option<String>,
}

async fn backlog(Query(q): Query<WsQuery>, State(st): State<AppState>) -> Response {
    backlog_response(
        &st.graph_html,
        q.t.as_deref(),
        &st.token,
        "FNO_NO_OPEN=1 fno backlog view",
        NavPage::Backlog,
    )
    .await
}

async fn crown(Query(q): Query<WsQuery>, State(st): State<AppState>) -> Response {
    let authorized = token_ok(q.t.as_deref(), &st.token);
    let modified = std::fs::metadata(&st.reign_html)
        .and_then(|m| m.modified())
        .ok();
    if crown_needs_republish(authorized, modified, SystemTime::now()) {
        start_crown_republish(&st.reign_html);
    }
    backlog_response(
        &st.reign_html,
        q.t.as_deref(),
        &st.token,
        "fno agents king ledger (a render has started; reload in about a minute)",
        NavPage::Crown,
    )
    .await
}

async fn fleet(Query(q): Query<WsQuery>, State(st): State<AppState>) -> Response {
    backlog_response(
        &st.fleet_html,
        q.t.as_deref(),
        &st.token,
        "the fno-agents daemon; its fleet_page arm writes fleet.html every 30 minutes, or run fno-agents intel --fleet --html",
        NavPage::Fleet,
    )
    .await
}

/// Which page the bridge serves; the shared nav fragment marks the current
/// one so a reader always knows where they are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavPage {
    Live,
    Backlog,
    Crown,
    Fleet,
}

fn token_ok(supplied: Option<&str>, expected: &str) -> bool {
    supplied.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
}

/// Republish-on-read: a render costs about a minute, so the bridge serves the
/// current file at once and starts ONE background render when the page is
/// stale. `crown()` checks; this spawns through the fleet admission gate.
const CROWN_REPUBLISH_AFTER: Duration = Duration::from_secs(300);
const CROWN_REPUBLISH_TIMEOUT: Duration = Duration::from_secs(300);
static CROWN_REPUBLISHING: AtomicBool = AtomicBool::new(false);

/// Pure staleness verdict: unauthorized reads start nothing, a missing file
/// is maximally stale, a future mtime reads as fresh, and anything older
/// than the threshold needs a render.
fn crown_needs_republish(authorized: bool, modified: Option<SystemTime>, now: SystemTime) -> bool {
    if !authorized {
        return false;
    }
    match modified {
        None => true,
        Some(m) if m > now => false,
        Some(m) => now
            .duration_since(m)
            .map(|age| age > CROWN_REPUBLISH_AFTER)
            .unwrap_or(false),
    }
}

/// Single-flight background render. `--out` names the exact path the bridge
/// reads, so writer and reader agree whatever the Python state root resolves
/// to. Admission refusal, non-zero exit or timeout each log and clear the
/// flag, so a later read may retry.
fn start_crown_republish(out: &Path) {
    if CROWN_REPUBLISHING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return; // a render is already in flight
    }
    let out = out.to_path_buf();
    tokio::spawn(async move {
        let fno = std::env::current_exe().unwrap_or_else(|_| "fno".into());
        let mut cmd = crate::process_admission::tokio_command(&fno);
        cmd.args(["agents", "king", "ledger", "--out"]).arg(&out);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let result = match crate::process_admission::tokio_spawn(&mut cmd) {
            Err(e) => Err(format!("{e}")),
            Ok(mut child) => {
                let stderr = child.stderr.take();
                let tail = tokio::spawn(async move {
                    let mut buf = String::new();
                    if let Some(mut s) = stderr {
                        use tokio::io::AsyncReadExt;
                        let mut chunk = vec![0u8; 4096];
                        loop {
                            match s.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                            }
                        }
                    }
                    buf
                });
                match tokio::time::timeout(CROWN_REPUBLISH_TIMEOUT, child.wait()).await {
                    Err(_) => {
                        let _ = child.kill().await;
                        Err("timed out after 300s".to_string())
                    }
                    Ok(Err(e)) => Err(format!("{e}")),
                    Ok(Ok(status)) if !status.success() => {
                        let last = tail
                            .await
                            .unwrap_or_default()
                            .lines()
                            .last()
                            .unwrap_or_default()
                            .to_string();
                        Err(last)
                    }
                    Ok(Ok(_)) => Ok(()),
                }
            }
        };
        if let Err(e) = result {
            eprintln!("fno mux web: `fno agents king ledger --out <reign.html>` failed: {e}");
        }
        CROWN_REPUBLISHING.store(false, Ordering::SeqCst);
    });
}

/// One sticky nav shared by the live, backlog and crown pages. Links are
/// computed in the browser from `location.pathname`, so any mount prefix
/// works, and no prefix works too.
fn nav_fragment(current: NavPage) -> String {
    let name = |p: NavPage| match p {
        NavPage::Live => "live",
        NavPage::Backlog => "backlog",
        NavPage::Crown => "crown",
        NavPage::Fleet => "fleet",
    };
    let link = |p: NavPage| {
        if p == current {
            format!(
                "<a data-page=\"{}\" aria-current=\"page\" href=\"#\">{}</a>",
                name(p),
                name(p)
            )
        } else {
            format!("<a data-page=\"{}\" href=\"#\">{}</a>", name(p), name(p))
        }
    };
    // The backlog page's own filter bar is sticky at top:0 and would slide
    // under the nav, so only that page's fragment lifts it.
    let controls = if current == NavPage::Backlog {
        ".controls{top:var(--fno-nav-h)}"
    } else {
        ""
    };
    format!(
        "<nav class=\"fno-nav\" data-current=\"{}\" aria-label=\"fno pages\">\
         <style>:root{{--fno-nav-h:42px}}\
         nav.fno-nav{{position:sticky;top:0;z-index:1000;display:flex;align-items:center;gap:18px;\
         padding:6px 14px;background:#14181a;color:#e3e8e4;\
         font:13px/1 system-ui,-apple-system,Segoe UI,sans-serif;box-sizing:border-box}}\
         nav.fno-nav *{{box-sizing:border-box}}\
         nav.fno-nav a{{color:#b9c2cf;text-decoration:none;display:flex;align-items:center;\
         min-height:30px;padding:0 4px;border-bottom:2px solid transparent}}\
         nav.fno-nav a[aria-current=\"page\"]{{color:#fff;border-bottom-color:#c99b45}}\
         nav.fno-nav a:hover{{color:#fff}}{controls}</style>\
         {}{}{}{}\
         <script>(function(){{var nav=document.querySelector(\"nav.fno-nav\");if(!nav)return;\
         var p=location.pathname,base;\
         if(nav.dataset.current===\"live\"){{base=p.endsWith(\"/\")?p:p+\"/\";}}\
         else{{base=p.slice(0,p.lastIndexOf(\"/\")+1);}}\
         nav.dataset.base=base;\
         var q=new URLSearchParams(location.search),t=q.get(\"t\")||\"\",pj=q.get(\"project\");\
         nav.querySelectorAll(\"a\").forEach(function(a){{\
         var page=a.dataset.page;\
         a.href=base+(page===\"live\"?\"\":page)+\"?t=\"+encodeURIComponent(t)\
         +(pj?\"&project=\"+encodeURIComponent(pj):\"\");}});}})();</script></nav>",
        name(current),
        link(NavPage::Live),
        link(NavPage::Backlog),
        link(NavPage::Crown),
        link(NavPage::Fleet),
    )
}

/// Insert the nav fragment right after the first `<body ...>` tag (ASCII
/// case-insensitive; graph.html opens with `<body data-local="true">`).
/// With no body tag at all, prepend.
fn with_nav(html: &str, current: NavPage) -> String {
    let fragment = nav_fragment(current);
    let lower = html.to_ascii_lowercase();
    let injected = match lower
        .find("<body")
        .and_then(|start| lower[start..].find('>').map(|end| start + end + 1))
    {
        Some(at) => {
            let mut out = String::with_capacity(html.len() + fragment.len());
            out.push_str(&html[..at]);
            out.push_str(&fragment);
            out.push_str(&html[at..]);
            out
        }
        None => format!("{fragment}{html}"),
    };
    injected
}

async fn backlog_response(
    path: &Path,
    supplied: Option<&str>,
    expected: &str,
    render_hint: &str,
    nav: NavPage,
) -> Response {
    if !token_ok(supplied, expected) {
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
            with_nav(&body, nav),
        )
            .into_response(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("page not rendered; run {render_hint}"),
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

    /// A temp dir per test; tests using it must clean up after themselves.
    fn temp_state_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("web-stop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stop_with_no_state_file_reports_done() {
        let dir = temp_state_dir("none");
        let socket = dir.join("t.sock");
        assert_eq!(stop_web("t", &socket), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_removes_a_corpse_state_file() {
        let dir = temp_state_dir("corpse");
        let socket = dir.join("t.sock");
        // A pid proven dead: spawned, reaped, gone.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(proto::pid_confirmed_dead(pid as libc::pid_t));
        let state = web_state_path(&socket).unwrap();
        std::fs::write(
            &state,
            serde_json::json!({"pid": pid, "started": 1}).to_string(),
        )
        .unwrap();
        assert_eq!(stop_web("t", &socket), 0);
        assert!(!state.exists(), "the corpse file is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_refuses_to_signal_a_recycled_pid() {
        let dir = temp_state_dir("recycled");
        let socket = dir.join("t.sock");
        // pid names THIS live test process, but the recorded start token is
        // wrong: the identity check must read stale, and this process must
        // survive the verb.
        let own_start =
            proto::pid_start_time(std::process::id()).expect("start time on test platform");
        let state = web_state_path(&socket).unwrap();
        std::fs::write(
            &state,
            serde_json::json!({
                "pid": std::process::id(),
                "started": own_start.wrapping_add(1),
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(stop_web("t", &socket), 0);
        assert!(!state.exists(), "the stale file is gone");
    }

    #[test]
    fn state_file_records_the_pid_start_token() {
        let dir = temp_state_dir("token");
        let socket = dir.join("t.sock");
        let guard = WebStateFile::write(&socket, "127.0.0.1", 8722, "tok").unwrap();
        let raw = std::fs::read_to_string(web_state_path(&socket).unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["pid"].as_u64(), Some(std::process::id() as u64));
        assert_eq!(
            v["started"].as_u64(),
            proto::pid_start_time(std::process::id())
        );
        drop(guard);
        assert!(
            !web_state_path(&socket).unwrap().exists(),
            "Drop removes the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_status_is_valid_json_the_browser_keys_on() {
        let s = bridge_status("disconnected");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["_bridge"]["state"], "disconnected");
    }

    /// The served page must stay pinch-zoomable. A pane grid is as wide
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

    /// The retention diff decides how many rows scrolled off between
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

    /// The page must keep calling retention what it is. A protocol
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

    /// Fit-to-width is client-side only. The bridge attaches passive
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
        let response = backlog_response(
            &path,
            Some("right"),
            "right",
            "fno backlog view",
            NavPage::Backlog,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("PRIVATE-BACKLOG-MARKER"));

        let denied = backlog_response(
            &path,
            Some("wrong"),
            "right",
            "fno backlog view",
            NavPage::Backlog,
        )
        .await;
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
        let response = backlog_response(
            &dir.join("graph.html"),
            Some("right"),
            "right",
            "FNO_NO_OPEN=1 fno backlog view",
            NavPage::Backlog,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("FNO_NO_OPEN=1 fno backlog view"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_serves_the_shared_nav_not_absolute_links() {
        assert!(!PAGE.contains("\"/backlog?t="));
        assert!(!PAGE.contains("\"/crown?t="));
        assert!(!PAGE.contains("\"/fleet?t="));
        assert!(!PAGE.contains("${location.host}/ws"));
        assert!(PAGE.contains("<!--fno-nav-->"));
        assert!(PAGE.contains("const base = document.querySelector(\"nav.fno-nav\").dataset.base;"));
        assert!(PAGE.contains("${location.host}${base}ws?t="));
    }

    #[test]
    fn nav_fragment_marks_one_current_page_and_carries_the_query_parts() {
        for (page, name) in [
            (NavPage::Live, "live"),
            (NavPage::Backlog, "backlog"),
            (NavPage::Crown, "crown"),
            (NavPage::Fleet, "fleet"),
        ] {
            let frag = nav_fragment(page);
            // The CSS selector also names the attribute; count the link tags.
            assert_eq!(frag.matches("aria-current=\"page\" href=\"#\"").count(), 1);
            assert!(frag.contains(&format!("data-current=\"{name}\"")));
            assert!(frag.contains(&format!(
                "<a data-page=\"{name}\" aria-current=\"page\" href=\"#\">{name}</a>"
            )));
            assert!(frag.contains("encodeURIComponent(t)"));
            assert!(frag.contains("encodeURIComponent(pj)"));
        }
    }

    #[test]
    fn with_nav_inserts_after_the_body_tag() {
        let out = with_nav(
            "<html><body data-local=\"true\"><p>x</p></body></html>",
            NavPage::Backlog,
        );
        assert!(out.contains("<body data-local=\"true\"><nav class=\"fno-nav\""));
        let out = with_nav("<html><BODY><p>x</p></BODY></html>", NavPage::Crown);
        assert!(out.contains("<BODY><nav class=\"fno-nav\""));
        let out = with_nav("<p>no body</p>", NavPage::Live);
        assert!(out.starts_with("<nav class=\"fno-nav\""));
    }

    #[test]
    fn only_the_backlog_nav_offsets_the_controls_bar() {
        assert!(nav_fragment(NavPage::Backlog).contains(".controls{top:var(--fno-nav-h)}"));
        assert!(!nav_fragment(NavPage::Live).contains(".controls"));
        assert!(!nav_fragment(NavPage::Crown).contains(".controls"));
        assert!(!nav_fragment(NavPage::Fleet).contains(".controls"));
    }

    #[test]
    fn backlog_html_follows_state_root_not_graph_json_override() {
        let state = Path::new("/configured/state");
        assert_eq!(
            graph_html_path_from_state_root(state),
            PathBuf::from("/configured/state/graph.html")
        );
    }

    #[tokio::test]
    async fn crown_requires_token_and_serves_private_file_without_cache() {
        let dir = std::env::temp_dir().join(format!("fno-web-crown-{}-serve", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reign.html");
        std::fs::write(&path, "<body><p>PRIVATE-CROWN-MARKER</p></body>").unwrap();
        let response = backlog_response(
            &path,
            Some("right"),
            "right",
            "fno agents king ledger",
            NavPage::Crown,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("PRIVATE-CROWN-MARKER"));
        // The served crown page carries the shared nav (inserted after <body>).
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains("nav class=\"fno-nav\" data-current=\"crown\""));
        let denied = backlog_response(
            &path,
            Some("wrong"),
            "right",
            "fno agents king ledger",
            NavPage::Crown,
        )
        .await;
        assert_eq!(denied.status(), axum::http::StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(denied.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("PRIVATE-CROWN-MARKER"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_crown_names_the_render_action() {
        let dir =
            std::env::temp_dir().join(format!("fno-web-crown-{}-missing", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let response = backlog_response(
            &dir.join("reign.html"),
            Some("right"),
            "right",
            "fno agents king ledger (a render has started; reload in about a minute)",
            NavPage::Crown,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("fno agents king ledger"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crown_republish_truth_table() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Unauthorized never starts a render.
        assert!(!crown_needs_republish(false, None, now));
        // A missing file is maximally stale.
        assert!(crown_needs_republish(true, None, now));
        // Fresh, and a future mtime, read as fresh.
        let fresh = now.checked_sub(Duration::from_secs(10)).unwrap();
        assert!(!crown_needs_republish(true, Some(fresh), now));
        let future = now.checked_add(Duration::from_secs(10)).unwrap();
        assert!(!crown_needs_republish(true, Some(future), now));
        // Older than the threshold needs a render.
        let stale = now
            .checked_sub(CROWN_REPUBLISH_AFTER + Duration::from_secs(1))
            .unwrap();
        assert!(crown_needs_republish(true, Some(stale), now));
        // Exactly at the threshold is still fresh ("older than" is strict).
        let boundary = now.checked_sub(CROWN_REPUBLISH_AFTER).unwrap();
        assert!(!crown_needs_republish(true, Some(boundary), now));
    }

    #[tokio::test]
    async fn republish_is_single_flight() {
        // With the flag held, a second start returns without spawning: no task
        // ever runs, so the flag survives the call untouched.
        CROWN_REPUBLISHING.store(true, Ordering::SeqCst);
        start_crown_republish(Path::new("/tmp/never-written-reign.html"));
        tokio::task::yield_now().await;
        assert!(CROWN_REPUBLISHING.load(Ordering::SeqCst));
        CROWN_REPUBLISHING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn reign_html_follows_state_root_beside_graph_json() {
        let state = Path::new("/configured/state");
        assert_eq!(
            reign_html_path_from_state_root(state),
            PathBuf::from("/configured/state/reign.html")
        );
    }

    #[test]
    fn fleet_html_follows_state_root_beside_graph_json() {
        let state = Path::new("/configured/state");
        assert_eq!(
            fleet_html_path_from_state_root(state),
            PathBuf::from("/configured/state/fleet.html")
        );
    }

    #[tokio::test]
    async fn missing_fleet_names_the_daemon_arm() {
        let dir =
            std::env::temp_dir().join(format!("fno-web-fleet-{}-missing", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let response = backlog_response(
            &dir.join("fleet.html"),
            Some("right"),
            "right",
            "the fno-agents daemon; its fleet_page arm writes fleet.html every 30 minutes",
            NavPage::Fleet,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("fleet_page"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn router_serves_fleet_with_token_and_shared_nav() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = std::env::temp_dir().join(format!("fno-web-fleet-{}-route", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fleet_path = dir.join("fleet.html");
        std::fs::write(&fleet_path, "<html><body>FLEET-MARKER</body></html>").unwrap();
        let (tx, _) = broadcast::channel(4);
        let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let state = AppState {
            tx,
            snap: Arc::new(Mutex::new(Snapshot::default())),
            token: Arc::<str>::from("right"),
            graph_html: dir.join("graph.html"),
            reign_html: dir.join("reign.html"),
            fleet_html: fleet_path,
            shutdown,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"GET /fleet?t=right HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("Cache-Control: no-store"), "{reply}");
        assert!(reply.contains("data-current=\"fleet\""), "{reply}");
        assert!(reply.contains("FLEET-MARKER"), "{reply}");
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn state_file_carries_the_ownership_fields() {
        // The record answers "who started this and how old is its build":
        // binary path, build rev, wall-clock start, launcher session.
        let dir = temp_state_dir("own");
        let socket = dir.join("t.sock");
        let guard = WebStateFile::write(&socket, "127.0.0.1", 8944, "tok").expect("wrote state");
        let raw = std::fs::read_to_string(web_state_path(&socket).unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["port"], 8944);
        assert_eq!(v["pid"], u64::from(std::process::id()));
        assert_eq!(v["rev"], env!("FNO_MUX_CRATES_REV"));
        assert_eq!(v["bin"], std::env::current_exe().unwrap().to_str().unwrap());
        assert!(v["started_at"].as_u64().unwrap() > 0);
        match launching_session_id() {
            Some(id) => assert_eq!(v["session"], id),
            None => assert!(
                v["session"].is_null(),
                "no launcher id -> the field is null"
            ),
        }
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rev_is_stale_only_flags_known_different_revs() {
        assert!(!rev_is_stale(env!("FNO_MUX_CRATES_REV")));
        assert!(!rev_is_stale("unknown"), "cannot know, so never stale");
        assert!(rev_is_stale("deadbeef-old-build"));
    }

    #[test]
    fn live_bridge_records_reads_only_live_sibling_sessions() {
        let dir = temp_state_dir("scan");
        // Live: this test process under its real start token.
        let live = dir.join("web-live.json");
        std::fs::write(
            &live,
            serde_json::json!({
                "pid": std::process::id(),
                "started": proto::pid_start_time(std::process::id()),
                "port": 8945,
            })
            .to_string(),
        )
        .unwrap();
        // Corpse: a pid proven gone.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let corpse = dir.join("web-corpse.json");
        std::fs::write(
            &corpse,
            serde_json::json!({"pid": dead_pid, "started": 1, "port": 8946}).to_string(),
        )
        .unwrap();
        // Unrelated file shape: ignored.
        std::fs::write(dir.join("other.json"), "{}").unwrap();
        let found: Vec<_> = live_bridge_records(&dir)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            found,
            vec!["live".to_string()],
            "corpse and non-bridge skipped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_holder_filter_matches_only_the_taken_port() {
        let dir = temp_state_dir("holder");
        let holder = dir.join("web-hold.json");
        std::fs::write(
            &holder,
            serde_json::json!({
                "pid": std::process::id(),
                "started": proto::pid_start_time(std::process::id()),
                "port": 8947,
            })
            .to_string(),
        )
        .unwrap();
        let holders: Vec<_> = live_bridge_records(&dir)
            .into_iter()
            .filter(|(_, r)| r.get("port").and_then(|p| p.as_u64()) == Some(8947))
            .collect();
        assert_eq!(holders.len(), 1, "the port match names the holder");
        assert_eq!(
            holders[0].1.get("session").and_then(|s| s.as_str()),
            launching_session_id().as_deref(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_web_exits_one_with_no_state_file() {
        let dir = temp_state_dir("status-none");
        let socket = dir.join("t.sock");
        assert_eq!(status_web("t", &socket), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_web_exits_zero_for_a_live_own_pid_record() {
        let dir = temp_state_dir("status-live");
        let socket = dir.join("t.sock");
        let state = web_state_path(&socket).unwrap();
        std::fs::write(
            &state,
            serde_json::json!({
                "pid": std::process::id(),
                "started": proto::pid_start_time(std::process::id()),
                "bind": "127.0.0.1",
                "port": 8948,
                "rev": env!("FNO_MUX_CRATES_REV"),
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(status_web("t", &socket), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_web_refuses_a_recycled_pid_record() {
        let dir = temp_state_dir("status-recycled");
        let socket = dir.join("t.sock");
        let state = web_state_path(&socket).unwrap();
        // This live test process under a WRONG start token: the record reads
        // stale, so status reports no bridge instead of a stranger's pid.
        std::fs::write(
            &state,
            serde_json::json!({
                "pid": std::process::id(),
                "started": 1,
                "bind": "127.0.0.1",
                "port": 8949,
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(status_web("t", &socket), 1);
        assert!(state.exists(), "status is a read door: it never deletes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
