//! The mux's native path onto the ported graph store (x-b21e). The reorder
//! verbs used to shell the `fno` porcelain server-side; the store is ported
//! now, so the server speaks the store keeper's framed protocol directly:
//! `u8 tag | u32 LE length | payload`, the pane keeper's shape (mirrored in
//! `pty.rs`), with the protocol version riding Identify.
//!
//! The socket is resolved exactly as the Python client
//! (`cli/src/fno/graph/store.py store_socket_for`) resolves it - a
//! `<graph>.store.sock` sibling, or a hashed short name under the platform
//! temp dir when the sibling would overrun the unix-socket address limit -
//! and a missing keeper is spawned on demand, the same rule the Python
//! client follows. A refused store is a REFUSAL, never an empty answer: the
//! verbs surface the failure as the card's notice line.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TAG_REQUEST: u8 = 1;
const TAG_RESPONSE: u8 = 4;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Bounded wait for a keeper this call spawned to bind its socket.
const SPAWN_WAIT: Duration = Duration::from_secs(10);
const SOCK_PATH_LIMIT: usize = 96;

/// The store keeper socket for a graph file. Mirrors the Python client's
/// `store_socket_for` (the Python side is the path authority - it passes
/// `--sock`): a sibling socket, or a uid-keyed hashed name under the
/// platform temp dir when the sibling path would overrun the sun_path limit.
pub fn store_socket_for(graph: &Path) -> PathBuf {
    let dir = graph.parent().unwrap_or_else(|| Path::new("."));
    let name = graph
        .file_name()
        .map(|n| format!("{}.store.sock", n.to_string_lossy()))
        .unwrap_or_else(|| "graph.store.sock".to_string());
    let sibling = dir.join(&name);
    if sibling.to_string_lossy().len() <= SOCK_PATH_LIMIT {
        return sibling;
    }
    use sha2::Digest as _;
    let absolute = graph
        .canonicalize()
        .unwrap_or_else(|_| absolute_lexical(graph));
    let digest = sha2::Sha256::digest(absolute.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    // SAFETY: getuid reads a per-process kernel value; it cannot fail or race.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir()
        .join(format!("fno-store-{uid}"))
        .join(format!("{}.sock", &hex[..16]))
}

/// The absolute spelling a nonexistent path hashes to, matching the Python
/// client's non-strict `resolve()`.
fn absolute_lexical(graph: &Path) -> PathBuf {
    if graph.is_absolute() {
        return graph.to_path_buf();
    }
    std::env::current_dir().unwrap_or_default().join(graph)
}

fn worker_binary() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("FNO_AGENTS_WORKER") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(front) = std::env::var("FNO_AGENTS_FRONT") {
        let p = Path::new(&front).parent()?.join("fno-agents-worker");
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("fno-agents-worker");
    if p.is_file() {
        return Some(p);
    }
    which_worker()
}

fn which_worker() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("fno-agents-worker"))
        .find(|p| p.is_file())
}

fn spawn_keeper(graph: &Path) -> Result<(), String> {
    let binary = worker_binary().ok_or_else(|| {
        "fno-agents-worker not found (set FNO_AGENTS_WORKER or install the runtime)".to_string()
    })?;
    let sock = store_socket_for(graph);
    let session = format!("mux-{}", std::process::id());
    std::process::Command::new(&binary)
        .args([
            "--store-keeper",
            "--sock",
            sock.to_str().unwrap_or_default(),
            "--graph",
            graph.to_str().unwrap_or_default(),
            "--session",
            session.as_str(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot spawn store keeper: {e}"))?;
    Ok(())
}

fn connect(sock: &Path) -> Result<UnixStream, String> {
    match UnixStream::connect(sock) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("absent".into()),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Err("no_listener".into()),
        Err(e) => Err(format!("unreachable: {e}")),
    }
}

fn round_trip(
    stream: &mut UnixStream,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let payload = serde_json::to_vec(&json!({"id": 1, "method": method, "params": params}))
        .map_err(|e| e.to_string())?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(format!("request frame of {} bytes exceeds the cap", payload.len()));
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(TAG_REQUEST);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(30))))
        .map_err(|e| e.to_string())?;
    stream.write_all(&frame).map_err(|e| e.to_string())?;
    stream.flush().ok();

    let mut header = [0u8; 5];
    read_exact(stream, &mut header)?;
    if header[0] != TAG_RESPONSE {
        return Err(format!("unexpected frame tag {} from keeper", header[0]));
    }
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(format!("oversized reply frame ({len} bytes)"));
    }
    let mut body = vec![0u8; len];
    read_exact(stream, &mut body)?;
    let reply: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
    }
    let error = reply.get("error").cloned().unwrap_or(Value::Null);
    Err(format!(
        "{}",
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("store refused")
    ))
}

fn read_exact(stream: &mut UnixStream, buf: &mut [u8]) -> Result<(), String> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err("keeper closed the connection mid-frame".into()),
            Ok(n) => filled += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Whether a connect error means the socket is POSITIVELY dead (worth a
/// spawn + re-probe) rather than unknown.
fn dead_socket(err: &str) -> bool {
    err == "absent" || err == "no_listener"
}

/// One keeper request against `graph`, spawning the keeper when the socket
/// is positively dead and waiting a bounded time for it to bind.
pub fn call(graph: &Path, method: &str, params: Value) -> Result<Value, String> {
    let sock = store_socket_for(graph);
    let mut attempt = connect(&sock);
    if attempt.as_ref().is_err_and(|e| dead_socket(e)) {
        spawn_keeper(graph)?;
        let deadline = Instant::now() + SPAWN_WAIT;
        loop {
            attempt = connect(&sock);
            if !attempt.as_ref().is_err_and(|e| dead_socket(e)) {
                break;
            }
            if Instant::now() >= deadline {
                return Err("the store keeper never bound its socket".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let mut stream = attempt?;
    round_trip(&mut stream, method, params)
}

/// A finite, non-huge rank value (render._rank_band's rule): a poisoned
/// non-finite or giant-int peer rank is unranked, so it can never corrupt
/// the --top arithmetic or persist a NaN/inf rank.
fn ranked_rank(e: &Value) -> Option<f64> {
    let r = e.get("rank")?;
    if let Value::Number(n) = r {
        let v = n.as_f64()?;
        if v.is_finite() && v.abs() < 1e15 {
            return Some(v);
        }
    }
    None
}

/// `backlog rank <node> --top`, natively: min(ranked peer ranks in the
/// target's (column, project) lane) - 1.0, or 0.0 for an unranked lane.
/// Lanes come from this crate's own `kanban_column` mirror, computed
/// claim-blind: rank never changes a column, and a claimed peer's extra rank
/// only pushes the float slightly lower, never into another lane's band.
pub fn rank_top(graph: &Path, node_id: &str) -> Result<String, String> {
    let snap = call(graph, "begin", json!({}))?;
    let entries = snap
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "the store returned no entries".to_string())?;
    let target = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(node_id))
        .ok_or_else(|| format!("no node resolves to '{node_id}'"))?;
    let project = target
        .get("project")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let lane = backlog_column(target);
    let head = entries
        .iter()
        .filter(|e| e.get("id").and_then(Value::as_str) != Some(node_id))
        .filter(|e| {
            backlog_column(e) == lane
                && e.get("project").and_then(Value::as_str).unwrap_or_default() == project
        })
        .filter_map(ranked_rank)
        .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a: f64| a.min(v))));
    let new_rank = head.map_or(0.0, |h| h - 1.0);
    call(
        graph,
        "op",
        json!({
            "name": "update_fields",
            "params": {
                "node_id": node_id,
                "fields": {"rank": new_rank}
            }
        }),
    )?;
    Ok(format!(
        "float to top: {node_id} (rank {new_rank})",
        new_rank = format_rank(new_rank)
    ))
}

fn format_rank(v: f64) -> String {
    if v == v.trunc() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// The kanban column for an entry, claim-blind (see [`rank_top`]).
fn backlog_column(e: &Value) -> Option<&'static str> {
    // `kanban_column` needs claim/underway facts the verb deliberately does
    // not fetch; false for both is the unclaimed view the float targets.
    crate::backlog_view::kanban_column(e, false, false)
}

/// `backlog defer <node> --reason <why>`, natively: the keeper's defer op
/// mirrors the CLI's exact write (locks cleared, deferred_at stamped,
/// deferred_kind classified from the reason).
pub fn defer(graph: &Path, node_id: &str, reason: &str) -> Result<String, String> {
    call(
        graph,
        "op",
        json!({
            "name": "defer",
            "params": {"node_id": node_id, "reason": reason}
        }),
    )?;
    Ok(format!("defer: {node_id}"))
}

/// `backlog advance --epic <node> --stop`, natively: clears the epic's
/// `mission_active` and dispatches nothing (LD6).
pub fn end_mission(graph: &Path, node_id: &str) -> Result<String, String> {
    call(
        graph,
        "op",
        json!({
            "name": "end_mission",
            "params": {"node_id": node_id}
        }),
    )?;
    Ok(format!("end mission: {node_id}"))
}
