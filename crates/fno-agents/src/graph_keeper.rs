//! The store keeper: a per-graph process that OWNS a graph file and serves
//! every consumer over a unix socket, modeled on `pane_keeper.rs` ("the
//! keeper keeps, the server views", docs/architecture/pane-keeper.md). The
//! daemon is a subscriber exactly as the mux server is for panes; a daemon
//! restart does not take the store with it, and the next connection to
//! arrive re-adopts the seat.
//!
//! Frame protocol (the pane keeper's shape: `u8 tag | u32 LE length |
//! payload`, with the protocol version riding the Identify reply):
//! Client -> keeper: `Request(json)`, `Shutdown`, `Identify`.
//! Keeper -> client: `Response(json)`, `IdentifyReply(json)`.
//!
//! Requests are one-shot JSON: `{"id": n, "method": ..., "params": {...}}`.
//! Methods: `op`, `api`, `read_ids`, `read_file`, `plan_refs`, `export_now`,
//! `export_status`, `set_backend`, `backend_status`, `write_status`,
//! plus the pure verbs (`defaults`, `recompute`, `ready`, `overlay`, `settle_edges`,
//! `normalize_plan_path`, `canonical_field_order`, `scoreboard_classify`, `pure_op`).
//! Responses: `{"id": n, "ok": true, "result": ...}` or
//! `{"id": n, "ok": false, "error": {"kind": ..., "message": ...}}`.
//!
//! Unlike the pane keeper's single subscriber, every connection is served
//! concurrently: reads share the state gate (`RwLock` read guards), writes
//! exclude on it (`write` guards) and on the bounded flock. A store RPC must
//! never starve a concurrent client or a SIGTERM (the `gc_sweep` lesson,
//!): each request runs on its own thread, and the accepting loop never
//! blocks on request work.
//!
//! Reapability is a release condition (the 2026-09-01 seven-unreaped-keepers
//! measurement): this lane declares itself the way keeper_lane.py discovers
//! keepers, through `--sock`/`--session` on its own argv plus the
//! `--store-keeper` lane flag, and its socket lives beside the graph file it
//! owns, so both the process-table walk and the socket-dir walk find it.

use crate::graph_store::{self, FieldUpdate, MutateInput, StoreError};
use crate::identity::{harness_of_session_id, shape_known_harness};

mod seat_lock;

mod splice;

use seat_lock::take_seat;
use splice::splice_reply;

use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// The store keeper frame protocol version. Bump on any frame-shape change.
pub const PROTOCOL_VERSION: u32 = 1;

// Frame tags. Client -> keeper then keeper -> client.
pub(crate) const TAG_REQUEST: u8 = 1;
pub(crate) const TAG_SHUTDOWN: u8 = 2;
pub(crate) const TAG_IDENTIFY: u8 = 3;
pub(crate) const TAG_RESPONSE: u8 = 4;
pub(crate) const TAG_IDENTIFY_REPLY: u8 = 5;

/// One request/response frame exchange bound. Sized for a large operator
/// graph's entries array, not the daemon protocol's cap: a canonical
/// graph.json of 11 MB answers a `read` with a same-order JSON array.
pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Parsed `--store-keeper` lane argv:
/// `--store-keeper --sock <path> --graph <path> [--session <id>]
/// [--canonical] [--lock-timeout-secs N]`. The backend is not argv state:
/// the store names it in graph_meta and every request re-reads it.
pub struct KeeperConfig {
    pub sock: PathBuf,
    pub graph: PathBuf,
    pub session: String,
    /// True when `--graph` IS the configured canonical graph: gates the
    /// closure-release hook and canonical-board effects.
    pub canonical: bool,
    pub lock_timeout: Duration,
    /// Project journal receiving bounded write-gate aggregates.
    pub events: Option<PathBuf>,
    /// Idle self-exit bound. A keeper is long-lived by design in production,
    /// but its spawner can vanish without a Shutdown frame - a crashed CLI,
    /// a killed pytest worker above all - and one orphan per fixture graph
    /// compounded into thousands of live workers on one machine (measured
    /// 2026-09-03: 6,955 keepers after one day of pytest runs, load 117).
    /// With this set, a keeper with zero client threads and no accepted
    /// connection for this long exits and unlinks its socket; the next
    /// client re-spawns it. Default: ten minutes. `FNO_STORE_KEEPER_IDLE_SECS`
    /// overrides it; that variable set to 0 disables idle exit entirely.
    pub idle_limit: Option<Duration>,
}

/// The default idle bound for a keeper whose spawner set no override.
pub const DEFAULT_IDLE_LIMIT: Option<Duration> = Some(Duration::from_secs(600));

pub fn parse_store_keeper_args(args: &[String]) -> Result<KeeperConfig, String> {
    let mut sock: Option<String> = None;
    let mut graph: Option<String> = None;
    let mut session = String::new();
    let mut canonical = false;
    let mut lock_timeout = graph_store::DEFAULT_LOCK_TIMEOUT;
    let mut events = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store-keeper" => {}
            "--sock" => sock = Some(it.next().ok_or("--sock needs a value")?.clone()),
            "--graph" => graph = Some(it.next().ok_or("--graph needs a value")?.clone()),
            "--session" => session = it.next().ok_or("--session needs a value")?.clone(),
            "--canonical" => canonical = true,
            "--lock-timeout-secs" => {
                let v: u64 = it
                    .next()
                    .ok_or("--lock-timeout-secs needs a value")?
                    .parse()
                    .map_err(|_| "--lock-timeout-secs needs a number")?;
                lock_timeout = Duration::from_secs(v);
            }
            "--events" => events = Some(PathBuf::from(it.next().ok_or("--events needs a value")?)),
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    let idle_limit = match std::env::var("FNO_STORE_KEEPER_IDLE_SECS") {
        Ok(v) if v.trim() == "0" => None,
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs),
        // No override: the default bound. A keeper the spawner abandoned
        // still dies, on a clock long enough that an operator's back-to-back
        // verbs keep one keeper alive across the session.
        Err(_) => DEFAULT_IDLE_LIMIT,
    };
    Ok(KeeperConfig {
        sock: PathBuf::from(sock.ok_or("missing --sock")?),
        graph: PathBuf::from(graph.ok_or("missing --graph")?),
        session,
        canonical,
        lock_timeout,
        events,
        idle_limit,
    })
}

fn encode(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

enum Incoming {
    Request(Vec<u8>),
    Shutdown,
    Identify,
    HungUp,
    Violation(String),
}

fn decode_frame(buf: &[u8]) -> (Option<Incoming>, usize) {
    if buf.len() < 5 {
        return (None, 0);
    }
    let tag = buf[0];
    let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if len > MAX_FRAME_BYTES {
        return (
            Some(Incoming::Violation(format!(
                "frame of {len} bytes exceeds the cap"
            ))),
            buf.len(),
        );
    }
    if buf.len() < 5 + len {
        return (None, 0);
    }
    let payload = buf[5..5 + len].to_vec();
    let frame = match tag {
        TAG_REQUEST => Incoming::Request(payload),
        TAG_SHUTDOWN => Incoming::Shutdown,
        TAG_IDENTIFY => Incoming::Identify,
        other => {
            return (
                Some(Incoming::Violation(format!(
                    "frame tag {other} with {len} payload byte(s) is not a store frame"
                ))),
                5 + len,
            )
        }
    };
    (Some(frame), 5 + len)
}

fn read_one_frame(stream: &mut UnixStream) -> Incoming {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let (frame, used) = decode_frame(&buf);
        if let Some(frame) = frame {
            let _ = used;
            return frame;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Incoming::HungUp,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

const WRITE_LEDGER_CAPACITY: usize = 64;
const WRITE_LEDGER_TTL: Duration = Duration::from_secs(600);

enum WriteLedgerState {
    InFlight,
    Done(Value),
}

pub(crate) struct WriteLedgerEntry {
    request_id: String,
    recorded_at: std::time::Instant,
    state: WriteLedgerState,
}

/// The keeper's shared state. Writes exclude here; reads hold shared guards,
/// so concurrent reads overlap and every read still waits out an in-flight
/// publish rather than observing one.
pub(crate) struct StoreState {
    pub(crate) graph: PathBuf,
    pub(crate) canonical: bool,
    pub(crate) lock_timeout: Duration,
    /// Readers share, writers exclude: read guards for handlers that only
    /// read the owned graph, write guards for the ones that publish.
    pub(crate) gate: RwLock<()>,
    /// One read guard per in-flight REQUEST, held from handle_request through
    /// the reply write. Shutdown ladders on the write guard, so it cannot cut
    /// a request that is mid-publish or mid-reply (the old ladder
    /// dropped its guard before exit and a later request died mid-frame with
    /// its client reading a hangup for a write that answered ok).
    pub(crate) inflight: RwLock<()>,
    pub(crate) write_ledger: Mutex<std::collections::VecDeque<WriteLedgerEntry>>,
    pub(crate) gate_metrics: Mutex<GateMetrics>,
    /// The instant of the last successful publish. The render trigger reads
    /// it to debounce: the pass runs once the store has been quiet for the
    /// settle window, never per write.
    pub(crate) last_write: Mutex<Option<std::time::Instant>>,
    /// True while the render trigger's subprocess runs, so overlapping 1 s
    /// ticks never stack two passes.
    pub(crate) render_in_flight: std::sync::atomic::AtomicBool,
    /// Consecutive render failures: drives the retry backoff, reset on the
    /// first success, so a keeper whose view pass can never run stops paying
    /// one spawn plus one durable event per second.
    pub(crate) render_failures: std::sync::atomic::AtomicU32,
    /// The instant the trigger last RAN a pass (success or failure): the
    /// backoff measures quiet time against this, not the tick clock.
    pub(crate) last_render_attempt: Mutex<Option<std::time::Instant>>,
    pub(crate) events: Option<PathBuf>,
    /// The (dev, ino) of the socket path at bind time: the seat's proof.
    /// Unlinks are guarded by it, and an idle keeper whose path was rebound
    /// stands down (AC2-ERR).
    pub(crate) sock_ino: Option<(u64, u64)>,
    /// The build this keeper process launched from: drift is computed fresh
    /// at every Identify, and the WouldBlock arm self-retires when the
    /// binary under the keeper is rewritten while it idles.
    pub(crate) startup_fp: Option<crate::drift::ExeFingerprint>,
}

const GATE_WINDOW: Duration = Duration::from_secs(300);
const WAIT_BOUNDS_MS: [u64; 12] = [
    1,
    5,
    10,
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_500,
    5_000,
    u64::MAX,
];

pub(crate) struct GateMetrics {
    started: std::time::Instant,
    started_epoch_ms: u128,
    counts: [u64; WAIT_BOUNDS_MS.len()],
    mutations: u64,
    bytes_written: u64,
    retries: u64,
}

impl GateMetrics {
    pub(crate) fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            started_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_millis())
                .unwrap_or(0),
            counts: [0; WAIT_BOUNDS_MS.len()],
            mutations: 0,
            bytes_written: 0,
            retries: 0,
        }
    }
}

fn flush_gate_metrics(state: &StoreState) {
    let Some(path) = &state.events else { return };
    let mut metrics = state
        .gate_metrics
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let elapsed = metrics.started.elapsed();
    let replacement = GateMetrics::new();
    let finished_epoch_ms = replacement.started_epoch_ms;
    let completed = std::mem::replace(&mut *metrics, replacement);
    drop(metrics);
    let bounds: Vec<Value> = WAIT_BOUNDS_MS
        .iter()
        .map(|bound| {
            if *bound == u64::MAX {
                Value::String("inf".into())
            } else {
                json!(bound)
            }
        })
        .collect();
    let emitter = crate::events::EventEmitter::new(path, "daemon");
    let _ = emitter.emit(
        "graph_write_gate",
        &json!({
            "keeper_pid": std::process::id(),
            "window_started_ms": completed.started_epoch_ms,
            "window_finished_ms": finished_epoch_ms,
            "completed_window_seconds": elapsed.as_secs_f64(),
            "wait_ms_bounds": bounds,
            "wait_ms_counts": completed.counts,
            "mutation_count": completed.mutations,
            "bytes_written": completed.bytes_written,
            "retry_count": completed.retries,
        }),
    );
}

/// Exit code for a keeper that found its seat owned: the Python spawner
/// (`store.py:_client_for`) reads this number and keeps polling the
/// incumbent instead of failing the spawn.
pub const EXIT_SEAT_OWNED: i32 = 3;

/// How long the store must stay quiet after the last write before the view
/// pass runs: a burst of writes renders once, not once per write.
const RENDER_SETTLE: Duration = Duration::from_secs(2);

/// Whether the canonical view pass is owed right now: the store's version
/// moved since the last render AND the write burst has settled. `None`
/// last-write reads as settled (a keeper born after writes still owes the
/// catch-up render). The settle is a parameter so tests skip the wait.
fn render_due(state: &StoreState, current: &str, rendered: Option<&str>, settle: Duration) -> bool {
    if rendered == Some(current) {
        return false;
    }
    let last = state
        .last_write
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    match last {
        Some(t) => t.elapsed() >= settle,
        None => true,
    }
}

/// One tick of the render trigger: when a pass is owed and none runs, run
/// the canonical view pass (the same `fno backlog render-views` a CLI write
/// used to run in-call), stamp `rendered_version` on success, and journal
/// `graph_render_failed` on failure. Never touches the write gate: a slow or
/// failing render delays nothing, and the next settled tick retries.
fn trigger_render(state: &StoreState) {
    trigger_render_with(state, run_render_pass);
}

/// Backoff between render attempts after consecutive failures: double from
/// the settle floor each time, capped at ten minutes. A permanently failing
/// pass then costs one spawn per cap window, not one per second.
fn render_backoff(failures: u32) -> Duration {
    let shift = failures.min(10);
    RENDER_SETTLE
        .checked_mul(1u32 << shift)
        .unwrap_or(Duration::from_secs(600))
        .min(Duration::from_secs(600))
}

/// The injectable body of [`trigger_render`]: tests pass their own pass
/// runner instead of the subprocess.
fn trigger_render_with(state: &StoreState, run: impl FnOnce() -> Result<(), (i32, String)>) {
    use std::sync::atomic::Ordering;
    let Ok(current) = crate::backlog::version(&state.graph) else {
        return; // no counter yet: nothing was ever written, nothing to render
    };
    let rendered = crate::backlog::rendered_version(&state.graph)
        .ok()
        .flatten();
    if !render_due(state, &current, rendered.as_deref(), RENDER_SETTLE) {
        return;
    }
    // Backoff gate: after failures, wait out the doubled quiet window since
    // the last attempt before paying for another one. A fresh write still
    // renders at the settle floor: the backoff only delays RETRIES of a pass
    // that already failed at this version.
    let failures = state.render_failures.load(Ordering::SeqCst);
    if failures > 0 {
        if let Some(last) = state
            .last_render_attempt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            if last.elapsed() < render_backoff(failures) {
                return;
            }
        }
    }
    if state
        .render_in_flight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    {
        let mut last = state
            .last_render_attempt
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *last = Some(std::time::Instant::now());
    }
    let outcome = run();
    match &outcome {
        Ok(()) => {
            if let Err(error) = crate::backlog::set_rendered_version(&state.graph, &current) {
                state.render_failures.fetch_add(1, Ordering::SeqCst);
                if let Some(events) = &state.events {
                    let emitter = crate::events::EventEmitter::new(events, "daemon");
                    let _ = emitter.emit(
                        "graph_render_failed",
                        &json!({
                            "version": current,
                            "exit": -1,
                            "stderr_tail": format!("rendered_version stamp failed: {error}"),
                        }),
                    );
                }
            } else {
                state.render_failures.store(0, Ordering::SeqCst);
            }
        }
        Err((exit, stderr_tail)) => {
            state.render_failures.fetch_add(1, Ordering::SeqCst);
            if let Some(events) = &state.events {
                let emitter = crate::events::EventEmitter::new(events, "daemon");
                let _ = emitter.emit(
                    "graph_render_failed",
                    &json!({
                        "version": current,
                        "exit": exit,
                        "stderr_tail": stderr_tail,
                    }),
                );
            }
        }
    }
    state.render_in_flight.store(false, Ordering::SeqCst);
}

/// Run the canonical view pass as a subprocess. The keeper is a Rust worker:
/// the pass is Python-owned (config.toml targets, vault rendering), so it
/// shells to the `fno` CLI the same way the mux's retired replay did.
/// `Err` carries the exit code and the stderr tail for the failure event.
fn run_render_pass() -> Result<(), (i32, String)> {
    let bin = match std::env::var_os("FNO_BIN") {
        Some(v) => PathBuf::from(v),
        None => {
            let path = match std::env::var_os("PATH") {
                Some(p) => p,
                None => return Err((-1, "PATH is unset; the view pass cannot run".into())),
            };
            match std::env::split_paths(&path)
                .map(|dir| dir.join("fno"))
                .find(|candidate| candidate.is_file())
            {
                Some(p) => p,
                None => {
                    return Err((-1, "no fno CLI on PATH; the view pass cannot run".into()));
                }
            }
        }
    };
    let out = std::process::Command::new(bin)
        .args(["backlog", "render-views"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| (-1, format!("spawn failed: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Tail only, char-safe: the emitter caps payloads at 500 B, so the tail
    // must leave room for the envelope fields.
    let tail: String = {
        let skip = stderr.chars().count().saturating_sub(300);
        stderr.chars().skip(skip).collect()
    };
    Err((code, tail))
}

/// How often an idle keeper re-checks that the socket path still names the
/// inode it bound.
const SEAT_CHECK_EVERY: Duration = Duration::from_secs(1);

/// One Identify with a short reply bound: true only when something behind
/// the path answers. An answering incumbent predates the seat lock (it was
/// built before this change); a refusal or silence is a dead leftover the
/// caller may clear.
fn a_live_keeper_answers(sock: &Path, bound: Duration) -> bool {
    let Ok(mut stream) = UnixStream::connect(sock) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(bound));
    let _ = stream.set_write_timeout(Some(bound));
    if stream.write_all(&encode(TAG_IDENTIFY, b"")).is_err() {
        return false;
    }
    let mut header = [0u8; 5];
    if stream.read_exact(&mut header).is_err() {
        return false;
    }
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len.min(1 << 20)];
    stream.read_exact(&mut payload).is_ok()
}

/// True while the socket path still names the inode THIS keeper bound. A
/// keeper never unlinks a socket it does not own, and an idle keeper whose
/// path was rebound stands down instead of serving a phantom.
fn seat_still_ours(sock: &Path, sock_ino: Option<(u64, u64)>) -> bool {
    match sock_ino {
        None => true,
        Some(mine) => match std::fs::metadata(sock) {
            Ok(md) => (md.dev(), md.ino()) == mine,
            Err(_) => false,
        },
    }
}

/// Run the store keeper to completion. Returns only on a startup failure;
/// a Shutdown frame ends the process from inside.
pub fn run(cfg: KeeperConfig) -> Result<(), String> {
    // SAFETY: setsid before any thread exists; a group-kill aimed at a
    // spawning process's group must not take the store with it.
    unsafe {
        libc::setsid();
    }
    // Seat flock BEFORE touching the socket: the loser exits 3 and the
    // Python spawner keeps polling the incumbent rather than respawning.
    // `_seat` is a named binding, the daemon's bind_supervisor_socket
    // shape: the File holds the flock, so an `if take_seat(..).is_none()`
    // temporary drops at the end of the condition and the lock guards
    // nothing.
    let Some(_seat) = take_seat(&cfg.sock) else {
        eprintln!(
            "store keeper: {} is owned by a live keeper (lock held); exiting",
            cfg.sock.display()
        );
        std::process::exit(EXIT_SEAT_OWNED);
    };
    // A keeper built before the seat lock can still own the path. With the
    // flock held, one short-bound Identify decides: an answerer is a live
    // incumbent, a refusal or silence is a dead leftover.
    if cfg.sock.exists() && a_live_keeper_answers(&cfg.sock, Duration::from_millis(750)) {
        eprintln!(
            "store keeper: {} is owned by a live keeper (Identify answered); exiting",
            cfg.sock.display()
        );
        std::process::exit(EXIT_SEAT_OWNED);
    }
    // Connect-before-bind: a live listener IS the seat, whatever its build
    // (a loaded incumbent can answer Identify too slowly for the probe above
    // and still own the path), so the loser exits EXIT_SEAT_OWNED and the
    // Python spawner keeps polling the incumbent instead of failing.
    if UnixStream::connect(&cfg.sock).is_ok() {
        eprintln!(
            "store keeper: {} is owned by a live keeper (connect before bind); exiting",
            cfg.sock.display()
        );
        std::process::exit(EXIT_SEAT_OWNED);
    }
    let _ = std::fs::remove_file(&cfg.sock);
    if let Some(parent) = cfg.sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let listener = match UnixListener::bind(&cfg.sock) {
        Ok(l) => l,
        // A listener appeared between the probe and the bind: the seat filled.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "store keeper: {} is owned by a live keeper (bind in use); exiting",
                cfg.sock.display()
            );
            std::process::exit(EXIT_SEAT_OWNED);
        }
        Err(e) => return Err(format!("cannot bind {}: {e}", cfg.sock.display())),
    };
    let sock_ino = std::fs::metadata(&cfg.sock)
        .ok()
        .map(|md| (md.dev(), md.ino()));
    let startup_fp = crate::drift::ExeFingerprint::current();

    let state = Arc::new(StoreState {
        graph: cfg.graph.clone(),
        canonical: cfg.canonical,
        lock_timeout: cfg.lock_timeout,
        gate: RwLock::new(()),
        inflight: RwLock::new(()),
        write_ledger: Mutex::new(std::collections::VecDeque::new()),
        gate_metrics: Mutex::new(GateMetrics::new()),
        last_write: Mutex::new(None),
        render_in_flight: std::sync::atomic::AtomicBool::new(false),
        render_failures: std::sync::atomic::AtomicU32::new(0),
        last_render_attempt: Mutex::new(None),
        events: cfg.events.clone(),
        sock_ino,
        startup_fp,
    });
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let identify = json!({
        "v": PROTOCOL_VERSION,
        "keeper_pid": std::process::id(),
        "graph": cfg.graph.display().to_string(),
        "session": cfg.session,
        "started_at": started_at,
        // The live backend is stamped onto every Identify reply in
        // serve_client, not frozen here.
    })
    .to_string()
    .into_bytes();

    let shutdown = Arc::new(AtomicU64::new(0));
    if state.canonical || state.events.is_some() {
        let metrics_state = Arc::clone(&state);
        let metrics_shutdown = Arc::clone(&shutdown);
        let _ = std::thread::Builder::new()
            .name("fno-store-metrics".into())
            .spawn(move || loop {
                std::thread::sleep(GATE_WINDOW);
                if metrics_shutdown.load(Ordering::SeqCst) == 1 {
                    break;
                }
                flush_gate_metrics(&metrics_state);
            });
    }
    // The background export thread is deleted (task 10.1): after the flip
    // graph.json is written only by `fno doctor graph export --now`, and a
    // reader left on graph.json must see it freeze rather than a 60 s stale
    // copy (Risk 5).
    if state.canonical {
        // The ONE render path (task 8.3): a 1 s tick checks whether the
        // store moved since the last render and the last write settled, and
        // runs the canonical view pass once per burst. Canonical keepers
        // only: test and temp stores never render (the historical
        // non-canonical arm rendered a sibling graph.html, which no consumer
        // of test graphs ever read).
        let render_state = Arc::clone(&state);
        let render_shutdown = Arc::clone(&shutdown);
        let _ = std::thread::Builder::new()
            .name("fno-store-render".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                if render_shutdown.load(Ordering::SeqCst) == 1 {
                    break;
                }
                trigger_render(&render_state);
            });
    }
    let active_clients = Arc::new(AtomicU64::new(0));
    let mut last_activity = std::time::Instant::now();
    let mut last_seat_check = std::time::Instant::now();
    let mut last_drift_check = std::time::Instant::now();
    // change 3: the drift tick period. 30s default, env-overridable
    // for tests, next to its idle-exit sibling's override.
    let drift_check_every = std::env::var("FNO_STORE_KEEPER_DRIFT_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30));
    // A test-owned fixture store (argv carries FNO_TEST_OWNER_PID/BIRTH) is
    // bound to that test run's lifetime, not the longer-lived idle bound
    // above: a wedged test that never sends Shutdown must not leak this
    // store past its own run. The watchdog sets the SAME `shutdown` flag an
    // explicit Shutdown frame does, so the accept loop below needs no
    // separate owner-liveness check of its own.
    if let Some((owner_pid, owner_birth)) = crate::test_run::declared_owner_from_env() {
        let shutdown = Arc::clone(&shutdown);
        crate::test_run::spawn_owner_watchdog(
            owner_pid,
            owner_birth,
            "fno-store-test-owner",
            move || {
                eprintln!(
                "fno-agents-worker: test_keeper_reaped graph_keeper owner_pid={owner_pid} owner_birth={owner_birth}"
            );
                shutdown.store(1, Ordering::SeqCst);
            },
        );
    }
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("cannot poll {}: {e}", cfg.sock.display()))?;
    loop {
        if shutdown.load(Ordering::SeqCst) == 1 {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                last_activity = std::time::Instant::now();
                // BSD accept() hands the listener's O_NONBLOCK to the accepted
                // socket, and serve_client does blocking reads: an inherited
                // non-blocking stream reads WouldBlock and hangs up before
                // the client's first frame lands. Restore blocking mode.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let state = Arc::clone(&state);
                let identify = identify.clone();
                let shutdown = Arc::clone(&shutdown);
                let active_clients = Arc::clone(&active_clients);
                active_clients.fetch_add(1, Ordering::SeqCst);
                // One thread per connection: a slow store call never starves
                // the accept loop, and a client connecting while the daemon
                // gets SIGTERM is served rather than queued behind it.
                let spawned = std::thread::Builder::new()
                    .name("fno-store-cli".into())
                    .spawn({
                        let active_clients = Arc::clone(&active_clients);
                        move || {
                            serve_client(state, stream, identify, shutdown);
                            active_clients.fetch_sub(1, Ordering::SeqCst);
                        }
                    });
                if spawned.is_err() {
                    active_clients.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Seat check while idle: once a second, an idle keeper
                // confirms the path still names the inode it bound. Lost
                // seat -> stand down WITHOUT unlinking (the post-loop unlink
                // is inode-guarded, so the rebinding keeper's socket stays).
                if active_clients.load(Ordering::SeqCst) == 0
                    && last_seat_check.elapsed() >= SEAT_CHECK_EVERY
                {
                    last_seat_check = std::time::Instant::now();
                    if !seat_still_ours(&cfg.sock, sock_ino) {
                        // The same verdict the pre-bind ladder spells 3, so
                        // spell 3 here too: falling through reports a robbed
                        // seat as success (the racer-0 exit-0 in the CI race).
                        // The path is not ours; skip the inode-guarded
                        // unlink below the loop as well.
                        eprintln!(
                            "store keeper: {} is owned by a live keeper (inode moved); exiting",
                            cfg.sock.display()
                        );
                        std::process::exit(EXIT_SEAT_OWNED);
                    }
                }
                // Drift self-retire (change 3): a keeper idling on a
                // binary that a rebuild replaced is a stale server no
                // restart reaches. Every drift tick with no client,
                // re-stat the own executable; Drifted -> break so the
                // inode-guarded unlink runs and the next caller respawns
                // on the installed binary.
                if active_clients.load(Ordering::SeqCst) == 0
                    && last_drift_check.elapsed() >= drift_check_every
                {
                    last_drift_check = std::time::Instant::now();
                    if let Some(fp) = &state.startup_fp {
                        if matches!(
                            crate::drift::self_drift(fp),
                            crate::drift::DriftState::Drifted { .. }
                        ) {
                            break;
                        }
                    }
                }
                if let Some(limit) = cfg.idle_limit {
                    if active_clients.load(Ordering::SeqCst) == 0
                        && last_activity.elapsed() >= limit
                    {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // A peer resetting between connect and accept surfaces here
            // (Linux ECONNABORTED); one dropped probe must not retire a
            // keeper that owns the seat. Interrupted accept retries too.
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionAborted
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            // Anything past WouldBlock/Aborted/Interrupted is a listener
            // that can no longer accept: a broken keeper must not report
            // success (worker.rs prints the Err and exits 2).
            Err(e) => return Err(format!("accept failed on {}: {e}", cfg.sock.display())),
        }
    }
    // Unlink only what we still own: after a seat loss the path names the
    // rebinding keeper's socket, and removing it would kill THEIR listener.
    if seat_still_ours(&cfg.sock, sock_ino) {
        let _ = std::fs::remove_file(&cfg.sock);
    }
    Ok(())
}

fn serve_client(
    state: Arc<StoreState>,
    mut stream: UnixStream,
    identify: Vec<u8>,
    shutdown: Arc<AtomicU64>,
) {
    loop {
        match read_one_frame(&mut stream) {
            Incoming::HungUp => return,
            Incoming::Violation(msg) => {
                // The message names the violation for whoever runs the keeper
                // in the foreground; a detached keeper's stderr is its
                // spawner's problem, and the client sees a plain hangup.
                eprintln!("store keeper: protocol violation: {msg}");
                return;
            }
            Incoming::Identify => {
                // Build + drift computed LIVE at each Identify: a binary
                // rewritten after this keeper started reads drifted in the
                // next census, not one restart behind. New JSON keys are
                // not a frame-shape change (PROTOCOL_VERSION stays 1).
                let mut id: Value = serde_json::from_slice(&identify).unwrap_or(json!({}));
                if let Some(obj) = id.as_object_mut() {
                    obj.insert(
                        "store_backend".to_string(),
                        json!(crate::backlog::BACKEND_NAME),
                    );
                }
                if let (Some(obj), Some(fp)) = (id.as_object_mut(), &state.startup_fp) {
                    obj.insert(
                        "build".to_string(),
                        json!({
                            "path": fp.path.display().to_string(),
                            "mtime_nanos": fp.mtime_nanos,
                            "size": fp.size,
                        }),
                    );
                    obj.insert(
                        "drift".to_string(),
                        json!(crate::drift::drift_label(&crate::drift::self_drift(fp))),
                    );
                }
                let _ = stream.write_all(&encode(TAG_IDENTIFY_REPLY, id.to_string().as_bytes()));
                let _ = stream.flush();
            }
            Incoming::Shutdown => {
                // Explicit shutdown: acknowledge, unlink, exit. A daemon
                // restart never sends this frame, which is exactly the
                // survived-hangup vs survived-close line; an explicit
                // shutdown ends the process here, so in-flight writers on
                // other threads are bounded by the atomic-replace publish.
                // Wait out in-flight REQUESTS first: a bounded try_write
                // ladder on the inflight lock;
                // when it cannot land within lock_timeout, answer busy and
                // KEEP SERVING instead of exiting mid-write. Once held, the
                // guard stays held until exit: no request is mid-publish or
                // mid-reply, and a request arriving later blocks until the
                // process dies under it.
                let deadline = std::time::Instant::now() + state.lock_timeout;
                let mut inflight_guard: Option<std::sync::RwLockWriteGuard<'_, ()>> = None;
                while std::time::Instant::now() < deadline {
                    if let Ok(g) = state.inflight.try_write() {
                        inflight_guard = Some(g);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let Some(inflight_guard) = inflight_guard else {
                    let _ = stream.write_all(&encode(
                        TAG_RESPONSE,
                        json!({"id": 0, "ok": false, "error": {"kind": "busy",
                              "message": "a mutation is in flight"}})
                        .to_string()
                        .as_bytes(),
                    ));
                    let _ = stream.flush();
                    return;
                };
                // Held to exit: never dropped before process::exit(0). Once
                // the write guard is ours, no request is mid-publish or
                // mid-reply, and a request arriving later blocks on
                // inflight.read() until the process dies under it.
                let _ = &inflight_guard;
                let _ = stream.write_all(&encode(
                    TAG_RESPONSE,
                    json!({"id": 0, "ok": true, "result": "shutdown"})
                        .to_string()
                        .as_bytes(),
                ));
                let _ = stream.flush();
                shutdown.store(1, Ordering::SeqCst);
                let sock = store_socket_for(&state.graph);
                if seat_still_ours(&sock, state.sock_ino) {
                    let _ = std::fs::remove_file(&sock);
                }
                std::process::exit(0);
            }
            Incoming::Request(payload) => {
                // Hold an inflight guard from handling through the
                // reply write. Shutdown ladders on this lock, so a request
                // that is mid-publish or mid-reply cannot be cut by an
                // exiting keeper.
                let _inflight = state.inflight.read().unwrap_or_else(|e| e.into_inner());
                let reply = handle_request(&state, &payload);
                let body = serde_json::to_vec(&reply).unwrap_or_else(|_| {
                    json!({"id": 0, "ok": false,
                           "error": {"kind": "internal", "message": "reply serialization failed"}})
                    .to_string()
                    .into_bytes()
                });
                if stream.write_all(&encode(TAG_RESPONSE, &body)).is_err()
                    || stream.flush().is_err()
                {
                    return;
                }
            }
        }
    }
}

pub(crate) fn err_reply(id: u64, kind: &str, message: String) -> Value {
    json!({"id": id, "ok": false, "error": {"kind": kind, "message": message}})
}

fn store_err_kind(err: &StoreError) -> &'static str {
    match err {
        StoreError::Corrupt(_) => "corrupt",
        StoreError::Unreadable(_, _) => "unreadable",
        StoreError::MalformedRoot(_) => "malformed_root",
        StoreError::LockTimeout(_, _) => "lock_timeout",
        StoreError::Conflict => "conflict",
        StoreError::EmptyFieldUpdate(_) => "empty_field_update",
        StoreError::Invalid(_) => "invalid",
        StoreError::ClaimsUnavailable(_) => "claims_unavailable",
        StoreError::Sqlite(_) => "sqlite",
        StoreError::Io(_) => "io",
    }
}

pub(crate) fn handle_request(state: &StoreState, payload: &[u8]) -> Value {
    let req: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => return err_reply(0, "malformed_frame", format!("request is not JSON: {e}")),
    };
    let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let request_id = if is_write_method(method) {
        params
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    if let Some(request_id) = request_id.as_deref() {
        record_write_started(state, request_id);
    }
    let result = match method {
        "read_ids" => handle_read_ids(state, &params),
        "plan_refs" => handle_plan_refs(state),
        "write_status" => handle_write_status(state, &params),
        "export_now" => handle_export_now(state),
        "export_status" => handle_export_status(state),
        "set_backend" => handle_set_backend(state, &params),
        "backend_status" => handle_backend_status(state),
        "keeper_scan" => crate::store_exec::handle_keeper_scan(state),
        "op" => handle_op(state, &params),
        "api" => handle_api(state, &params),
        // The raw row publish behind Python's `commit_rows_via_store`: the
        // client mutated a snapshot it read from this store and ships the
        // changed rows plus removed ids; the store applies the diff under
        // the version check, so two writers converge on one serialized
        // order instead of last-write-wins on whole files.
        "commit_rows" => handle_commit_rows(state, &params),
        "read_file" => handle_read_file(state),
        "defaults" => handle_pure(&params, |mut entries, p| {
            graph_store::apply_defaults(
                &mut entries,
                p.get("keep_malformed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            entries
        }),
        "recompute" => handle_pure(&params, |mut entries, p| {
            let plan_rungs = plan_rung_map(p);
            graph_store::recompute_statuses_with_plan_rungs(&mut entries, plan_rungs.as_ref());
            entries
        }),
        // The dispatch admission decision (backlog_ready::select), served so
        // the Python callers are clients and no second selection leg exists.
        // With `entries` in the params the verb filters that list (the
        // external-tracker backend's joined candidates); without, it reads
        // the graph this keeper owns.
        "ready" => handle_ready(state, &params),
        // The read-time readiness overlay (statuses.compute_readiness), for
        // the client's pre-render pass: the write path's recompute does not
        // derive `blocked` -- it is a read overlay -- so a mutation that
        // newly blocks a sibling must overlay before rendering graph.md.
        "overlay" => handle_pure(&params, |mut entries, _p| {
            graph_store::apply_readiness_overlay(&mut entries);
            entries
        }),
        // The blocked_by edge settlement (the full-sweep mutator's write-side
        // twin of the overlay chase): client-shipped rows in, rewritten rows
        // plus one receipt per settled edge out. No file I/O, no publish -
        // the caller persists under the graph lock.
        "settle_edges" => handle_settle_edges(&params),
        "normalize_plan_path" => {
            let normalized = graph_store::normalize_plan_path(opt_str(&params, "path"));
            Ok(json!({ "path": normalized }))
        }
        // The canonical key order, for the ordering tests and one caller that
        // documents the on-disk shape: one source of truth (the ported
        // store's constant), never a re-typed copy.
        "canonical_field_order" => Ok(json!({ "fields": graph_store::CANONICAL_FIELD_ORDER })),
        // The one delivery classifier (scoreboard.rs): graph nodes + ledger
        // rows in, a per-node delivery classification out. Pure; the
        // scoreboard views are the callers, so seven views read one decision.
        "scoreboard_classify" => crate::scoreboard::classify(&params).map_err(StoreError::Invalid),
        // One named op applied over client-shipped rows, no file I/O and no
        // publish: `set_related`, `plan_path_owner_conflict`, and friends
        // run INSIDE a client mutator on an in-hand snapshot, where a full
        // locked cycle would be a write the caller never asked for.
        "pure_op" => handle_pure_op(&params),
        other => Err(StoreError::Invalid(format!(
            "unknown store method {other:?}"
        ))),
    };
    let reply = match result {
        Ok(v) => json!({"id": id, "ok": true, "result": v}),
        Err(e) => err_reply(id, store_err_kind(&e), e.to_string()),
    };
    if let Some(request_id) = request_id.as_deref() {
        record_write_done(state, request_id, &reply);
    }
    reply
}

fn is_write_method(method: &str) -> bool {
    matches!(method, "op" | "api" | "commit_rows")
}

fn prune_write_ledger(
    ledger: &mut std::collections::VecDeque<WriteLedgerEntry>,
    now: std::time::Instant,
) {
    ledger.retain(|entry| now.duration_since(entry.recorded_at) <= WRITE_LEDGER_TTL);
}

fn record_write_started(state: &StoreState, request_id: &str) {
    let now = std::time::Instant::now();
    let mut ledger = state
        .write_ledger
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    prune_write_ledger(&mut ledger, now);
    ledger.retain(|entry| entry.request_id != request_id);
    ledger.push_back(WriteLedgerEntry {
        request_id: request_id.to_owned(),
        recorded_at: now,
        state: WriteLedgerState::InFlight,
    });
    while ledger.len() > WRITE_LEDGER_CAPACITY {
        ledger.pop_front();
    }
}

fn elide_write_entries(reply: &Value) -> Value {
    let mut reply = reply.clone();
    let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut) else {
        return reply;
    };
    if result.get("entries").map(Value::is_array).unwrap_or(false) {
        result.insert("entries".to_owned(), Value::Null);
        result.insert("entries_elided".to_owned(), Value::Bool(true));
    }
    if let Some(outcome) = result.get_mut("outcome").and_then(Value::as_object_mut) {
        if outcome.get("entries").map(Value::is_array).unwrap_or(false) {
            outcome.insert("entries".to_owned(), Value::Null);
            outcome.insert("entries_elided".to_owned(), Value::Bool(true));
        }
    }
    reply
}

fn record_write_done(state: &StoreState, request_id: &str, reply: &Value) {
    let now = std::time::Instant::now();
    let mut ledger = state
        .write_ledger
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    prune_write_ledger(&mut ledger, now);
    if let Some(entry) = ledger
        .iter_mut()
        .find(|entry| entry.request_id == request_id)
    {
        entry.recorded_at = now;
        entry.state = WriteLedgerState::Done(elide_write_entries(reply));
    }
}

fn handle_write_status(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let request_id = params
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| StoreError::Invalid("write_status needs request_id".into()))?;
    let now = std::time::Instant::now();
    let mut ledger = state
        .write_ledger
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    prune_write_ledger(&mut ledger, now);
    let Some(entry) = ledger.iter().find(|entry| entry.request_id == request_id) else {
        return Ok(json!({"state": "unknown"}));
    };
    Ok(match &entry.state {
        WriteLedgerState::InFlight => json!({"state": "in_flight"}),
        WriteLedgerState::Done(reply) => json!({"state": "done", "reply": reply}),
    })
}

/// The dispatch admission decision over client-shipped rows or the graph
/// this keeper owns: `backlog_ready::select` in, survivors + drops out.
/// Params: `project`, `all`, `roadmap_id`, `parent`, `mission`,
/// `include_ideas`, `include_deferred`, `repo_root`, `entries` (optional -
/// the external-backend path), `claimed` (optional - live claim ids; when
/// absent the keeper resolves them from the claims store itself).
fn handle_ready(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    use crate::backlog_ready::{select, NoSuchParent, ReadyOpts};
    use std::collections::BTreeSet;

    let opt_str_owned = |k: &str| -> Option<String> {
        params
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let opts = ReadyOpts {
        project: opt_str_owned("project"),
        all: params.get("all").and_then(Value::as_bool).unwrap_or(false),
        roadmap_id: opt_str_owned("roadmap_id"),
        parent: opt_str_owned("parent"),
        mission: opt_str_owned("mission"),
        include_ideas: params
            .get("include_ideas")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        include_deferred: params
            .get("include_deferred")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        repo_root: opt_str_owned("repo_root"),
        claimed: match params.get("claimed") {
            Some(Value::Array(ids)) => ids
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<String>>(),
            // Unknown claim state must refuse, not read as "nothing is
            // claimed": the Python leg this verb replaced failed closed
            // (`live_claimed_node_ids(strict=True)`).
            _ => crate::claims::list(Some("node:"), None, false)
                .map_err(|e| {
                    StoreError::ClaimsUnavailable(format!(
                        "live claim state is unavailable; ready selection refused: {e}"
                    ))
                })?
                .iter()
                .filter_map(|rec| rec.key.strip_prefix("node:").map(str::to_string))
                .collect(),
        },
        // Explicit param first (a client that resolved policy), then the
        // config beside the graph, then the fail-open default in select().
        staleness_days: params
            .get("staleness_days")
            .and_then(Value::as_i64)
            .or_else(|| {
                crate::backlog_ready::configured_staleness_days(
                    &state.graph.parent().unwrap_or(Path::new("")),
                )
            }),
        now_ms: params
            .get("now_ms")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| crate::claims::now_ms()),
    };
    let sqlite;
    let entries: &[Value] = match params.get("entries").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            sqlite = read_state(state)?;
            &sqlite
        }
    };
    match select(entries, &opts) {
        Ok(reply) => Ok(json!({
            "rows": reply.rows,
            "drops": reply
                .drops
                .iter()
                .map(|d| json!({"id": d.id, "filter": d.filter, "reason": d.reason}))
                .collect::<Vec<_>>(),
        })),
        Err(NoSuchParent(parent)) => Err(StoreError::Invalid(format!("no such node '{parent}'"))),
    }
}

/// The by-id read: exact id-then-slug rows from the cache, in argument order,
/// with the readiness overlay applied server-side (the overlay derives
/// `blocked` from the blockers' rows, so it needs the whole list even when
/// the reply carries one). Unmatched tokens are reported, never guessed;
/// the client falls back to the full read on any miss it cannot use.
fn handle_read_ids(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let tokens: Vec<String> = match params.get("ids").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        None => return Err(StoreError::Invalid("read_ids needs ids".into())),
    };
    if tokens.is_empty() {
        return Err(StoreError::Invalid(
            "read_ids needs a non-empty ids list".into(),
        ));
    }
    let mut overlaid = read_state(state)?;
    // The id read is a live-graph seam: archived residents never answer it.
    overlaid.retain(|entry| entry.get("archived_at").map_or(true, Value::is_null));
    graph_store::apply_readiness_overlay(&mut overlaid);
    let mut out = Vec::with_capacity(tokens.len());
    let mut missing = Vec::new();
    for token in &tokens {
        match crate::graph_get::find_entry(&overlaid, token) {
            Some(entry) => out.push(entry.clone()),
            None => missing.push(token.clone()),
        }
    }
    crate::node_reading::attach_reading(&mut out);
    Ok(json!({"entries": out, "missing": missing}))
}

/// The plan-rung inputs: id plus the two fields the Python rung table
/// (`ladder.plan_rung`) reads on its side of the seam. The typed-op client
/// derives the rung map from this light read instead of a full begin, which
/// ships the whole graph for one derived value.
fn handle_plan_refs(state: &StoreState) -> Result<Value, StoreError> {
    let entries = std::sync::Arc::new(read_state(state)?);
    let refs: Vec<Value> = entries
        .iter()
        .filter(|e| graph_store::is_dict(e))
        .map(|e| {
            json!({
                "id": e.get("id"),
                "plan_path": e.get("plan_path"),
                "cwd": e.get("cwd"),
            })
        })
        .collect();
    Ok(json!({ "entries": refs }))
}

fn read_state(state: &StoreState) -> Result<Vec<Value>, StoreError> {
    crate::backlog::read_entries(&state.graph).map_err(|error| {
        StoreError::Unreadable(
            crate::backlog::database_path(&state.graph)
                .display()
                .to_string(),
            error,
        )
    })
}

fn state_version(state: &StoreState) -> Result<String, StoreError> {
    crate::backlog::version(&state.graph).map_err(|error| {
        StoreError::Unreadable(
            crate::backlog::database_path(&state.graph)
                .display()
                .to_string(),
            error,
        )
    })
}

/// Pure transforms over client-shipped rows: the migration seam and the
/// status cascade, for callers holding entries in memory (scoreboard fold,
/// drift checks). No file I/O, no publish.
fn handle_pure(
    params: &Value,
    f: impl FnOnce(Vec<Value>, &Value) -> Vec<Value>,
) -> Result<Value, StoreError> {
    let entries: Vec<Value> = params
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::Invalid("pure methods need entries".into()))?
        .clone();
    let out = f(entries, params);
    Ok(json!({ "entries": out }))
}

/// The blocked_by edge settlement over client-shipped rows: rewritten rows,
/// one receipt per settled edge, and the per-node change map
/// (graph_store::settle_blocked_by_edges).
fn handle_settle_edges(params: &Value) -> Result<Value, StoreError> {
    let entries: Vec<Value> = params
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::Invalid("settle_edges needs entries".into()))?
        .clone();
    let (entries, receipts, changes) = graph_store::settle_blocked_by_edges(entries);
    Ok(json!({
        "entries": entries,
        "receipts": receipts,
        "blocked_by": changes,
    }))
}

/// The store entries serialized in the graph-file format, plus their digest
/// (the version token): load_graph parses the bytes, and the keeper's
/// serialized publish guarantees the reads never observe a half-written file.
fn handle_read_file(state: &StoreState) -> Result<Value, StoreError> {
    let _gate = state.gate.read().unwrap_or_else(|e| e.into_inner());
    let bytes = graph_store::serialize_graph_file(&read_state(state)?).into_bytes();
    Ok(json!({
        "bytes_b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
        "sha256": state_version(state)?,
    }))
}

fn handle_export_now(state: &StoreState) -> Result<Value, StoreError> {
    let _gate = state
        .gate
        .write()
        .unwrap_or_else(|error| error.into_inner());
    let version = crate::backlog::export_now(&state.graph).map_err(StoreError::Sqlite)?;
    Ok(json!({
        "version": version,
        "path": state.graph.display().to_string(),
    }))
}

fn handle_export_status(state: &StoreState) -> Result<Value, StoreError> {
    let (current, exported) =
        crate::backlog::export_status(&state.graph).map_err(StoreError::Sqlite)?;
    Ok(json!({
        "backend": crate::backlog::BACKEND_NAME,
        "stale": exported.as_deref() != Some(current.as_str()),
        "version": current,
        "exported_version": exported,
    }))
}

/// The stamp verb's write side: the json backend is deleted, so "sqlite" is
/// the only name it accepts (the refusal self-teaches that) and the call
/// just stamps `graph_meta.backend` (and `backend_since_ms` on the first
/// run) under the shared gate, so the stamp cannot interleave with a
/// mutation. Idempotent by design: a re-run keeps the original since stamp.
fn handle_set_backend(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let name = params
        .get("backend")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("set_backend needs a backend name".into()))?;
    if name != crate::backlog::BACKEND_NAME {
        return Err(StoreError::Invalid(format!(
            "unknown backend {name:?}; the json backend is deleted and {:?} is the only store",
            crate::backlog::BACKEND_NAME
        )));
    }
    let _gate = state
        .gate
        .write()
        .unwrap_or_else(|error| error.into_inner());
    let (previous, since) =
        crate::backlog::set_backend(&state.graph).map_err(StoreError::Sqlite)?;
    Ok(json!({
        "backend": crate::backlog::BACKEND_NAME,
        "previous": previous.unwrap_or_else(|| crate::backlog::BACKEND_NAME.to_string()),
        "since_ms": since.map(|v| v.to_string()),
    }))
}

/// `--status`'s read side: the store name plus when the stamp landed.
fn handle_backend_status(state: &StoreState) -> Result<Value, StoreError> {
    Ok(json!({
        "backend": crate::backlog::BACKEND_NAME,
        "since_ms": crate::backlog::backend_since(&state.graph)
            .map_err(StoreError::Sqlite)?
            .map(|v| v.to_string()),
    }))
}

/// The client-supplied node id -> plan rung map (see
/// `graph_store::supplied_plan_rung`): repo law keeps plan-document reading
/// on the Python side, so the map crosses as data. Absent key = the caller
/// is not re-deriving from plans, and stored statuses stay.
fn plan_rung_map(params: &Value) -> Option<std::collections::BTreeMap<String, String>> {
    plan_rung_map_field(params, "plan_rungs")
}

fn plan_rung_map_field(
    params: &Value,
    field: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let obj = params.get(field)?.as_object()?;
    Some(
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
    )
}

fn outcome_json(outcome: &graph_store::MutateOutcome) -> Value {
    json!({
        "entries": outcome.entries,
        "dropped": outcome.dropped,
        "closure_releases": outcome
            .closure_releases
            .iter()
            .map(|(id, rung)| json!({"id": id, "rung": rung}))
            .collect::<Vec<_>>(),
        "is_canonical": outcome.is_canonical,
    })
}

// ---------------------------------------------------------------------------
// Typed operations
// ---------------------------------------------------------------------------

fn find_exact<'a>(entries: &'a [Value], node_id: &str) -> Option<usize> {
    entries
        .iter()
        .position(|e| graph_store::entry_id(e) == Some(node_id))
}

/// Apply one typed operation to a defaulted entry list. Each op mirrors the
/// corresponding store.py helper; the keeper wraps it in the same locked
/// mutate cycle the commit path uses.
fn apply_op(entries: &mut Vec<Value>, name: &str, p: &Value) -> Result<Value, StoreError> {
    apply_op_impl(entries, name, p)
}

/// Test bridge: keeper unit tests drive the same op dispatch the wire
/// serves, in-process, without a socket in the loop.
pub fn apply_op_for_tests(entries: &mut Vec<Value>, request: &Value) -> Result<Value, StoreError> {
    let name = request
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("op needs a name".into()))?;
    let p = request.get("params").cloned().unwrap_or(Value::Null);
    apply_op_impl(entries, name, &p)
}

fn apply_op_impl(entries: &mut Vec<Value>, name: &str, p: &Value) -> Result<Value, StoreError> {
    match name {
        "update_fields" => {
            let node_id = param_str(p, "node_id")?;
            let fields = p
                .get("fields")
                .and_then(Value::as_object)
                .ok_or_else(|| StoreError::Invalid("update_fields needs fields".into()))?;
            let idx = find_exact(entries, node_id)
                .ok_or_else(|| StoreError::Invalid(format!("no node resolves to '{node_id}'")))?;
            let mut applied = 0usize;
            for (field, spec) in fields {
                let update = FieldUpdate::from_value(field, spec)?;
                if matches!(update, FieldUpdate::Keep) {
                    continue;
                }
                let obj = entries[idx].as_object_mut().unwrap();
                graph_store::apply_field_update(obj, field, &update);
                applied += 1;
            }
            Ok(json!({"applied": applied}))
        }
        "append_progress_note" => {
            let node_id = param_str(p, "node_id")?;
            let note = p.get("note").cloned().unwrap_or(Value::Null);
            let mut found = false;
            let mut plan_path = Value::Null;
            if let Some(idx) = find_exact(entries, node_id) {
                let obj = entries[idx].as_object_mut().unwrap();
                let notes = obj
                    .entry("progress_notes".to_string())
                    .or_insert_with(|| Value::Array(vec![]));
                if !notes.is_array() {
                    *notes = Value::Array(vec![]);
                }
                notes.as_array_mut().unwrap().push(note.clone());
                plan_path = obj.get("plan_path").cloned().unwrap_or(Value::Null);
                found = true;
            }
            Ok(json!({"found": found, "plan_path": plan_path}))
        }
        "append_encounter" => {
            let node_id = param_str(p, "node_id")?;
            let record = p
                .get("record")
                .cloned()
                .ok_or_else(|| StoreError::Invalid("append_encounter needs a record".into()))?;
            // demand.voter_key: the record's own voter_key, else its
            // session_id - never a params-level field.
            let key = record
                .get("voter_key")
                .and_then(Value::as_str)
                .or_else(|| record.get("session_id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if key.is_empty() {
                return Ok(json!({"appended": false,
                    "error": "an encounter with no voter key (session_id) is not readable back to a transcript",
                    "reason": "unidentified"}));
            }
            let Some(idx) = find_exact(entries, node_id) else {
                return Ok(json!({"appended": false,
                    "error": format!("no node resolves to '{node_id}'"), "reason": "missing"}));
            };
            let obj = entries[idx].as_object_mut().unwrap();
            let existing = obj
                .get("encounters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for prior in &existing {
                let prior_key = prior
                    .get("voter_key")
                    .and_then(Value::as_str)
                    .or_else(|| prior.get("session_id").and_then(Value::as_str))
                    .unwrap_or_default();
                if prior_key == key {
                    return Ok(json!({"appended": false,
                        "error": format!(
                            "voter {key} already recorded an encounter on {} at {}",
                            obj.get("id").and_then(Value::as_str).unwrap_or(node_id),
                            // Python's f-string prints the None of a missing
                            // ts as "None"; byte parity keeps that spelling.
                            prior.get("ts").and_then(Value::as_str).unwrap_or("None")
                        ),
                        "reason": "duplicate"}));
                }
            }
            let encounters = obj
                .entry("encounters".to_string())
                .or_insert_with(|| Value::Array(vec![]));
            if !encounters.is_array() {
                *encounters = Value::Array(vec![]);
            }
            encounters.as_array_mut().unwrap().push(record);
            Ok(json!({"appended": true, "error": Value::Null, "reason": Value::Null}))
        }
        "append_wave_note" => {
            let node_id = param_str(p, "node_id")?;
            let note = p
                .get("note")
                .cloned()
                .ok_or_else(|| StoreError::Invalid("append_wave_note needs a note".into()))?;
            let Some(idx) = find_exact(entries, node_id) else {
                return Ok(json!({"found": false,
                    "error": format!("no node resolves to '{node_id}'")}));
            };
            let obj = entries[idx].as_object_mut().unwrap();
            let terminal = obj
                .get("completed_at")
                .map(|v| !v.is_null())
                .unwrap_or(false)
                || matches!(
                    obj.get("status").and_then(Value::as_str),
                    Some("done") | Some("superseded")
                );
            if terminal {
                return Ok(json!({"found": false,
                    "error": format!("wave target '{node_id}' is terminal")}));
            }
            let notes = obj
                .entry("progress_notes".to_string())
                .or_insert_with(|| Value::Array(vec![]));
            if !notes.is_array() {
                *notes = Value::Array(vec![]);
            }
            notes.as_array_mut().unwrap().push(note);
            Ok(json!({"found": true, "error": Value::Null}))
        }
        "session_append" => {
            let node_id = param_str(p, "node_id")?;
            let phase = param_str(p, "phase")?;
            let harness = param_str(p, "harness")?;
            let session_id = param_str(p, "session_id")?;
            let effort = opt_str(p, "effort");
            let started_at = opt_str(p, "started_at");
            let ended_at = opt_str(p, "ended_at");
            let observed = p.get("observed").cloned();
            let merge_grant = p.get("merge_grant").filter(|v| !v.is_null());
            let row = session_row(
                phase,
                harness,
                session_id,
                effort,
                started_at,
                ended_at,
                observed,
                merge_grant,
            )?;
            let (found, added) = session_append(entries, node_id, row)?;
            Ok(json!({"found": found, "added": added}))
        }
        "session_remove_open" => {
            let node_id = param_str(p, "node_id")?;
            let phase = param_str(p, "phase")?;
            let harness = param_str(p, "harness")?;
            let session_id = param_str(p, "session_id")?;
            let started_at = param_str(p, "started_at")?;
            let (found, removed) =
                session_remove_open(entries, node_id, phase, harness, session_id, started_at)?;
            Ok(json!({"found": found, "removed": removed}))
        }
        "session_reap_open" => {
            let node_id = opt_str(p, "node_id");
            let phase = param_str(p, "phase")?;
            let harness = param_str(p, "harness")?;
            let session_id = param_str(p, "session_id")?;
            let ended_at = opt_str(p, "ended_at");
            let report = session_reap_open(entries, node_id, phase, harness, session_id, ended_at)?;
            Ok(report)
        }
        "set_related" => {
            let node_id = param_str(p, "node_id")?;
            let desired: Vec<String> = p
                .get("desired")
                .and_then(Value::as_array)
                .ok_or_else(|| StoreError::Invalid("set_related needs desired".into()))?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            set_related(entries, node_id, &desired)?;
            Ok(json!({"ok": true}))
        }
        "defer" => {
            let node_id = param_str(p, "node_id")?;
            let reason = param_str(p, "reason")?;
            let kind = opt_str(p, "kind")
                .map(str::to_string)
                .or_else(|| classify_deferred_reason(reason).map(str::to_string));
            // The shared defer leg: the blank-reason refusal and the
            // kind vocabulary live in the patch planner, so the mux op and
            // the CLI door cannot disagree.
            crate::backlog::patch::defer_facts(entries, node_id, reason, kind.as_deref())?;
            Ok(json!({"deferred": true}))
        }
        "end_mission" => {
            let node_id = param_str(p, "node_id")?;
            let idx = find_exact(entries, node_id)
                .ok_or_else(|| StoreError::Invalid(format!("no node resolves to '{node_id}'")))?;
            entries[idx]
                .as_object_mut()
                .unwrap()
                .shift_remove("mission_active");
            Ok(json!({"mission_active": false}))
        }
        "find_for_pr" => {
            let pr_number = p
                .get("pr_number")
                .and_then(Value::as_i64)
                .ok_or_else(|| StoreError::Invalid("find_for_pr needs pr_number".into()))?;
            let repo = opt_str(p, "repo");
            let ids: Vec<String> = entries
                .iter()
                .filter(|e| graph_store::is_dict(e))
                .filter(|e| node_carries_pr(e, pr_number as i64, repo))
                .filter_map(|e| graph_store::entry_id(e).map(str::to_string))
                .collect();
            Ok(json!({"ids": ids}))
        }
        "plan_path_owner_conflict" => {
            let node_id = opt_str(p, "node_id");
            let plan_path = opt_str(p, "plan_path");
            Ok(json!({
                "owner": graph_store::plan_path_owner_conflict(entries, node_id, plan_path),
            }))
        }
        other => Err(StoreError::Invalid(format!("unknown op {other:?}"))),
    }
}

/// One named op applied over client-shipped rows, no file I/O and no
/// publish: `set_related`, `plan_path_owner_conflict`, and friends run
/// INSIDE a client mutator on an in-hand snapshot, where a full locked
/// cycle would be a write the caller never asked for.
fn handle_pure_op(params: &Value) -> Result<Value, StoreError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("pure_op needs a name".into()))?;
    let p = params.get("params").cloned().unwrap_or(Value::Null);
    let mut entries: Vec<Value> = params
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::Invalid("pure_op needs entries".into()))?
        .clone();
    let op_result = match name {
        "canonicalize" => {
            graph_store::canonicalize_entries(&mut entries);
            json!({"ok": true})
        }
        other => apply_op(&mut entries, other, &p)?,
    };
    Ok(json!({ "entries": entries, "op": op_result }))
}

fn param_str<'a>(p: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    p.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid(format!("{key} must be a string")))
}

fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

fn classify_deferred_reason(reason: &str) -> Option<&'static str> {
    crate::backlog::patch::classify_deferred_reason(reason)
}

pub(crate) fn node_carries_pr(node: &Value, pr_number: i64, repo: Option<&str>) -> bool {
    let primary = node.get("pr_number").and_then(Value::as_i64) == Some(pr_number);
    let urls: Vec<String> = {
        let mut v = Vec::new();
        if let Some(u) = node.get("pr_url").and_then(Value::as_str) {
            v.push(u.to_string());
        }
        if let Some(extras) = node.get("additional_prs").and_then(Value::as_array) {
            for e in extras {
                if let Some(u) = e.get("url").and_then(Value::as_str) {
                    v.push(u.to_string());
                }
            }
        }
        v
    };
    let carries = primary
        || node
            .get("additional_prs")
            .and_then(Value::as_array)
            .map(|extras| {
                extras
                    .iter()
                    .any(|e| e.get("number").and_then(Value::as_i64) == Some(pr_number))
            })
            .unwrap_or(false);
    if !carries {
        return false;
    }
    match repo {
        None => {
            primary
                || node
                    .get("additional_prs")
                    .and_then(Value::as_array)
                    .map(|extras| {
                        extras
                            .iter()
                            .any(|e| e.get("number").and_then(Value::as_i64) == Some(pr_number))
                    })
                    .unwrap_or(false)
        }
        Some(want) => {
            let want = want.to_lowercase();
            urls.iter().any(|url| {
                let clean = url.split('?').next().unwrap_or(url);
                let clean = clean.split('#').next().unwrap_or(clean);
                let clean = clean.trim_end_matches('/');
                let Some((head, tail)) = clean.rsplit_once("/pull/") else {
                    return false;
                };
                tail.parse::<i64>()
                    .map(|n| n == pr_number && head.to_lowercase().ends_with(&format!("/{want}")))
                    .unwrap_or(false)
            })
        }
    }
}

/// Build one session row, validating identity/timestamps under the same
/// contract as store.append_session_record.
fn session_row(
    phase: &str,
    harness: &str,
    session_id: &str,
    effort: Option<&str>,
    started_at: Option<&str>,
    ended_at: Option<&str>,
    observed: Option<Value>,
    merge_grant: Option<&Value>,
) -> Result<Value, StoreError> {
    const SESSION_PHASES: &[&str] = &["think", "blueprint", "do", "review", "ship"];
    const STR_MAX: usize = 200;
    if !SESSION_PHASES.contains(&phase) {
        return Err(StoreError::Invalid(format!(
            "invalid phase {phase:?}; expected one of {SESSION_PHASES:?}"
        )));
    }
    let harness = harness.trim();
    let session_id = session_id.trim();
    for (label, value) in [("harness", harness), ("session_id", session_id)] {
        if value.is_empty() {
            return Err(StoreError::Invalid(format!(
                "{label} must be a non-empty string"
            )));
        }
        if value.len() > STR_MAX {
            return Err(StoreError::Invalid(format!(
                "{label} exceeds {STR_MAX} chars"
            )));
        }
    }
    // An id whose shape names one of the shape-known harnesses refuses a
    // stamp naming another: the wrong-harness stamp is how phantom twin rows
    // get minted (a codex v7 id under `harness: claude` reads as a second,
    // distinct session to every keyed resolver).
    if let Some(shape) = harness_of_session_id(session_id) {
        if harness != shape && shape_known_harness(harness) {
            return Err(StoreError::Invalid(format!(
                "session_id {session_id} is a {shape} id; refusing harness {harness}"
            )));
        }
    }
    let effort = match effort {
        Some(e) => {
            let e = e.trim();
            if e.is_empty() {
                return Err(StoreError::Invalid(
                    "effort must be a non-empty string when provided".into(),
                ));
            }
            if e.len() > STR_MAX {
                return Err(StoreError::Invalid(format!(
                    "effort exceeds {STR_MAX} chars"
                )));
            }
            Some(e.to_string())
        }
        None => None,
    };
    let stamp = |label: &str, v: &str| -> Result<String, StoreError> {
        let parsed = chrono::DateTime::parse_from_rfc3339(&v.trim().replace('Z', "+00:00"))
            .map_err(|_| {
                StoreError::Invalid(format!("{label} must be an ISO-8601 timestamp, got {v:?}"))
            })?;
        if parsed.offset().local_minus_utc() != 0 {
            return Err(StoreError::Invalid(format!(
                "{label} must be a UTC timestamp (offset +00:00 / Z), got {v:?}"
            )));
        }
        Ok(parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    };
    let started_at = started_at.map(|s| stamp("started_at", s)).transpose()?;
    let ended_at = ended_at.map(|s| stamp("ended_at", s)).transpose()?;
    // The spawner-resolved merge posture on a do row. The client validates the
    // shape for its ValueError contract; the keeper re-validates before the
    // row can carry it, so no raw caller can store a guessed grant.
    let grant = match merge_grant {
        None => None,
        Some(g) => {
            const GRANT_KEYS: &[&str] = &["approved", "source", "recorded_by", "recorded_at"];
            let obj = g.as_object().ok_or_else(|| {
                StoreError::Invalid("merge_grant must be a mapping when provided".into())
            })?;
            let unknown: Vec<String> = obj
                .keys()
                .filter(|k| !GRANT_KEYS.contains(&k.as_str()))
                .cloned()
                .collect();
            if !unknown.is_empty() {
                return Err(StoreError::Invalid(format!(
                    "merge_grant carries unknown keys: {unknown:?}"
                )));
            }
            let approved = obj
                .get("approved")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    StoreError::Invalid("merge_grant.approved must be a boolean".into())
                })?;
            let text = |key: &str| -> Result<String, StoreError> {
                let v = obj.get(key).and_then(Value::as_str).unwrap_or("").trim();
                if v.is_empty() {
                    return Err(StoreError::Invalid(format!(
                        "merge_grant.{key} must be a non-empty string"
                    )));
                }
                if v.len() > STR_MAX {
                    return Err(StoreError::Invalid(format!(
                        "merge_grant.{key} exceeds {STR_MAX} chars"
                    )));
                }
                Ok(v.to_string())
            };
            let source = text("source")?;
            let recorded_by = text("recorded_by")?;
            let raw_at = obj.get("recorded_at").and_then(Value::as_str).unwrap_or("");
            if raw_at.trim().is_empty() {
                return Err(StoreError::Invalid(
                    "merge_grant.recorded_at must be a non-empty string".into(),
                ));
            }
            let recorded_at = stamp("merge_grant.recorded_at", raw_at)?;
            let mut grant = Map::new();
            grant.insert("approved".into(), Value::Bool(approved));
            grant.insert("source".into(), Value::String(source));
            grant.insert("recorded_by".into(), Value::String(recorded_by));
            grant.insert("recorded_at".into(), Value::String(recorded_at));
            Some(Value::Object(grant))
        }
    };
    let mut row = Map::new();
    row.insert("phase".into(), Value::String(phase.to_string()));
    row.insert("harness".into(), Value::String(harness.to_string()));
    row.insert("session_id".into(), Value::String(session_id.to_string()));
    if let Some(e) = effort {
        row.insert("effort".into(), Value::String(e));
    }
    if let Some(s) = started_at {
        row.insert("started_at".into(), Value::String(s));
    }
    if let Some(e) = ended_at {
        row.insert("ended_at".into(), Value::String(e));
    }
    // Written unconditionally, including the unknown kinds: an ABSENT key
    // means the writer never looked; a present one is what the writer saw.
    row.insert("observed_model".into(), observed.unwrap_or(Value::Null));
    if let Some(g) = grant {
        row.insert("merge_grant".into(), g);
    }
    Ok(Value::Object(row))
}

/// The append half of store.append_session_record: idempotent on
/// (session_id, phase); a duplicate fills only timestamps it left open, and
/// observed_model is the one field the LATEST stamp owns. The harness is not
/// part of the key: one session on one phase is one row, whatever harness
/// spelling a writer carried (the shape check above already refuses a
/// provably wrong one).
fn session_append(
    entries: &mut Vec<Value>,
    node_id: &str,
    row: Value,
) -> Result<(bool, bool), StoreError> {
    let Some(idx) = find_exact(entries, node_id) else {
        return Ok((false, false));
    };
    let phase = row
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_id = row
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let obj = entries[idx].as_object_mut().unwrap();
    let sessions = obj
        .entry("sessions".to_string())
        .or_insert_with(|| Value::Array(vec![]));
    if !sessions.is_array() {
        *sessions = Value::Array(vec![]);
    }
    let rows = sessions.as_array_mut().unwrap();
    let prior = rows.iter_mut().find(|r| {
        r.get("phase").and_then(Value::as_str) == Some(phase.as_str())
            && r.get("session_id").and_then(Value::as_str) == Some(session_id.as_str())
    });
    if let Some(prior) = prior {
        // merge_grant joins the fill-if-absent set: the first resolved posture
        // owns the row, and a re-stamp cannot rewrite a recorded refusal into
        // a grant in place.
        for key in ["ended_at", "started_at", "effort", "merge_grant"] {
            if let Some(v) = row.get(key) {
                if !v.is_null() && !prior.as_object().unwrap().contains_key(key) {
                    prior
                        .as_object_mut()
                        .unwrap()
                        .insert(key.to_string(), v.clone());
                }
            }
        }
        let merged = merge_observed_model(prior.get("observed_model"), row.get("observed_model"));
        if let Some(merged) = merged {
            prior
                .as_object_mut()
                .unwrap()
                .insert("observed_model".to_string(), merged);
        }
        return Ok((true, false));
    }
    rows.push(row);
    Ok((true, true))
}

/// The value a re-stamp should write to observed_model, or None to keep
/// (store._merge_observed_model): a later real observation wins; a recorded
/// disagreement stays observed-multiple; an unknown never displaces a
/// recording but upgrades an absent/unknown prior.
fn merge_observed_model(prior: Option<&Value>, fresh: Option<&Value>) -> Option<Value> {
    let Some(fresh) = fresh else {
        return None;
    };
    if fresh.is_null() {
        return None;
    }
    let Some(prior) = prior else {
        return Some(fresh.clone());
    };
    if prior.is_null() {
        return Some(fresh.clone());
    }
    let fresh_kind = fresh.get("kind").and_then(Value::as_str).unwrap_or("");
    if fresh_kind != "observed" {
        return None;
    }
    let prior_kind = prior.get("kind").and_then(Value::as_str).unwrap_or("");
    if prior_kind != "observed" && prior_kind != "observed-multiple" {
        return Some(fresh.clone());
    }
    let mut seen: Vec<Value> = prior
        .get("prior_models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prior_model = prior.get("model").cloned().unwrap_or(Value::Null);
    let fresh_model = fresh.get("model").cloned().unwrap_or(Value::Null);
    let mut out = fresh.as_object().cloned().unwrap_or_default();
    if prior_model == fresh_model {
        out.insert("kind".into(), Value::String(prior_kind.to_string()));
        if !seen.is_empty() {
            out.insert("prior_models".into(), Value::Array(seen));
        } else {
            out.shift_remove("prior_models");
        }
        return Some(Value::Object(out));
    }
    if !prior_model.is_null() && !seen.contains(&prior_model) {
        seen.push(prior_model);
    }
    out.insert("kind".into(), Value::String("observed-multiple".into()));
    out.insert("prior_models".into(), Value::Array(seen));
    Some(Value::Object(out))
}

/// store.remove_open_session_record: the one compensating write against the
/// append-only sessions list, gated on all four preconditions.
fn session_remove_open(
    entries: &mut Vec<Value>,
    node_id: &str,
    phase: &str,
    harness: &str,
    session_id: &str,
    started_at: &str,
) -> Result<(bool, bool), StoreError> {
    let Some(idx) = find_exact(entries, node_id) else {
        return Ok((false, false));
    };
    let started_norm = stamp_utc(started_at)?;
    let obj = entries[idx].as_object_mut().unwrap();
    let rows = obj
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let keep: Vec<Value> = rows
        .iter()
        .filter(|r| {
            !((r.get("phase").and_then(Value::as_str) == Some(phase)
                && r.get("harness").and_then(Value::as_str) == Some(harness)
                && r.get("session_id").and_then(Value::as_str) == Some(session_id))
                && !r
                    .as_object()
                    .map(|o| o.contains_key("ended_at"))
                    .unwrap_or(false)
                && r.get("started_at").and_then(Value::as_str) == Some(started_norm.as_str()))
        })
        .cloned()
        .collect();
    let removed = keep.len() != rows.len();
    if removed {
        obj.insert("sessions".to_string(), Value::Array(keep));
    }
    Ok((true, removed))
}

fn stamp_utc(v: &str) -> Result<String, StoreError> {
    let parsed =
        chrono::DateTime::parse_from_rfc3339(&v.trim().replace('Z', "+00:00")).map_err(|_| {
            StoreError::Invalid(format!(
                "started_at must be an ISO-8601 timestamp, got {v:?}"
            ))
        })?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(StoreError::Invalid(format!(
            "started_at must be a UTC timestamp (offset +00:00 / Z), got {v:?}"
        )));
    }
    Ok(parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// store.reap_open_session_record: close open rows with positive death
/// evidence. Every phase, `do` included, FILLS ended_at and keeps the row:
/// a filled row is not open, so it un-wedges node status exactly as a
/// removal did, and the session provenance survives. `all` applies the fill
/// to every open row carrying the identity.
///
/// With a node id the op answers about that one entry exactly as before.
/// Without one (the death-cascade form), it walks every entry and applies
/// the same semantics to each open row carrying the identity, so one call
/// settles a session that worked several nodes. `found` means "the named
/// node exists" on the exact form and "at least one node matched" on the
/// identity form; `node_ids` names every node the write touched.
fn session_reap_open(
    entries: &mut Vec<Value>,
    node_id: Option<&str>,
    phase: &str,
    harness: &str,
    session_id: &str,
    ended_at: Option<&str>,
) -> Result<Value, StoreError> {
    const SESSION_PHASES: &[&str] = &["think", "blueprint", "do", "review", "ship"];
    if phase != "all" && !SESSION_PHASES.contains(&phase) {
        return Err(StoreError::Invalid(format!(
            "invalid phase {phase:?}; expected 'all' or one of {SESSION_PHASES:?}"
        )));
    }
    let close_phases: Vec<&str> = if phase == "all" {
        SESSION_PHASES.to_vec()
    } else {
        vec![phase]
    };
    if harness.trim().is_empty() || session_id.trim().is_empty() {
        return Err(StoreError::Invalid(
            "identity must be non-empty strings".into(),
        ));
    }
    let ended = match ended_at {
        Some(v) => stamp_utc(v)?,
        None => chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    // The crate's one openness predicate (graph_store::is_open_phase_row,
    // mirroring the Python authority) applied to one entry's rows; both
    // forms share it so they cannot drift.
    let reap_rows = |rows: &mut Vec<Value>| -> bool {
        let mut row_closed = false;
        for cp in &close_phases {
            for r in rows.iter_mut() {
                if graph_store::is_open_phase_row(r, cp)
                    && r.get("harness").and_then(Value::as_str) == Some(harness)
                    && r.get("session_id").and_then(Value::as_str) == Some(session_id)
                {
                    r.as_object_mut()
                        .unwrap()
                        .entry("ended_at".to_string())
                        .or_insert_with(|| Value::String(ended.clone()));
                    row_closed = true;
                }
            }
        }
        row_closed
    };
    match node_id {
        Some(node_id) => {
            let Some(idx) = find_exact(entries, node_id) else {
                return Ok(json!({
                    "found": false, "settled": false, "row_removed": false, "row_closed": false,
                    "status_before": null, "status_after": null, "remaining_open_do": 0,
                    "node_ids": [],
                }));
            };
            let status_before = entries[idx]
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string);
            let obj = entries[idx].as_object_mut().unwrap();
            let mut rows = obj
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let row_closed = reap_rows(&mut rows);
            if row_closed {
                obj.insert("sessions".to_string(), Value::Array(rows));
            }
            Ok(json!({
                "found": true,
                "settled": true,
                "row_removed": false,
                "row_closed": row_closed,
                "status_before": status_before,
                "status_after": Value::Null,
                "remaining_open_do": Value::Null,
                "node_ids": [node_id],
            }))
        }
        None => {
            let mut node_ids: Vec<String> = Vec::new();
            let mut row_closed = false;
            for idx in 0..entries.len() {
                if !entries[idx]
                    .get("sessions")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                {
                    continue;
                }
                let node = graph_store::entry_id(&entries[idx]).map(str::to_string);
                let obj = entries[idx].as_object_mut().unwrap();
                let mut rows = obj
                    .get("sessions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let closed = reap_rows(&mut rows);
                if closed {
                    if let Some(node) = node {
                        node_ids.push(node);
                    }
                    obj.insert("sessions".to_string(), Value::Array(rows));
                }
                row_closed |= closed;
            }
            Ok(json!({
                "found": !node_ids.is_empty(),
                "settled": true,
                "row_removed": false,
                "row_closed": row_closed,
                "status_before": Value::Null,
                "status_after": Value::Null,
                "remaining_open_do": Value::Null,
                "node_ids": node_ids,
            }))
        }
    }
}

/// store.set_related + _mirror_related: symmetric edges stored on both
/// endpoints; a missing peer in `added` is a programming error and fails
/// loudly rather than writing a dangling half-edge.
fn set_related(entries: &mut [Value], node_id: &str, desired: &[String]) -> Result<(), StoreError> {
    let idx = find_exact(entries, node_id)
        .ok_or_else(|| StoreError::Invalid(format!("no node resolves to '{node_id}'")))?;
    let before: std::collections::HashSet<String> = entries[idx]
        .get("related")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let after: std::collections::HashSet<String> = desired.iter().cloned().collect();
    let mut sorted: Vec<&String> = after.iter().collect();
    sorted.sort();
    entries[idx].as_object_mut().unwrap().insert(
        "related".to_string(),
        Value::Array(
            sorted
                .into_iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    for peer_id in after.difference(&before) {
        let Some(pidx) = find_exact(entries, peer_id) else {
            return Err(StoreError::Invalid(format!(
                "related peer '{peer_id}' is absent from the graph; refusing a dangling half-edge"
            )));
        };
        let obj = entries[pidx].as_object_mut().unwrap();
        let mut rel: std::collections::BTreeSet<String> = obj
            .get("related")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        rel.insert(node_id.to_string());
        obj.insert(
            "related".to_string(),
            Value::Array(rel.into_iter().map(Value::String).collect()),
        );
    }
    for peer_id in before.difference(&after) {
        let Some(pidx) = find_exact(entries, peer_id) else {
            continue; // the edge is already gone on that side
        };
        let obj = entries[pidx].as_object_mut().unwrap();
        let rel: std::collections::BTreeSet<String> = obj
            .get("related")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let without: std::collections::BTreeSet<String> = rel
            .difference(&std::iter::once(node_id.to_string()).collect())
            .cloned()
            .collect();
        obj.insert(
            "related".to_string(),
            Value::Array(without.into_iter().map(Value::String).collect()),
        );
    }
    Ok(())
}

/// Run one typed op through the full locked cycle: snapshot, apply, publish.
/// The gate's write guard excludes this against every other keeper-side
/// cycle, and
/// the base-version check still guards against a FOREIGN writer (an old
/// Python leg, a hand edit) that touched the file after the read.
///
/// A caller that computed its op input from an earlier snapshot (rank_top
/// reads peer ranks with `begin`, then writes) passes `base_version` with
/// that snapshot's digest: the keeper refuses with `conflict` when the file
/// moved between the caller's read and this cycle, so the computation is
/// provably fresh instead of hopefully fresh. The caller retries.
/// The raw row publish behind Python's `commit_rows_via_store`: the client
/// read a snapshot (its `sha256` IS the store version), mutated it, and
/// ships the changed rows plus the removed ids. A mismatched `base_version`
/// answers Conflict so the caller re-reads and re-diffs; the reply carries
/// the post-write rows plus the shape `_finish_mutation` consumes. The
/// write ledger records the request by id like any other write, so a
/// crashed client can ask `write_status` what landed.
fn handle_commit_rows(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let base = params.get("base_version").and_then(Value::as_str);
    let changed: Vec<Value> = params
        .get("changed")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let removed: Vec<String> = params
        .get("removed")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let _gate = state.gate.write().unwrap_or_else(|e| e.into_inner());
    if let Some(expected) = base {
        if state_version(state)? != expected {
            return Err(StoreError::Conflict);
        }
    }
    let entries = crate::backlog::apply_client_rows(&state.graph, "commit_rows", changed, removed)
        .map_err(StoreError::Invalid)?;
    Ok(json!({
        "dropped": 0,
        "backup": Value::Null,
        "entries": entries,
    }))
}

fn handle_op(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("op needs a name".into()))?;
    let p = params.get("params").cloned().unwrap_or(Value::Null);
    let client_base = params.get("base_version").and_then(Value::as_str);
    let _gate = state.gate.write().unwrap_or_else(|e| e.into_inner());
    // A foreign writer that publishes between this op's
    // gated read and the flock check surfaces as Conflict; retry the
    // read-apply-publish cycle while the gate is held, so the op lands
    // instead of replying kind conflict (the measured session-close
    // traceback). The client-supplied base stays terminal: the CALLER's
    // snapshot is what it names, retrying cannot refresh it.
    for attempt in 0..5u8 {
        let base = state_version(state)?;
        if let Some(expected) = client_base {
            if base != expected {
                return Err(StoreError::Conflict);
            }
        }
        let mut entries = read_state(state)?;
        let op_result = apply_op(&mut entries, name, &p)?;
        let outcome = match graph_store::locked_mutate(
            &state.graph,
            MutateInput {
                entries,
                canonical_path: state.canonical.then(|| state.graph.clone()),
                base_version: base,
                // The Python client sends the begin snapshot's map with every op
                // (a session op that opens or closes a do row re-derives
                // in_progress like any full write); a caller that sends none
                // keeps stored statuses.
                plan_rungs: plan_rung_map(&p),
            },
            state.lock_timeout,
        ) {
            Ok(outcome) => outcome,
            // Conflict only: a LockTimeout is a wedged or genuinely busy
            // lock, and retrying it inside one op would multiply the caller's
            // deadline (the wedged-writer test bounds it at 12s).
            Err(StoreError::Conflict) if attempt + 1 < 5 => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(err) => return Err(err),
        };
        return Ok(json!({
            "op": op_result,
            "outcome": outcome_json(&outcome),
        }));
    }
    unreachable!("every loop arm returns")
}

/// The typed backlog API over the wire: one keeper op per
/// `backlog::api` function, same name, same JSON fields. Queries hold the
/// read gate; mutations hold the write gate, which serializes every write
/// on this keeper (the store's own file lock serializes across processes).
fn handle_api(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("api needs an op".into()))?;
    let store = crate::backlog::api::Store::new(&state.graph);
    const READ_OPS: &[&str] = &["node", "nodes", "comments", "version", "rows", "search"];
    if READ_OPS.contains(&op) {
        let _gate = state.gate.read().unwrap_or_else(|e| e.into_inner());
        return api_op(&store, op, params);
    }
    let _gate = state.gate.write().unwrap_or_else(|e| e.into_inner());
    api_op(&store, op, params)
}

fn api_op(
    store: &crate::backlog::api::Store,
    op: &str,
    params: &Value,
) -> Result<Value, StoreError> {
    use crate::backlog::api;
    let node_row = |node: &api::Node| node.to_json();
    match op {
        "node" => {
            let id = param_str(params, "id")?;
            let found = api::node(store, id)?;
            Ok(json!({
                "node": found.map(|n| n.to_json()),
                "version": api::version(store)?,
            }))
        }
        "nodes" => {
            let filter: api::NodeFilter = params
                .get("filter")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| StoreError::Invalid(format!("bad filter: {e}").into()))?
                .unwrap_or_default();
            let page: api::Page = serde_json::from_value(params.clone())
                .map_err(|e| StoreError::Invalid(format!("bad page: {e}").into()))?;
            let connection = api::nodes(store, &filter, &page)?;
            Ok(json!({
                "nodes": connection.nodes.iter().map(node_row).collect::<Vec<_>>(),
                "page_info": serde_json::to_value(&connection.page_info).unwrap_or(Value::Null),
                "version": api::version(store)?,
            }))
        }
        "comments" => {
            let id = param_str(params, "id")?;
            let page: api::Page = serde_json::from_value(params.clone())
                .map_err(|e| StoreError::Invalid(format!("bad page: {e}").into()))?;
            let connection = api::comments(store, id, &page)?;
            Ok(json!({
                "nodes": connection
                    .nodes
                    .iter()
                    .map(crate::backlog::model::comment_to_json)
                    .collect::<Vec<_>>(),
                "page_info": serde_json::to_value(&connection.page_info).unwrap_or(Value::Null),
                "version": api::version(store)?,
            }))
        }
        "version" => Ok(json!({ "version": api::version(store)? })),
        "rows" => Ok(json!({
            "rows": api::rows(store, params
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false))?,
            "version": api::version(store)?,
        })),
        "search" => {
            let q = param_str(params, "q")?;
            Ok(json!({
                "rows": api::search(store, q, params
                    .get("limit")
                    .and_then(Value::as_i64))?,
                "version": api::version(store)?,
            }))
        }
        _ => api_mutation(store, op, params),
    }
}

fn api_mutation(
    store: &crate::backlog::api::Store,
    op: &str,
    params: &Value,
) -> Result<Value, StoreError> {
    use crate::backlog::api;
    let id = params.get("id").and_then(Value::as_str);
    match op {
        "node_create" => {
            let input: api::NodeCreateInput = input_of(params, "input")?;
            let payload = api::node_create(store, input)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "node_update" => {
            let input: api::NodeUpdateInput = input_of(params, "input")?;
            let payload = api::node_update(store, id.unwrap_or_default(), input)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "decision_record" => {
            let event: Value = input_of(params, "event")?;
            let payload = api::decision_record(store, event)?;
            Ok(json!({
                "success": payload.success,
                "event": payload.node,
                "version": payload.version,
            }))
        }
        "decision_retract" => {
            let event: Value = input_of(params, "event")?;
            let payload = api::decision_retract(store, event)?;
            Ok(json!({
                "success": payload.success,
                "event": payload.node,
                "version": payload.version,
            }))
        }
        "decisions" => {
            let node = params.get("node").and_then(Value::as_str);
            let decision_id = params.get("decision_id").and_then(Value::as_str);
            let rows = api::decisions(store, node, decision_id)?;
            Ok(json!({ "rows": rows }))
        }
        "node_batch_update" => {
            let ids: Vec<String> = input_of(params, "ids")?;
            let input: api::NodeUpdateInput = input_of(params, "input")?;
            let payload = api::node_batch_update(store, &ids, input)?;
            Ok(json!({
                "success": payload.success,
                "node": payload
                    .node
                    .map(|list| list.iter().map(|n| n.to_json()).collect::<Vec<_>>()),
                "version": payload.version,
            }))
        }
        "node_archive" => {
            let payload = api::node_archive(store, id.unwrap_or_default())?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "node_unarchive" => {
            let payload = api::node_unarchive(store, id.unwrap_or_default())?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "node_delete" => {
            let payload = api::node_delete(store, id.unwrap_or_default())?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "relation_create" | "relation_delete" => {
            let related = param_str(params, "related")?;
            let t: api::RelationType = match params.get("type") {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| StoreError::Invalid(format!("bad type: {e}").into()))?,
                None => api::RelationType::Related,
            };
            let payload = if op == "relation_create" {
                api::relation_create(store, id.unwrap_or_default(), related, t)?
            } else {
                api::relation_delete(store, id.unwrap_or_default(), related, t)?
            };
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "label_add" | "label_remove" => {
            let name = param_str(params, "name")?;
            let payload = if op == "label_add" {
                api::label_add(store, id.unwrap_or_default(), name)?
            } else {
                api::label_remove(store, id.unwrap_or_default(), name)?
            };
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "comment_create" => {
            let input: api::CommentCreateInput = input_of(params, "input")?;
            let payload = api::comment_create(store, id.unwrap_or_default(), input)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "pull_request_attach" => {
            let input: api::PullRequestInput = input_of(params, "input")?;
            let payload = api::pull_request_attach(store, id.unwrap_or_default(), input)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "session_append" => {
            let row: api::SessionRecord = input_of(params, "row")?;
            let payload = api::session_append(store, id.unwrap_or_default(), row)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "session_end" => {
            let session_id = param_str(params, "session_id")?;
            let ended_by = param_str(params, "ended_by")?;
            let phase = params.get("phase").and_then(Value::as_str);
            let harness = params.get("harness").and_then(Value::as_str);
            let ended_at = params.get("ended_at").and_then(Value::as_str);
            let payload = api::session_end(
                store,
                id.unwrap_or_default(),
                session_id,
                ended_by,
                phase,
                harness,
                ended_at,
            )?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "encounter_create" => {
            let input: api::EncounterInput = input_of(params, "input")?;
            let payload = api::encounter_create(store, id.unwrap_or_default(), input)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        "dispatch_set" => {
            let d: Option<api::Dispatch> = match params.get("dispatch") {
                Some(v) if !v.is_null() => Some(
                    serde_json::from_value(v.clone())
                        .map_err(|e| StoreError::Invalid(format!("bad dispatch: {e}").into()))?,
                ),
                _ => None,
            };
            let payload = api::dispatch_set(store, id.unwrap_or_default(), d)?;
            Ok(json!({
                "success": payload.success,
                "node": payload.node.map(|n| n.to_json()),
                "version": payload.version,
            }))
        }
        other => Err(StoreError::Invalid(
            format!("unknown api op {other:?}").into(),
        )),
    }
}

fn input_of<T: serde::de::DeserializeOwned>(params: &Value, key: &str) -> Result<T, StoreError> {
    serde_json::from_value(
        params
            .get(key)
            .cloned()
            .ok_or_else(|| StoreError::Invalid(format!("api op needs {key}").into()))?,
    )
    .map_err(|e| StoreError::Invalid(format!("bad {key}: {e}").into()))
}
/// The socket path for a graph file: a sibling `<graph>.store.sock`, so the
/// operator's state root. When the sibling would overrun the unix-socket
/// address limit (macOS binds 104 sun_path bytes, directory included), the
/// socket moves to a uid-keyed root under the platform temp dir, named by
/// the graph path's hash; mirrors the Python client's `store_socket_for`,
/// which is the path authority (it passes --sock).
pub fn store_socket_for(graph: &std::path::Path) -> PathBuf {
    const SOCK_PATH_LIMIT: usize = 96;
    let dir = graph.parent().unwrap_or(std::path::Path::new("."));
    let name = graph
        .file_name()
        .map(|n| format!("{}.store.sock", n.to_string_lossy()))
        .unwrap_or_else(|| "graph.store.sock".to_string());
    let sibling = dir.join(&name);
    if sibling.to_string_lossy().len() <= SOCK_PATH_LIMIT {
        return sibling;
    }
    use sha2::Digest as _;
    // Hash the ABSOLUTE spelling the Python client resolves: an existing
    // path canonicalizes (symlinks followed); a missing one keeps its
    // symlinked directory prefix and joins the tail lexically, matching
    // Python's non-strict resolve(). A spelling divergence would make this
    // keeper bind a socket no client finds.
    let absolute = match graph.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let abs = if graph.is_absolute() {
                graph.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(graph)
            };
            let resolved = abs
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .and_then(|resolved_parent| abs.file_name().map(|name| resolved_parent.join(name)));
            resolved.unwrap_or(abs)
        }
    };
    let mut h = sha2::Sha256::new();
    h.update(absolute.to_string_lossy().as_bytes());
    let digest: String = h
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("");
    // SAFETY: getuid reads a per-process kernel value; it cannot fail or
    // race, and this thread is not mid-syscall elsewhere.
    let uid = unsafe { libc::getuid() };
    let root = std::env::temp_dir().join(format!("fno-store-{uid}"));
    root.join(format!("{}.sock", &digest[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_store_state(graph: PathBuf) -> StoreState {
        StoreState {
            graph,
            canonical: false,
            lock_timeout: Duration::from_secs(2),
            gate: RwLock::new(()),
            inflight: RwLock::new(()),
            write_ledger: Mutex::new(std::collections::VecDeque::new()),
            gate_metrics: Mutex::new(GateMetrics::new()),
            last_write: Mutex::new(None),
            render_in_flight: std::sync::atomic::AtomicBool::new(false),
            render_failures: std::sync::atomic::AtomicU32::new(0),
            last_render_attempt: Mutex::new(None),
            events: None,
            sock_ino: None,
            startup_fp: None,
        }
    }

    #[test]
    fn write_status_reports_done_with_elided_entries() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            r#"{"entries":[{"id":"x-written","slug":"x-written","title":"written","type":"feature","status":"ready","priority":"p2"}]}"#,
        )
        .unwrap();
        let state = test_store_state(graph.clone());
        let request = json!({
            "id": 1,
            "method": "op",
            "params": {
                "name": "append_progress_note",
                "params": {"node_id": "x-written", "note": {"text": "noted"}},
                "request_id": "r1"
            }
        });

        let reply = handle_request(&state, &serde_json::to_vec(&request).unwrap());
        assert_eq!(reply["ok"], json!(true));
        let status = handle_request(
            &state,
            &serde_json::to_vec(&json!({
                "id": 2,
                "method": "write_status",
                "params": {"request_id": "r1"}
            }))
            .unwrap(),
        );
        assert_eq!(status["ok"], json!(true));
        assert_eq!(status["result"]["state"], json!("done"));
        assert_eq!(status["result"]["reply"]["ok"], json!(true));
        assert_eq!(
            status["result"]["reply"]["result"]["outcome"]["entries"],
            Value::Null
        );
        assert_eq!(
            status["result"]["reply"]["result"]["outcome"]["entries_elided"],
            json!(true)
        );
        let rows = crate::backlog::read_entries(&graph).unwrap();
        assert_eq!(rows[0]["progress_notes"][0]["text"], json!("noted"));
    }

    #[test]
    fn write_status_reports_unknown_request() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, r#"{"entries":[]}"#).unwrap();
        let state = test_store_state(graph);
        let status = handle_request(
            &state,
            &serde_json::to_vec(&json!({
                "id": 1,
                "method": "write_status",
                "params": {"request_id": "nope"}
            }))
            .unwrap(),
        );
        assert_eq!(status["ok"], json!(true));
        assert_eq!(status["result"], json!({"state": "unknown"}));
    }

    #[test]
    fn write_status_reports_in_flight_while_op_waits_on_gate() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            r#"{"entries":[{"id":"x-waiting","slug":"x-waiting","title":"waiting","type":"feature","status":"ready","priority":"p2"}]}"#,
        )
        .unwrap();
        let state = std::sync::Arc::new(test_store_state(graph));
        let request = json!({
            "id": 1,
            "method": "op",
            "params": {
                "name": "append_progress_note",
                "params": {"node_id": "x-waiting", "note": {"text": "noted"}},
                "request_id": "r-wait"
            }
        });
        let gate = state.gate.write().unwrap();
        let worker_state = std::sync::Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            handle_request(&worker_state, &serde_json::to_vec(&request).unwrap())
        });
        std::thread::sleep(Duration::from_millis(20));
        let status = handle_request(
            &state,
            &serde_json::to_vec(&json!({
                "id": 2,
                "method": "write_status",
                "params": {"request_id": "r-wait"}
            }))
            .unwrap(),
        );
        assert_eq!(status["ok"], json!(true));
        assert_eq!(status["result"]["state"], json!("in_flight"));
        drop(gate);
        assert_eq!(worker.join().unwrap()["ok"], json!(true));
    }

    fn read_state(graph: &std::path::Path) -> StoreState {
        StoreState {
            graph: graph.to_path_buf(),
            canonical: false,
            lock_timeout: Duration::from_secs(2),
            gate: RwLock::new(()),
            inflight: RwLock::new(()),
            write_ledger: Mutex::new(std::collections::VecDeque::new()),
            gate_metrics: Mutex::new(GateMetrics::new()),
            last_write: Mutex::new(None),
            render_in_flight: std::sync::atomic::AtomicBool::new(false),
            render_failures: std::sync::atomic::AtomicU32::new(0),
            last_render_attempt: Mutex::new(None),
            events: None,
            sock_ino: None,
            startup_fp: None,
        }
    }

    #[test]
    fn two_reads_hold_shared_guards_at_once_and_a_write_still_excludes() {
        // AC1: the positive marker is the second read guard being acquired
        // while the first is held; on the old Mutex this call would have
        // returned Err (and the fleet's reads serialized on it).
        let state = read_state(std::path::Path::new("/nonexistent/graph.json"));
        let g1 = state.gate.read().unwrap_or_else(|e| e.into_inner());
        let g2 = state.gate.try_read();
        assert!(g2.is_ok(), "a second read guard must share with the first");
        drop(g2);
        let w = state.gate.try_write();
        assert!(
            w.is_err(),
            "a writer must not enter while readers hold the gate"
        );
        drop(g1);
        assert!(
            state.gate.try_write().is_ok(),
            "the gate frees when the read guard drops"
        );
    }

    #[test]
    fn a_read_waits_out_an_in_flight_commit_and_never_sees_half_of_one() {
        // AC2: while a write guard is held (a commit's publish window), a
        // read blocks; when the guard drops, the read completes against the
        // committed store. Asserted on the positive marker (the read finishing
        // only after release), never on a timeout absence.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            "{\"entries\": [{\"id\": \"x-1\", \"slug\": \"x-1\", \"title\": \"before\", \"type\": \"feature\", \"status\": \"ready\", \"priority\": \"p2\"}]}",
        )
        .unwrap();
        let state = Arc::new(read_state(&graph));
        // The commit lands through the real op channel first, so the window
        // held below is a publish window: a reader admitted early would see
        // the committed note whole.
        let write = serde_json::to_vec(&json!({
            "id": 1,
            "method": "op",
            "params": {
                "name": "append_progress_note",
                "params": {"node_id": "x-1", "note": {"text": "noted"}},
                "request_id": "r-window"
            }
        }))
        .unwrap();
        let write_reply = handle_request(&state, &write);
        assert_eq!(
            write_reply["result"]["op"]["found"],
            json!(true),
            "the op must commit before the window opens: {write_reply}"
        );
        let window = state.gate.write().unwrap_or_else(|e| e.into_inner());
        let reader_state = Arc::clone(&state);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let payload = serde_json::to_vec(&json!({
                "id": 2,
                "method": "api",
                "params": {"op": "rows"}
            }))
            .unwrap();
            let reply = handle_request(&reader_state, &payload);
            let _ = tx.send(reply);
        });
        // Positive marker: the read cannot finish while the write guard is
        // held (the commit's publish window).
        assert!(
            rx.recv_timeout(Duration::from_millis(60)).is_err(),
            "a read must not complete while a commit's write guard is held"
        );
        drop(window);
        let reply = rx.recv().unwrap();
        let body = reply.to_string();
        assert!(
            body.contains("noted"),
            "the read must observe the committed write: {body}"
        );
    }

    #[test]
    fn plan_refs_ships_only_the_rung_inputs() {
        // The typed-op client derives the plan-rung map from this read, so
        // each row carries id + plan_path + cwd and nothing else: one
        // derived value must not cost a full begin. Absent fields ride as
        // null, which ladder.plan_rung reads as no plan, same as before.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let body = serde_json::to_string(&json!({
            "entries": [
                {"id": "x-planned", "slug": "x-planned", "title": "planned", "type": "feature",
                 "status": "ready", "priority": "p2",
                 "plan_path": "docs/plans/p.md", "cwd": "/tmp/proj",
                 "progress_notes": [{"ts": "t", "text": "x"}]},
                {"id": "x-bare", "slug": "x-bare", "title": "bare", "type": "feature",
                 "status": "idea", "priority": "p2"},
                {"id": "x-anchored", "slug": "third-node", "title": "third", "type": "feature",
                 "status": "ready", "priority": "p2",
                 "plan_path": "p.md#anchor", "cwd": "~/proj"},
            ]
        }))
        .unwrap();
        std::fs::write(&graph, body).unwrap();
        let state = read_state(&graph);
        let reply = handle_plan_refs(&state).unwrap();
        let entries = reply["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        for e in entries {
            let keys: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
            assert!(
                keys.iter()
                    .all(|k| matches!(*k, "id" | "plan_path" | "cwd")),
                "only the rung inputs ship, got {keys:?}"
            );
        }
        assert_eq!(entries[0]["plan_path"], json!("docs/plans/p.md"));
        assert_eq!(entries[0]["cwd"], json!("/tmp/proj"));
        assert!(
            entries[1]["plan_path"].is_null(),
            "a plan-less node ships a null plan_path, not guessed fields"
        );
    }

    #[test]
    fn read_ids_returns_overlaid_rows_in_order_and_reports_missing() {
        // AC9-HP / AC10-EDGE / AC11-EDGE: one row for one id (the reply body
        // is a row, not the graph), the readiness overlay applied server-side,
        // argument order preserved, unmatched tokens reported not guessed,
        // and a mixed-case slug resolving like the batch matcher's field_eq.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let body = serde_json::to_string(&json!({
            "entries": [
                {"id": "x-hit", "slug": "first-node", "title": "hit", "type": "feature",
                 "status": "ready", "priority": "p2", "blocked_by": ["x-gate"]},
                {"id": "x-gate", "slug": "second-node", "title": "gate", "type": "feature",
                 "status": "in_progress", "priority": "p2"},
                {"id": "x-late", "slug": "third-node", "title": "late", "type": "feature",
                 "status": "ready", "priority": "p2"},
            ]
        }))
        .unwrap();
        std::fs::write(&graph, body).unwrap();
        let state = read_state(&graph);
        let reply = handle_read_ids(
            &state,
            &json!({"ids": ["x-late", "second-node", "X-HIT", "x-nope"]}),
        )
        .unwrap();
        let entries = reply["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "three matched tokens, one row each");
        assert_eq!(entries[0]["id"], json!("x-late"));
        assert_eq!(entries[1]["id"], json!("x-gate"));
        assert_eq!(entries[2]["id"], json!("x-hit"));
        // The overlay: x-hit is blocked by x-gate's non-terminal status.
        assert_eq!(entries[2]["status"], json!("blocked"));
        assert!(entries[2]["blocked_reason"].is_string());
        let missing: Vec<String> = reply["missing"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(missing, vec!["x-nope".to_string()]);
    }

    /// Reads the Identify reply's `store_backend` over the wire.
    fn identify_backend(stream: &mut UnixStream) -> String {
        stream
            .write_all(&encode(TAG_IDENTIFY, b""))
            .expect("identify write");
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).expect("identify header");
        assert_eq!(header[0], TAG_IDENTIFY_REPLY, "unexpected reply tag");
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("identify body");
        let id: Value = serde_json::from_slice(&body).unwrap();
        id["store_backend"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn keeper_identify_reports_the_named_backend_live() {
        // Over the wire: the reply carries the store name, and a stamp by
        // another process changes nothing (the json backend is deleted; the
        // answer never flips).
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("backend.store.sock");
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let cfg = KeeperConfig {
            sock: sock.clone(),
            graph: graph.clone(),
            session: "test-backend".into(),
            canonical: false,
            lock_timeout: Duration::from_secs(2),
            events: None,
            // The Shutdown frame ends the keeper process from inside, which
            // under test kills the whole binary; the idle bound is the way a
            // test keeper exits.
            idle_limit: Some(Duration::from_millis(700)),
        };
        let handle = std::thread::spawn(move || run(cfg));
        let mut stream = loop {
            match UnixStream::connect(&sock) {
                Ok(stream) => break stream,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        assert_eq!(identify_backend(&mut stream), "sqlite");
        crate::backlog::set_backend(&graph).unwrap();
        assert_eq!(
            identify_backend(&mut stream),
            "sqlite",
            "the store name never flips"
        );
        // Drop the client and let the idle bound retire the keeper.
        drop(stream);
        let result = handle.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(!sock.exists(), "idle exit must unlink the socket");
    }

    #[test]
    fn a_keeper_with_an_idle_deadline_exits_and_unlinks_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("idle.store.sock");
        let cfg = KeeperConfig {
            sock: sock.clone(),
            graph: dir.path().join("graph.json"),
            session: "test-idle".into(),
            canonical: false,
            lock_timeout: Duration::from_secs(2),
            events: None,
            idle_limit: Some(Duration::from_millis(700)),
        };
        let handle = std::thread::spawn(move || run(cfg));
        let mut bound = false;
        for _ in 0..100 {
            if UnixStream::connect(&sock).is_ok() {
                bound = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(bound, "keeper never bound its socket");
        let result = handle.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(!sock.exists(), "idle exit must unlink the socket");
    }

    #[test]
    fn an_op_with_a_stale_base_version_conflicts_instead_of_writing() {
        // rank_top computes its rank from a begin snapshot; the base_version
        // it carries must make the keeper refuse when the store moved between
        // that read and the op, so the float is provably computed from fresh
        // peer ranks.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            "{\"entries\": [{\"id\": \"ab-a\", \"slug\": \"ab-a\", \"title\": \"a\", \"type\": \"feature\", \"status\": \"ready\", \"priority\": \"p2\", \"rank\": 5.0}]}",
        )
        .unwrap();
        let state = StoreState {
            graph: graph.clone(),
            canonical: false,
            lock_timeout: Duration::from_secs(2),
            gate: RwLock::new(()),
            inflight: RwLock::new(()),
            write_ledger: Mutex::new(std::collections::VecDeque::new()),
            gate_metrics: Mutex::new(GateMetrics::new()),
            last_write: Mutex::new(None),
            render_in_flight: std::sync::atomic::AtomicBool::new(false),
            render_failures: std::sync::atomic::AtomicU32::new(0),
            last_render_attempt: Mutex::new(None),
            events: None,
            sock_ino: None,
            startup_fp: None,
        };
        let stale = json!({
            "name": "update_fields",
            "params": {"node_id": "ab-a", "fields": {"rank": 1.0}},
            "base_version": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        });
        let err = handle_op(&state, &stale).unwrap_err();
        assert!(matches!(err, StoreError::Conflict), "{err}");
        // Nothing was written under the stale base.
        let rank = || crate::backlog::read_entries(&graph).unwrap()[0]["rank"].clone();
        assert_eq!(rank(), json!(5.0));

        // A matching base_version goes through.
        let fresh = json!({
            "name": "update_fields",
            "params": {"node_id": "ab-a", "fields": {"rank": 1.0}},
            "base_version": crate::backlog::version(&graph).unwrap(),
        });
        handle_op(&state, &fresh).unwrap();
        assert_eq!(rank(), json!(1.0));

        // No base_version at all: the pre-existing op contract, unchanged.
        let plain = json!({
            "name": "update_fields",
            "params": {"node_id": "ab-a", "fields": {"rank": 2.0}},
        });
        handle_op(&state, &plain).unwrap();
        assert_eq!(rank(), json!(2.0));
    }

    #[test]
    fn session_append_records_a_merge_grant_and_fills_absent_on_duplicate() {
        let mut entries = vec![json!({"id": "x-grnt", "title": "t", "status": "in_progress"})];
        let grant = json!({
            "approved": true, "source": "config",
            "recorded_by": "spawner", "recorded_at": "2026-09-02T10:00:00Z"
        });
        let req = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-grnt", "phase": "do", "harness": "claude",
                "session_id": "s-1", "merge_grant": grant,
            }
        });
        let out = apply_op_for_tests(&mut entries, &req).unwrap();
        assert_eq!(out["added"], json!(true));
        let row = entries[0]["sessions"][0].as_object().unwrap();
        assert_eq!(row["merge_grant"]["approved"], json!(true));
        assert_eq!(
            row["merge_grant"]["recorded_at"],
            json!("2026-09-02T10:00:00Z")
        );

        // A re-stamp carrying a DIFFERENT posture must not rewrite the
        // recorded one: the first resolved posture owns the row.
        let req2 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-grnt", "phase": "do", "harness": "claude",
                "session_id": "s-1",
                "merge_grant": {"approved": false, "source": "none",
                                "recorded_by": "spawner",
                                "recorded_at": "2026-09-02T11:00:00Z"},
            }
        });
        let out = apply_op_for_tests(&mut entries, &req2).unwrap();
        assert_eq!(out["added"], json!(false));
        assert_eq!(
            entries[0]["sessions"][0]["merge_grant"]["approved"],
            json!(true)
        );

        // An ABSENT grant on a fresh row writes no key.
        let req3 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-grnt", "phase": "review", "harness": "claude",
                "session_id": "s-2",
            }
        });
        apply_op_for_tests(&mut entries, &req3).unwrap();
        assert!(entries[0]["sessions"][1].get("merge_grant").is_none());

        // A malformed grant is refused at the store boundary.
        let req4 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-grnt", "phase": "do", "harness": "claude",
                "session_id": "s-3", "merge_grant": {"approved": "yes"},
            }
        });
        assert!(apply_op_for_tests(&mut entries, &req4).is_err());
    }

    #[test]
    fn session_append_dedupes_on_session_and_phase_across_harness_spellings() {
        let mut entries = vec![json!({"id": "x-twin", "title": "t", "status": "in_progress"})];
        let req = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-twin", "phase": "do", "harness": "claude",
                "session_id": "legacy-1", "started_at": "2026-09-04T10:00:00Z",
            }
        });
        apply_op_for_tests(&mut entries, &req).unwrap();
        // A second writer spelling a different harness for the SAME
        // (session_id, phase) fills the existing row; it never mints a twin.
        let req2 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-twin", "phase": "do", "harness": "unknown",
                "session_id": "legacy-1", "started_at": "2026-09-04T10:00:30Z",
                "ended_at": "2026-09-04T11:00:00Z",
            }
        });
        let out = apply_op_for_tests(&mut entries, &req2).unwrap();
        assert_eq!(out["added"], json!(false));
        let sessions = entries[0]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["harness"], json!("claude"));
        assert_eq!(sessions[0]["ended_at"], json!("2026-09-04T11:00:00Z"));
    }

    #[test]
    fn session_append_refuses_an_id_stamped_under_the_wrong_shape_harness() {
        let mut entries = vec![json!({"id": "x-shape", "title": "t", "status": "in_progress"})];
        // A codex UUIDv7 id under `harness: claude`: the phantom-twin shape.
        let req = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-shape", "phase": "do", "harness": "claude",
                "session_id": "01a06886-9405-74a1-8afd-5b67baf89604",
            }
        });
        let err = apply_op_for_tests(&mut entries, &req).unwrap_err();
        assert!(
            err.to_string()
                .contains("is a codex id; refusing harness claude"),
            "unexpected error: {err}"
        );
        assert!(entries[0].get("sessions").is_none());
        // The same id under its own harness stamps fine, and a v4 id is
        // accepted under claude.
        let req2 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-shape", "phase": "do", "harness": "codex",
                "session_id": "01a06886-9405-74a1-8afd-5b67baf89604",
            }
        });
        let out = apply_op_for_tests(&mut entries, &req2).unwrap();
        assert_eq!(out["added"], json!(true));
        let req3 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-shape", "phase": "do", "harness": "claude",
                "session_id": "b936b571-e0aa-40ed-a07d-97acb9a87db1",
            }
        });
        let out = apply_op_for_tests(&mut entries, &req3).unwrap();
        assert_eq!(out["added"], json!(true));
        // A shape-silent id (fno-minted uuid4 shape under grok) is never
        // refused: grok threads legally carry caller-minted v4 ids.
        let req4 = json!({
            "name": "session_append",
            "params": {
                "node_id": "x-shape", "phase": "do", "harness": "grok",
                "session_id": "8ad8e13c-1111-4222-8333-444455556666",
            }
        });
        let out = apply_op_for_tests(&mut entries, &req4).unwrap();
        assert_eq!(out["added"], json!(true));
    }

    fn render_trigger_state(graph: PathBuf, events: Option<PathBuf>) -> StoreState {
        StoreState {
            graph,
            canonical: true,
            lock_timeout: Duration::from_secs(2),
            gate: RwLock::new(()),
            inflight: RwLock::new(()),
            write_ledger: Mutex::new(std::collections::VecDeque::new()),
            gate_metrics: Mutex::new(GateMetrics::new()),
            last_write: Mutex::new(None),
            render_in_flight: std::sync::atomic::AtomicBool::new(false),
            render_failures: std::sync::atomic::AtomicU32::new(0),
            last_render_attempt: Mutex::new(None),
            events,
            sock_ino: None,
            startup_fp: None,
        }
    }

    fn seed_render_store(graph: &Path) -> String {
        graph_store::locked_mutate(
            graph,
            graph_store::MutateInput {
                entries: vec![json!({
                    "id": "x-rend", "title": "Render me", "slug": "x-rend",
                    "type": "feature", "status": "idea", "priority": "p2",
                    "created_at": "2026-09-11T00:00:00+00:00"
                })],
                canonical_path: None,
                base_version: graph_store::base_version(graph).unwrap(),
                plan_rungs: None,
            },
            Duration::from_secs(2),
        )
        .unwrap();
        crate::backlog::version(graph).unwrap()
    }

    /// The trigger's decision: a moved version renders once, a rendered
    /// version renders never, and an unsettled write burst renders later.
    #[test]
    fn render_trigger_is_due_only_for_moved_unrendered_versions() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let current = seed_render_store(&graph);
        let state = render_trigger_state(graph.clone(), None);

        assert!(
            render_due(&state, &current, None, Duration::ZERO),
            "a version with no rendered stamp is due once the burst settles"
        );
        assert!(
            !render_due(&state, &current, Some(&current), Duration::ZERO),
            "the rendered version never re-renders"
        );
        // A write 1 tick ago blocks the default settle.
        *state.last_write.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(std::time::Instant::now());
        assert!(
            !render_due(&state, &current, None, RENDER_SETTLE),
            "an unsettled burst waits"
        );
        // A state with NO stamp at all reads as settled: boot catch-up.
        let fresh = render_trigger_state(graph.clone(), None);
        assert!(render_due(&fresh, &current, None, RENDER_SETTLE));
    }

    /// The happy path: the pass runs, the marker stamps, and the next tick
    /// is a no-op (positive marker: the stamp exists and the runner ran).
    #[test]
    fn render_trigger_runs_the_pass_and_stamps_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let current = seed_render_store(&graph);
        let state = render_trigger_state(graph.clone(), None);
        let runs = std::cell::Cell::new(0u32);
        trigger_render_with(&state, || {
            runs.set(runs.get() + 1);
            Ok(())
        });
        assert_eq!(runs.get(), 1);
        assert_eq!(
            crate::backlog::rendered_version(&graph).unwrap(),
            Some(current),
            "the marker names the version the pass rendered"
        );

        // The stamped marker retires the debt: the next tick does not run.
        let runs2 = std::cell::Cell::new(0u32);
        trigger_render_with(&state, || {
            runs2.set(runs2.get() + 1);
            Ok(())
        });
        assert_eq!(runs2.get(), 0);
    }

    /// A failed pass journals `graph_render_failed` with the exit code and
    /// the stderr tail, leaves the marker unstamped, and the next tick
    /// retries the debt.
    #[test]
    fn render_trigger_journals_a_failed_pass_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let _current = seed_render_store(&graph);
        let events_path = dir.path().join("events.jsonl");
        let state = render_trigger_state(graph.clone(), Some(events_path.clone()));
        trigger_render_with(&state, || Err((3, "boom".into())));
        assert_eq!(
            crate::backlog::rendered_version(&graph).unwrap(),
            None,
            "a failed pass never stamps"
        );
        let journal = std::fs::read_to_string(&events_path).unwrap();
        assert!(journal.contains("graph_render_failed"), "{journal}");
        assert!(journal.contains("boom"), "{journal}");
        assert!(journal.contains("\"exit\":3"), "{journal}");
    }

    /// A failed pass backs off: an immediate second tick does not re-run the
    /// pass, one past the doubled quiet window retries, and a success resets
    /// the failure count.
    #[test]
    fn render_failure_backs_off_until_the_window_passes() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let _current = seed_render_store(&graph);
        let state = render_trigger_state(graph.clone(), None);
        let runs = std::cell::Cell::new(0u32);
        trigger_render_with(&state, || {
            runs.set(runs.get() + 1);
            Err((3, "boom".into()))
        });
        assert_eq!(runs.get(), 1);
        trigger_render_with(&state, || {
            runs.set(runs.get() + 1);
            Ok(())
        });
        assert_eq!(runs.get(), 1, "inside the backoff window the retry is held");
        // Age the last attempt past the doubled window: the retry fires.
        {
            let mut last = state.last_render_attempt.lock().unwrap();
            *last = last.map(|t| t - render_backoff(1) - Duration::from_secs(1));
        }
        trigger_render_with(&state, || {
            runs.set(runs.get() + 1);
            Ok(())
        });
        assert_eq!(runs.get(), 2, "past the window the retry runs");
        assert_eq!(
            state
                .render_failures
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "success resets the failure count"
        );
    }
}
