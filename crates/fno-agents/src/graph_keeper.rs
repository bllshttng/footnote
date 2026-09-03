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
//! Methods: `read`, `read_strict`, `begin`, `commit`, `op`, `read_archive`.
//! Responses: `{"id": n, "ok": true, "result": ...}` or
//! `{"id": n, "ok": false, "error": {"kind": ..., "message": ...}}`.
//!
//! Unlike the pane keeper's single subscriber, every connection is served
//! concurrently: reads share, writes serialize on the state mutex and the
//! bounded flock. A store RPC must never starve a concurrent client or a
//! SIGTERM (the `gc_sweep` lesson, x-d78a): each request runs on its own
//! thread, and the accepting loop never blocks on request work.
//!
//! Reapability is a release condition (the 2026-09-01 seven-unreaped-keepers
//! measurement): this lane declares itself the way keeper_lane.py discovers
//! keepers, through `--sock`/`--session` on its own argv plus the
//! `--store-keeper` lane flag, and its socket lives beside the graph file it
//! owns, so both the process-table walk and the socket-dir walk find it.

use crate::graph_store::{self, FieldUpdate, MutateInput, StoreError};
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Parsed `--store-keeper` lane argv:
/// `--store-keeper --sock <path> --graph <path> [--session <id>]
/// [--canonical] [--lock-timeout-secs N]`.
pub struct KeeperConfig {
    pub sock: PathBuf,
    pub graph: PathBuf,
    pub session: String,
    /// True when `--graph` IS the configured canonical graph: gates the
    /// closure-release hook and canonical-board effects.
    pub canonical: bool,
    pub lock_timeout: Duration,
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

/// The keeper's shared state. Writes serialize here; reads take the same
/// mutex because a read mid-publish would observe the file between the two
/// atomic replaces (bytes then sidecar).
struct StoreState {
    graph: PathBuf,
    canonical: bool,
    lock_timeout: Duration,
    /// Serializes the read-modify-write cycles across client threads.
    write_gate: Mutex<()>,
}

/// Run the store keeper to completion. Returns only on a startup failure;
/// a Shutdown frame ends the process from inside.
pub fn run(cfg: KeeperConfig) -> Result<(), String> {
    // SAFETY: setsid before any thread exists; a group-kill aimed at a
    // spawning process's group must not take the store with it.
    unsafe {
        libc::setsid();
    }
    // Connect-before-bind: a double keeper is a loud refusal.
    if UnixStream::connect(&cfg.sock).is_ok() {
        return Err(format!(
            "store socket {} already has a live listener behind it",
            cfg.sock.display()
        ));
    }
    let _ = std::fs::remove_file(&cfg.sock);
    if let Some(parent) = cfg.sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let listener = UnixListener::bind(&cfg.sock)
        .map_err(|e| format!("cannot bind {}: {e}", cfg.sock.display()))?;

    let state = Arc::new(StoreState {
        graph: cfg.graph.clone(),
        canonical: cfg.canonical,
        lock_timeout: cfg.lock_timeout,
        write_gate: Mutex::new(()),
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
    })
    .to_string()
    .into_bytes();

    let shutdown = Arc::new(AtomicU64::new(0));
    let active_clients = Arc::new(AtomicU64::new(0));
    let mut last_activity = std::time::Instant::now();
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
                if let Some(limit) = cfg.idle_limit {
                    if active_clients.load(Ordering::SeqCst) == 0
                        && last_activity.elapsed() >= limit
                    {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_file(&cfg.sock);
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
                let _ = stream.write_all(&encode(TAG_IDENTIFY_REPLY, &identify));
                let _ = stream.flush();
            }
            Incoming::Shutdown => {
                // Explicit shutdown: acknowledge, unlink, exit. A daemon
                // restart never sends this frame, which is exactly the
                // survived-hangup vs survived-close line; an explicit
                // shutdown ends the process here, so in-flight writers on
                // other threads are bounded by the atomic-replace publish.
                let _ = stream.write_all(&encode(
                    TAG_RESPONSE,
                    json!({"id": 0, "ok": true, "result": "shutdown"})
                        .to_string()
                        .as_bytes(),
                ));
                let _ = stream.flush();
                shutdown.store(1, Ordering::SeqCst);
                let _ = std::fs::remove_file(store_socket_for(&state.graph));
                std::process::exit(0);
            }
            Incoming::Request(payload) => {
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

fn err_reply(id: u64, kind: &str, message: String) -> Value {
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
        StoreError::Io(_) => "io",
    }
}

fn handle_request(state: &StoreState, payload: &[u8]) -> Value {
    let req: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => return err_reply(0, "malformed_frame", format!("request is not JSON: {e}")),
    };
    let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "read" => handle_read(state, &params),
        "read_strict" => handle_read(state, &params),
        "begin" => handle_begin(state),
        "commit" => handle_commit(state, &params),
        "op" => handle_op(state, &params),
        "read_archive" => handle_read_archive(state, &params),
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
        // The read-time readiness overlay (statuses.compute_readiness), for
        // the client's pre-render pass: the write path's recompute does not
        // derive `blocked` -- it is a read overlay -- so a mutation that
        // newly blocks a sibling must overlay before rendering graph.md.
        "overlay" => handle_pure(&params, |mut entries, _p| {
            graph_store::apply_readiness_overlay(&mut entries);
            entries
        }),
        "normalize_plan_path" => {
            let normalized = graph_store::normalize_plan_path(opt_str(&params, "path"));
            Ok(json!({ "path": normalized }))
        }
        // The canonical key order, for the ordering tests and any caller
        // that documents the on-disk shape: one source of truth (the
        // ported store's constant), never a re-typed copy.
        "canonical_field_order" => Ok(json!({ "fields": graph_store::CANONICAL_FIELD_ORDER })),
        // One named op applied over client-shipped rows, no file I/O and no
        // publish: `set_related`, `plan_path_owner_conflict`, and friends
        // run INSIDE a client mutator on an in-hand snapshot, where a full
        // locked cycle would be a write the caller never asked for.
        "pure_op" => handle_pure_op(&params),
        other => Err(StoreError::Invalid(format!(
            "unknown store method {other:?}"
        ))),
    };
    match result {
        Ok(v) => json!({"id": id, "ok": true, "result": v}),
        Err(e) => err_reply(id, store_err_kind(&e), e.to_string()),
    }
}

/// The default-entries read, returned with the byte-exact serialization the
/// file leg produced, so the parity contract ("byte-for-byte on the
/// serialized result") is checkable on the wire. `read` is the soft path
/// (a corrupt read leaves a .bak behind, as read_graph did); `read_strict`
/// diagnoses without writing.
fn handle_read(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let _gate = state.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    let strict = params
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let keep_malformed = params
        .get("keep_malformed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Entries only: the parity-era byte-serialization echoes rode every
    // reply and tripled its size on a large graph; the differential stage
    // that needed them is over (graph_store_parity.rs is characterization).
    let entries = graph_store::read_defaulted_opts(&state.graph, keep_malformed, !strict)?;
    Ok(json!({ "entries": entries }))
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

/// The raw file bytes + their digest, for the hash-validated read
/// (load_graph): the sidecar contract is the client's to enforce against
/// these bytes, and the keeper's serialized publish guarantees the reads
/// never observe the two-write window the Python retry loop existed for.
fn handle_read_file(state: &StoreState) -> Result<Value, StoreError> {
    use std::io::Read as _;
    let _gate = state.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = std::fs::File::open(&state.graph)
        .map_err(|e| StoreError::Unreadable(state.graph.display().to_string(), format!("{e}")))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(json!({
        "bytes_b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
        "sha256": file_version(&state.graph),
    }))
}

fn handle_begin(state: &StoreState) -> Result<Value, StoreError> {
    let _gate = state.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    let version = file_version(&state.graph);
    let entries = graph_store::read_defaulted(&state.graph, false)?;
    Ok(json!({
        "version": version,
        "entries": entries,
    }))
}

fn file_version(path: &std::path::Path) -> String {
    graph_store::file_content_version(path)
}

fn handle_commit(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let version = params
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("commit needs a version".into()))?;
    let entries: Vec<Value> = params
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::Invalid("commit needs entries".into()))?
        .clone();
    let _gate = state.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = graph_store::locked_mutate(
        &state.graph,
        MutateInput {
            entries,
            canonical_path: state.canonical.then(|| state.graph.clone()),
            base_version: Some(version.to_string()),
            plan_rungs: plan_rung_map(params),
        },
        state.lock_timeout,
    )?;
    Ok(outcome_json(&outcome))
}

/// The client-supplied node id -> plan rung map (see
/// `graph_store::supplied_plan_rung`): repo law keeps plan-document reading
/// on the Python side, so the map crosses as data. Absent key = the caller
/// is not re-deriving from plans, and stored statuses stay.
fn plan_rung_map(params: &Value) -> Option<std::collections::BTreeMap<String, String>> {
    let obj = params.get("plan_rungs")?.as_object()?;
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
        "backup": outcome.backup,
        "closure_releases": outcome
            .closure_releases
            .iter()
            .map(|(id, rung)| json!({"id": id, "rung": rung}))
            .collect::<Vec<_>>(),
        "is_canonical": outcome.is_canonical,
    })
}

fn handle_read_archive(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let archive = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("read_archive needs a path".into()))?;
    let _gate = state.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    match graph_store::read_defaulted(std::path::Path::new(archive), false) {
        Ok(entries) => Ok(json!({
            "entries": entries,
            "serialized": graph_store::serialize_graph_file(&entries),
        })),
        // The archive is advisory: any read failure degrades to empty.
        Err(_) => Ok(json!({"entries": []})),
    }
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

/// Test bridge for the differential parity harness: the parity test drives
/// the exact op dispatch the keeper serves, in-process, so both legs run the
/// same code paths without a socket in the loop.
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
            let node_id = param_str(p, "node_id")?;
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
            let idx = find_exact(entries, node_id)
                .ok_or_else(|| StoreError::Invalid(format!("no node resolves to '{node_id}'")))?;
            let obj = entries[idx].as_object_mut().unwrap();
            obj.insert("locked_by".to_string(), Value::Null);
            obj.insert("locked_at".to_string(), Value::Null);
            obj.insert("completed_at".to_string(), Value::Null);
            obj.insert(
                "deferred_at".to_string(),
                Value::String(graph_store::now_isoformat()),
            );
            obj.insert(
                "deferred_reason".to_string(),
                Value::String(reason.to_string()),
            );
            match kind {
                Some(k) => {
                    obj.insert("deferred_kind".to_string(), Value::String(k));
                }
                None => {
                    obj.shift_remove("deferred_kind");
                }
            }
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
    match reason {
        "stale >30d, drained by maintain" => Some("expired"),
        "stale-quarantine (guard)" => Some("expired"),
        _ => None,
    }
}

fn node_carries_pr(node: &Value, pr_number: i64, repo: Option<&str>) -> bool {
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
/// (phase, harness, session_id); a duplicate fills only timestamps it left
/// open, and observed_model is the one field the LATEST stamp owns.
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
    let harness = row
        .get("harness")
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
            && r.get("harness").and_then(Value::as_str) == Some(harness.as_str())
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

/// store.reap_open_session_record: close one exact open row with positive
/// death evidence. `do` REMOVES; every other phase FILLS ended_at; `all`
/// applies both to every open row carrying the identity.
fn session_reap_open(
    entries: &mut Vec<Value>,
    node_id: &str,
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
        SESSION_PHASES
            .iter()
            .copied()
            .filter(|p| *p != "do")
            .collect()
    } else if phase == "do" {
        vec![]
    } else {
        vec![phase]
    };
    let remove_do = phase == "do" || phase == "all";
    if harness.trim().is_empty() || session_id.trim().is_empty() {
        return Err(StoreError::Invalid(
            "identity must be non-empty strings".into(),
        ));
    }
    let ended = match ended_at {
        Some(v) => stamp_utc(v)?,
        None => chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    let Some(idx) = find_exact(entries, node_id) else {
        return Ok(json!({
            "found": false, "settled": false, "row_removed": false, "row_closed": false,
            "status_before": null, "status_after": null, "remaining_open_do": 0,
        }));
    };
    let status_before = entries[idx]
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string);
    let obj = entries[idx].as_object_mut().unwrap();
    let rows = obj
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_open = |r: &Value, phase: &str| -> bool {
        r.get("phase").and_then(Value::as_str) == Some(phase)
            && r.get("harness")
                .and_then(Value::as_str)
                .map(|h| !h.trim().is_empty())
                .unwrap_or(false)
            && r.get("session_id")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            && r.get("started_at")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            && !r
                .as_object()
                .map(|o| o.contains_key("ended_at"))
                .unwrap_or(false)
    };
    let mut row_removed = false;
    let mut kept: Vec<Value> = rows.clone();
    if remove_do {
        kept.retain(|r| {
            let matches = is_open(r, "do")
                && r.get("harness").and_then(Value::as_str) == Some(harness)
                && r.get("session_id").and_then(Value::as_str) == Some(session_id);
            if matches {
                row_removed = true;
            }
            !matches
        });
    }
    let mut row_closed = false;
    for cp in &close_phases {
        for r in kept.iter_mut() {
            if is_open(r, cp)
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
    if row_removed || row_closed {
        obj.insert("sessions".to_string(), Value::Array(kept));
    }
    Ok(json!({
        "found": true,
        "settled": true,
        "row_removed": row_removed,
        "row_closed": row_closed,
        "status_before": status_before,
        "status_after": Value::Null,
        "remaining_open_do": Value::Null,
    }))
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
/// The write_gate serializes this against every other keeper-side cycle, and
/// the base-version check still guards against a FOREIGN writer (an old
/// Python leg, a hand edit) that touched the file after the read.
fn handle_op(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("op needs a name".into()))?;
    let p = params.get("params").cloned().unwrap_or(Value::Null);
    let _gate = state.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    let base = graph_store::file_content_version(&state.graph);
    let mut entries = graph_store::read_defaulted(&state.graph, false)?;
    let op_result = apply_op(&mut entries, name, &p)?;
    let outcome = graph_store::locked_mutate(
        &state.graph,
        MutateInput {
            entries,
            canonical_path: state.canonical.then(|| state.graph.clone()),
            base_version: Some(base),
            // The Python client sends the begin snapshot's map with every op
            // (a session op that opens or closes a do row re-derives
            // in_progress like any full write); a caller that sends none
            // keeps stored statuses.
            plan_rungs: plan_rung_map(&p),
        },
        state.lock_timeout,
    )?;
    Ok(json!({
        "op": op_result,
        "outcome": outcome_json(&outcome),
    }))
}

/// The socket path for a graph file: a sibling `<graph>.store.sock`, so the
/// keeper's discovery needs no config and a tmp test graph never touches the
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
}
