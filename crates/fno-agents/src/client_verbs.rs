//! Client-side `fno-agents` verbs ported from the Python `fno agents` app
//! (the "Python-only verbs" bucket: `drive-authority`, `trace`, `ping`,
//! `attach`, `resume`, plus the non-streaming `logs` paths).
//!
//! These verbs do **not** issue a daemon RPC (with the sole exception of
//! `logs --follow` for codex/gemini, handled in [`crate::logs_client`]): they
//! read state/registry/event files directly, exec a provider CLI, or print a
//! stub. The Python implementations stay registered as the
//! `FNO_AGENTS_RUNTIME=python` fallback; this module is the Rust surface the
//! default `auto` runtime routes to.
//!
//! **Byte-for-byte parity is the contract** (the promotion gate). Each verb
//! reproduces the Python implementation's stdout, stderr, and exit codes. Two
//! Python-isms are reproduced deliberately:
//!
//! - `drive-authority --json` uses Python's *default* `json.dumps` separators
//!   (`", "` / `": "`, with spaces) -- see [`to_python_json`].
//! - `trace --json` uses *compact* separators and `sort_keys=False`; events.jsonl
//!   lines are already compact, so each matching line is emitted verbatim to
//!   preserve source key order without a crate-wide serde_json `preserve_order`.

use crate::claude_ask::{liveness_probe, locate_session, ClaudeHome};
use crate::paths::AgentsHome;
use crate::state::REGISTRY_SCHEMA_VERSION;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Python-default json.dumps formatter (separators `, ` and `: `).
// ---------------------------------------------------------------------------

/// A serde_json [`Formatter`](serde_json::ser::Formatter) that mirrors Python's
/// default `json.dumps` spacing: `", "` between items and `": "` after a key.
/// serde_json's default `CompactFormatter` emits no spaces, which would diverge
/// from Python's default-separator output (used by `drive-authority --json`).
struct PythonDefaultFormatter;

impl serde_json::ser::Formatter for PythonDefaultFormatter {
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        writer.write_all(b": ")
    }
}

/// Serialize `value` with Python's default `json.dumps` spacing. Field order is
/// the struct's declaration order (serde serializes struct fields in order), so
/// callers control key order by field order rather than relying on map ordering.
fn to_python_json<T: Serialize>(value: &T) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PythonDefaultFormatter);
    value
        .serialize(&mut ser)
        .expect("serializing an owned value to a Vec never fails");
    String::from_utf8(buf).expect("serde_json emits valid UTF-8")
}

/// Normalize `--key=value` tokens into `["--key", "value"]`, matching Click/Typer
/// (which accept both the equals and space-separated forms). Only long options
/// are split; positionals and `-n5`-style attached short options pass through.
fn expand_eq(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    for a in rest {
        if let Some(eq) = a.find('=') {
            if a.starts_with("--") && eq > 2 {
                out.push(a[..eq].to_string());
                out.push(a[eq + 1..].to_string());
                continue;
            }
        }
        out.push(a.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// ping
// ---------------------------------------------------------------------------

/// `fno-agents ping` -- verbatim port of the Python phase-1 stub.
///
/// The Python implementation (`cli.py::cmd_ping`) prints a fixed placeholder
/// and exits 0. Porting it as a stub (rather than inventing liveness semantics)
/// is the parity-preserving choice: byte-parity with the stub is what lets the
/// verb auto-route (Locked Decision #3). AC5-PING explicitly accepts the
/// "stub-verbatim" resolution of Open Question 1.
pub fn run_ping(args: &[String]) -> i32 {
    // Python `ping` takes no arguments; Typer rejects extras with exit 2.
    if let Some(extra) = args.iter().find(|a| !a.is_empty()) {
        eprintln!("fno-agents: ping takes no arguments (got: {extra})");
        return 2;
    }
    println!("(not yet implemented; planned for a future story)");
    0
}

// ---------------------------------------------------------------------------
// drive-authority
// ---------------------------------------------------------------------------

/// Drive modes that open the gate-hardening authority window (Python
/// `AUTHORITY_MODES`). `watch` is the read-only carve-out and is excluded.
const AUTHORITY_MODES: &[&str] = &["interactive", "step", "paranoid"];

/// One active drive-authority session. Field order is the JSON object key order
/// Python emits (`active_drive_sessions` builds `{short_id, session_id, mode}`).
#[derive(Serialize)]
struct DriveAuthSession {
    short_id: String,
    /// `drive_session_id` may be absent/null in `state.json`; preserved as-is so
    /// the JSON shows `null` (matching Python's `None`).
    session_id: Value,
    mode: String,
}

#[derive(Serialize)]
struct DriveAuthOut {
    active: bool,
    sessions: Vec<DriveAuthSession>,
}

/// Python truthiness of an optional JSON value (`not pty.get("drive_active")`).
fn json_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Render a JSON value the way Python's f-string `str()` would for the human
/// `drive-authority` line: a string prints unquoted; `null`/absent prints
/// `None` (Python `str(None)`); everything else falls back to compact JSON.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Scan each agent's `state.json` for an open authority drive window, mirroring
/// Python `drive_authority.active_drive_sessions`. Reads raw JSON (not the typed
/// `AgentState`) so an edge-case `state.json` -- e.g. one with `drive_active`
/// but no `short_id` -- is handled exactly as Python's `data.get(...)` does,
/// rather than diverging on strict deserialization.
fn active_drive_sessions(agents_root: &Path) -> Vec<DriveAuthSession> {
    let mut sessions = Vec::new();
    let read = match fs::read_dir(agents_root) {
        Ok(rd) => rd,
        Err(_) => return sessions, // base dir absent -> "no authority", never an error
    };
    // Python iterates `sorted(base.iterdir())`; sort by entry name for the same order.
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if dir_name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Best-effort read: a missing/unreadable/partial state.json is skipped.
        let data: Value = match fs::read_to_string(path.join("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let pty = match data.get("pty") {
            Some(p) if p.is_object() => p,
            _ => continue,
        };
        if !json_truthy(pty.get("drive_active")) {
            continue;
        }
        let mode = match pty.get("drive_mode").and_then(Value::as_str) {
            Some(m) if AUTHORITY_MODES.contains(&m) => m.to_string(),
            _ => continue,
        };
        let short_id = data
            .get("short_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or(dir_name);
        let session_id = pty.get("drive_session_id").cloned().unwrap_or(Value::Null);
        sessions.push(DriveAuthSession {
            short_id,
            session_id,
            mode,
        });
    }
    sessions
}

/// `fno-agents drive-authority [--json]` -- report open gate-hardening windows.
/// Exit 0 when any agent holds an interactive/step/paranoid window, else 1.
pub fn run_drive_authority(args: &[String], home: &AgentsHome) -> i32 {
    let mut json_out = false;
    for a in args {
        match a.as_str() {
            "--json" | "-J" => json_out = true, // ab-3ff64151: global-register short
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown drive-authority flag: {other}");
                return 2;
            }
            other => {
                eprintln!(
                    "fno-agents: drive-authority takes no positional arguments (got: {other})"
                );
                return 2;
            }
        }
    }

    let sessions = active_drive_sessions(home.root());
    let active = !sessions.is_empty();

    if json_out {
        let out = DriveAuthOut { active, sessions };
        println!("{}", to_python_json(&out));
    } else if active {
        for s in &sessions {
            // Python human line order: short_id, mode, session_id.
            println!("{} {} {}", s.short_id, s.mode, py_str(&s.session_id));
        }
    } else {
        println!("no active drive authority");
    }

    if active {
        0
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// trace
// ---------------------------------------------------------------------------

const REQUEST_ID_PREFIX_LEN: usize = 8;
/// The exact orphan-marker line Python emits (copied verbatim for byte parity).
const ORPHAN_MARKER: &str = "                                          no _done received";

/// Resolve the project/state events.jsonl the way Python's `trace_logic` does:
/// `paths.state_dir() / "events.jsonl"`. The Rust agents home is
/// `state_dir/agents`, so the events log is the agents-home parent's
/// `events.jsonl`.
fn trace_events_path(home: &AgentsHome) -> PathBuf {
    home.root()
        .parent()
        .map(|p| p.join("events.jsonl"))
        .unwrap_or_else(|| PathBuf::from("events.jsonl"))
}

/// Parse an ISO8601 timestamp into a UTC instant, mirroring Python's
/// `_parse_iso8601`: a trailing `Z` becomes `+00:00`, naive timestamps are
/// assumed UTC. Returns `None` on unparseable input (the caller degrades open).
fn parse_iso8601(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
    let raw = s.trim();
    let raw = match raw.strip_suffix('Z') {
        Some(stripped) => format!("{stripped}+00:00"),
        None => raw.to_string(),
    };
    if let Ok(dt) = DateTime::parse_from_rfc3339(&raw) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%:z"] {
        if let Ok(dt) = DateTime::parse_from_str(&raw, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(&raw, fmt) {
            return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(&raw, "%Y-%m-%d") {
        let ndt = d.and_hms_opt(0, 0, 0)?;
        return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
    }
    None
}

/// Read JSONL records, returning `(raw_line, parsed)` pairs and a malformed
/// count. Mirrors Python `_read_jsonl`: UTF-8 with replacement, skip blank
/// lines, count lines that fail to parse or are non-objects.
fn read_jsonl(path: &Path) -> (Vec<(String, Value)>, usize) {
    let mut records = Vec::new();
    let mut malformed = 0usize;
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return (records, 0), // absent file -> no records (Python: path.exists() guard)
    };
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) if v.is_object() => records.push((line.to_string(), v)),
            _ => malformed += 1,
        }
    }
    (records, malformed)
}

/// A well-shaped registry identity token (provider or harness): non-empty,
/// all-lowercase, whitespace-free. The relaxed load-gate corruption guard
/// (x-8dfc) mirroring Python `registry._is_identity_token` -- it replaced the
/// `KNOWN_PROVIDERS` enumeration so one alien harness never bricks the shared
/// read (it degrades to durable routing, x-ec59 posture); dispatch capability
/// is gated separately at the spawn/ask seam (`bin/client.rs`).
fn is_identity_token(v: Option<&str>) -> bool {
    matches!(
        v,
        Some(s) if !s.is_empty()
            && s == s.to_lowercase()
            && !s.chars().any(|c| c.is_whitespace())
    )
}
/// Valid registry statuses. `registry.status` is a projection of
/// `state.status` (LD10), so it can be ANY [`crate::AgentStatus`] variant —
/// the daemon writes `live` on spawn and `exited` on child exit (the latter
/// "retained until rm" per the AgentStatus docs), and reconcile writes
/// `orphaned`. The earlier `{live, orphaned}` set was too narrow: it rejected
/// the `exited` rows the daemon legitimately writes, hard-erroring every
/// registry read until the row was rm'd. This is the full snake_case
/// AgentStatus vocabulary (mirrors the `status-v1` enum in
/// `crate::emit_schema_json`); it accepts every valid projected status while
/// still rejecting genuine garbage. Must stay in lockstep with Python
/// `registry.py::KNOWN_STATUSES`.
const KNOWN_STATUSES: &[&str] = &[
    "spawning",
    "ready",
    "idle",
    "busy",
    "live",
    "restarting",
    "orphaned",
    "failed",
    "exited",
    "permanent_dead",
];
/// Registry schema versions this fno reads (current write version plus the older
/// shapes it back-fills in memory). Each bump is forward-compat: a stale reader
/// pinned to a lower set rejects a newer store instead of silently dropping a
/// field. v10 (x-880e) removes the on-disk `provider` + per-provider session-id
/// trio; a legacy v1..=v9 row still carries `provider`, read leniently below.
const ACCEPTED_SCHEMA_VERSIONS: &[u64] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

// The accepted set's upper bound MUST equal the version this binary writes, or
// a freshly-written store would be rejected by its own reader. Compiler-enforced
// so a future REGISTRY_SCHEMA_VERSION bump that forgets to widen the array fails
// the build instead of drifting silently (type-design review, ab-a171ceb2).
const _: () = assert!(
    ACCEPTED_SCHEMA_VERSIONS[ACCEPTED_SCHEMA_VERSIONS.len() - 1] == REGISTRY_SCHEMA_VERSION as u64,
    "ACCEPTED_SCHEMA_VERSIONS upper bound must equal REGISTRY_SCHEMA_VERSION"
);

/// Load the registry rows as raw JSON values, reproducing Python
/// `registry.load_registry`:
///
/// - A missing file is an empty registry (`Ok(vec![])`), NOT an error.
/// - The rows live under the top-level `"agents"` key (Python `write_registry`);
///   `"entries"` is accepted as a fallback for a registry last written by the
///   Rust daemon's `state::update_registry` (which serializes that key).
/// - Malformed JSON / non-object top-level / unknown `schema_version` /
///   non-list agents / non-object row / unknown provider / unknown status all
///   map to `Err` (Python's `RegistryVersionError`), which callers translate to
///   their verb-specific exit code (attach/trace 12, resume 13).
///
/// Raw `Value` access (not the strict typed `RegistryEntry`) mirrors Python's
/// duck-typed `getattr`/`row.get` so extra/missing optional fields behave the
/// same across the two implementations.
fn load_registry_entries(registry_path: &Path) -> Result<Vec<Value>, String> {
    let bytes = match fs::read(registry_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("registry read failed: {e}")),
    };
    // Strict UTF-8: Python reads the registry with encoding="utf-8" (no
    // replacement), so invalid bytes are a registry error, not silently mangled
    // content the verbs then operate on (codex P2). (The trace events.jsonl read
    // stays lossy on purpose -- Python uses errors="replace" there.)
    let text =
        std::str::from_utf8(&bytes).map_err(|e| format!("registry is not valid UTF-8: {e}"))?;
    let raw: Value =
        serde_json::from_str(text).map_err(|e| format!("registry is malformed JSON: {e}"))?;
    let obj = raw
        .as_object()
        .ok_or_else(|| "registry top-level is not a JSON object".to_string())?;
    match obj.get("schema_version").and_then(Value::as_u64) {
        Some(v) if ACCEPTED_SCHEMA_VERSIONS.contains(&v) => {}
        other => {
            return Err(format!(
            "registry has schema_version={other:?}; this fno understands {REGISTRY_SCHEMA_VERSION}"
        ))
        }
    }
    let agents = obj.get("agents").or_else(|| obj.get("entries"));
    let rows = match agents {
        None => return Ok(Vec::new()),
        Some(Value::Array(rows)) => rows,
        Some(_) => return Err("registry 'agents' field is not a list".to_string()),
    };
    for (i, row) in rows.iter().enumerate() {
        let row = row
            .as_object()
            .ok_or_else(|| format!("registry row {i} is not a JSON object"))?;
        // Identity is one axis (x-8dfc): tolerate ANY well-shaped identity
        // token (provider OR harness) so a single alien-harness row never
        // bricks the shared read; capability is gated at the spawn seam. The
        // corruption guard survives as the shape check.
        let provider = row.get("provider").and_then(Value::as_str);
        let harness = row.get("harness").and_then(Value::as_str);
        if !(is_identity_token(provider) || is_identity_token(harness)) {
            return Err(format!(
                "registry row {i} has no valid identity token (provider={provider:?}, harness={harness:?})"
            ));
        }
        // Divergence is loud, not fatal (x-8dfc), mirroring Python: a writer bug
        // stamping provider != harness surfaces in the skew window; harness wins.
        if is_identity_token(provider) && is_identity_token(harness) && provider != harness {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            eprintln!(
                "fno agents: warning: registry row {name:?} has provider={provider:?} and harness={harness:?} (diverged); harness wins for identity"
            );
        }
        let status = row.get("status").and_then(Value::as_str).unwrap_or("live");
        if !KNOWN_STATUSES.contains(&status) {
            return Err(format!("registry row {i} has status={status:?}"));
        }
        // Required-field presence, mirroring Python `AgentEntry(**row)` (codex P2):
        // a row missing a no-default field (name/cwd/log_path) raises
        // TypeError -> RegistryVersionError, not a later "agent not found" / "no
        // cwd". `provider` left OFF this list (x-8dfc): a provider-less post-v10
        // row backfills provider <- harness below, so identity is enforced by the
        // shape check above, not by provider presence. Presence only (a null
        // value is a value), matching the dataclass.
        for required in ["name", "cwd", "log_path"] {
            if !row.contains_key(required) {
                return Err(format!(
                    "registry row {i} missing required field '{required}'"
                ));
            }
        }
    }
    // v9 transport-key backfill (x-1b1e), the raw-Value mirror of Python
    // `load_registry` popping `claude_short_id` into `short_id`: a legacy row's
    // jobId moves into an empty `short_id` and the old key is dropped so no verb
    // body reads it. A conflicting pair keeps `short_id` and warns once.
    let mut out = rows.clone();
    for row in &mut out {
        if let Some(obj) = row.as_object_mut() {
            // Lockstep alias heal (x-8dfc), mirroring Python `load_registry`:
            // the two identity fields are the same token in the skew window, so
            // heal whichever is missing OR corrupt (shape-checked, not truthy)
            // from the valid sibling. Both directions, because resume reads
            // through this same healed value -- a truthy-corrupt harness would
            // otherwise resolve session_id to None. The gate above guarantees at
            // least one field is a valid token.
            let provider_valid = is_identity_token(obj.get("provider").and_then(Value::as_str));
            let harness_valid = is_identity_token(obj.get("harness").and_then(Value::as_str));
            if !provider_valid && harness_valid {
                if let Some(h) = obj
                    .get("harness")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    obj.insert("provider".into(), Value::String(h));
                }
            } else if !harness_valid && provider_valid {
                if let Some(p) = obj
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    obj.insert("harness".into(), Value::String(p));
                }
            }
            // v10 (x-880e) accept-on-read, the raw-Value mirror of Python
            // sync_harness_aliases: back-fill harness_session_id from a legacy
            // row's harness-matching per-provider session key so the resume /
            // resolver path sees the canonical id on a v1..=v9 row too.
            let hsid_empty = obj
                .get("harness_session_id")
                .and_then(Value::as_str)
                .map(str::is_empty)
                .unwrap_or(true);
            if hsid_empty {
                let legacy_key = match obj.get("harness").and_then(Value::as_str) {
                    Some("claude") => Some("claude_session_uuid"),
                    Some("codex") => Some("codex_session_id"),
                    Some("gemini") => Some("gemini_session_id"),
                    _ => None,
                };
                if let Some(k) = legacy_key {
                    if let Some(v) = obj
                        .get(k)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty() && *s != "null")
                        .map(str::to_string)
                    {
                        obj.insert("harness_session_id".into(), Value::String(v));
                    }
                }
            }
            let legacy = obj
                .remove("claude_short_id")
                .and_then(|v| v.as_str().map(str::to_string))
                .filter(|s| !s.is_empty());
            if let Some(legacy) = legacy {
                let existing = obj
                    .get("short_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                match existing {
                    None => {
                        obj.insert("short_id".into(), Value::String(legacy));
                    }
                    Some(short) if short != legacy => {
                        let name = obj.get("name").and_then(Value::as_str).unwrap_or("?");
                        eprintln!(
                            "fno agents: warning: registry row {name:?} carries short_id={short:?} and legacy claude_short_id={legacy:?}; keeping short_id"
                        );
                    }
                    Some(_) => {}
                }
            }
        }
    }
    Ok(out)
}

/// String field accessor with a default (Python `ev.get(key, "")`).
fn ev_str<'a>(ev: &'a Value, key: &str) -> &'a str {
    ev.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Python `filtered[:limit]` slice semantics, including negative limits
/// (`list[:-n]` drops the last n; `list[:0]` is empty).
fn slice_limit<T>(mut v: Vec<T>, limit: i64) -> Vec<T> {
    let len = v.len() as i64;
    let take = if limit < 0 {
        (len + limit).max(0)
    } else {
        limit.min(len)
    };
    v.truncate(take as usize);
    v
}

/// Outcome of the pure trace pipeline (mirrors Python `TraceResult`).
struct TraceResult {
    exit_code: i32,
    output: String,
    stderr: String,
}

/// Parsed `trace` flags.
struct TraceArgs {
    name: Option<String>,
    request_id: Option<String>,
    all_agents: bool,
    json_out: bool,
    limit: i64,
    since: Option<String>,
}

fn parse_trace_args(rest: &[String]) -> Result<TraceArgs, String> {
    let mut a = TraceArgs {
        name: None,
        request_id: None,
        all_agents: false,
        json_out: false,
        limit: 200,
        since: None,
    };
    let rest = expand_eq(rest);
    let mut it = rest.iter().cloned().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // ab-3ff64151: -A/-J are the global-register shorts; mirror the
            // Python typer.Option aliases so Rust-routed `trace` honors them.
            "--all" | "-A" => a.all_agents = true,
            "--json" | "-J" => a.json_out = true,
            "--request-id" => {
                a.request_id = Some(it.next().ok_or("--request-id needs a value")?);
            }
            "--since" => {
                a.since = Some(it.next().ok_or("--since needs a value")?);
            }
            "--limit" => {
                let v = it.next().ok_or("--limit needs a value")?;
                a.limit = v
                    .parse::<i64>()
                    .map_err(|_| format!("--limit needs an integer (got: {v})"))?;
            }
            other if other.starts_with("--") => {
                return Err(format!("fno-agents: unknown trace flag: {other}"));
            }
            positional => {
                if a.name.is_some() {
                    return Err(format!(
                        "fno-agents: trace takes one NAME (got extra: {positional})"
                    ));
                }
                a.name = Some(positional.to_string());
            }
        }
    }
    Ok(a)
}

/// The pure trace pipeline (no I/O side effects on stdout/stderr); the CLI
/// dispatch writes `output`/`stderr` and uses `exit_code`. Mirrors Python
/// `trace_logic` step for step.
fn trace_logic(args: &TraceArgs, events_path: &Path, registry_path: &Path) -> TraceResult {
    // name required unless --all.
    if args.name.is_none() && !args.all_agents {
        return TraceResult {
            exit_code: 2,
            output: String::new(),
            stderr: "fno agents trace: agent NAME is required unless --all is set\n".to_string(),
        };
    }

    // Parse --since; on failure, warn and fall back to raw-string compare.
    let mut since_dt = None;
    let mut since_warn = String::new();
    if let Some(since) = &args.since {
        match parse_iso8601(since) {
            Some(dt) => since_dt = Some(dt),
            None => {
                since_warn = format!(
                    "fno agents trace: warn: --since '{since}' did not parse as ISO8601; falling back to raw-string compare\n"
                );
            }
        }
    }

    // Registry membership gate (unless --all). Resolve the token (name | short |
    // full id, x-1b1e) to its canonical name so events - which key on the name -
    // filter correctly regardless of the address form the caller used.
    let mut resolved_name: Option<String> = args.name.clone();
    if let Some(token) = &args.name {
        if !args.all_agents {
            match load_registry_entries(registry_path) {
                Err(exc) => {
                    return TraceResult {
                        exit_code: 12,
                        output: String::new(),
                        stderr: format!("fno agents trace: registry load failed: {exc}\n"),
                    };
                }
                Ok(rows) => match find_agent_entry(&rows, token) {
                    Ok(e) => {
                        resolved_name = Some(
                            e.get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(token)
                                .to_string(),
                        );
                    }
                    Err(_) => {
                        return TraceResult {
                            exit_code: 13,
                            output: String::new(),
                            stderr: format!(
                                "fno agents trace: agent '{token}' not found in registry\n"
                            ),
                        };
                    }
                },
            }
        }
    }

    let (events, malformed) = read_jsonl(events_path);

    // Filter: by name (unless --all), by request_id, by since.
    let matches = |ev: &Value| -> bool {
        if !args.all_agents {
            if let Some(name) = &resolved_name {
                let recipient = ev
                    .get("to_name")
                    .and_then(Value::as_str)
                    .or_else(|| ev.get("name").and_then(Value::as_str));
                if recipient != Some(name.as_str()) {
                    return false;
                }
            }
        }
        if let Some(rid) = &args.request_id {
            if ev.get("request_id").and_then(Value::as_str) != Some(rid.as_str()) {
                return false;
            }
        }
        if let Some(since) = &args.since {
            let ts = ev_str(ev, "ts");
            match &since_dt {
                Some(sdt) => {
                    // Datetime compare; an unparseable event ts is kept (degrade-open).
                    if let Some(edt) = parse_iso8601(ts) {
                        if edt < *sdt {
                            return false;
                        }
                    }
                }
                None => {
                    // Raw-string fallback.
                    if ts < since.as_str() {
                        return false;
                    }
                }
            }
        }
        true
    };

    let mut filtered: Vec<(String, Value)> =
        events.into_iter().filter(|(_, ev)| matches(ev)).collect();
    // Stable sort ascending by ts string (matches Python's key=ts sort).
    filtered.sort_by(|(_, a), (_, b)| ev_str(a, "ts").cmp(ev_str(b, "ts")));

    // Orphan detection over the FULL filtered set, BEFORE the limit (human only).
    let mut orphan_rids: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !args.json_out {
        let seen_done: std::collections::HashSet<&str> = filtered
            .iter()
            .filter_map(|(_, e)| {
                let kind = ev_str(e, "kind");
                let rid = e.get("request_id").and_then(Value::as_str);
                if kind.ends_with("_done") {
                    rid
                } else {
                    None
                }
            })
            .collect();
        for (_, e) in &filtered {
            let kind = ev_str(e, "kind");
            if let Some(rid) = e.get("request_id").and_then(Value::as_str) {
                if kind.ends_with("_started") && !seen_done.contains(rid) {
                    orphan_rids.insert(rid.to_string());
                }
            }
        }
    }

    // Apply limit after sort + orphan detection.
    let filtered = slice_limit(filtered, args.limit);

    let malformed_warn = |buf: &mut String| {
        if malformed > 0 {
            buf.push_str(&format!(
                "fno agents trace: skipped {malformed} malformed line(s) in {}\n",
                events_path.display()
            ));
        }
    };

    if filtered.is_empty() {
        let mut err = since_warn.clone();
        malformed_warn(&mut err);
        return TraceResult {
            exit_code: 0,
            output: "no events yet\n".to_string(),
            stderr: err,
        };
    }

    let mut lines: Vec<String> = Vec::new();

    // Synthesize the target_session header (human only).
    if !args.json_out {
        let mut rsids: Vec<&str> = filtered
            .iter()
            .filter_map(|(_, e)| e.get("target_session_id").and_then(Value::as_str))
            .collect();
        rsids.sort_unstable();
        rsids.dedup();
        if !rsids.is_empty() {
            lines.push(format!("target_session: {}", rsids.join(", ")));
        }
    }

    for (raw, ev) in &filtered {
        if args.json_out {
            // events.jsonl lines are compact; emit verbatim to preserve source
            // key order (Python's `json.dumps(ev, sort_keys=False, compact)`).
            lines.push(raw.clone());
        } else {
            let ts = ev_str(ev, "ts");
            let kind = ev_str(ev, "kind");
            let recipient = ev
                .get("to_name")
                .and_then(Value::as_str)
                .or_else(|| ev.get("name").and_then(Value::as_str))
                .unwrap_or("?");
            let sender = ev.get("from_name").and_then(Value::as_str).unwrap_or("?");
            let rid_full = ev.get("request_id").and_then(Value::as_str).unwrap_or("");
            let rid = if rid_full.is_empty() {
                String::new()
            } else {
                rid_full.chars().take(REQUEST_ID_PREFIX_LEN).collect()
            };
            let ck = ev.get("caller_kind").and_then(Value::as_str).unwrap_or("-");
            lines.push(format!(
                "{ts}  {kind}  {sender} -> {recipient}  rid={rid}  caller={ck}"
            ));
            if kind.ends_with("_started") && orphan_rids.contains(rid_full) {
                lines.push(ORPHAN_MARKER.to_string());
            }
        }
    }

    let mut err = since_warn.clone();
    malformed_warn(&mut err);
    TraceResult {
        exit_code: 0,
        output: lines.join("\n") + "\n",
        stderr: err,
    }
}

/// `fno-agents trace [name] [--all] [--request-id X] [--json] [--limit N] [--since S]`.
pub fn run_trace(rest: &[String], home: &AgentsHome) -> i32 {
    let args = match parse_trace_args(rest) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };
    let events_path = trace_events_path(home);
    let registry_path = home.registry_json();
    let result = trace_logic(&args, &events_path, &registry_path);
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    if !result.output.is_empty() {
        print!("{}", result.output);
    }
    result.exit_code
}

// ---------------------------------------------------------------------------
// Shared helpers for the subprocess-exec verbs (attach, resume).
// ---------------------------------------------------------------------------

/// Harness -> session-id registry field, mirroring Python
/// `registry.HARNESS_SESSION_ID_FIELDS`. claude resolves to the unified `short_id`
/// transport key (the jobId); v10 (x-880e) resolves codex/gemini to the canonical
/// `harness_session_id` (their per-provider fields are gone -- load_registry_entries
/// back-fills it from a legacy row's per-provider key). This is the ONLY place a
/// verb touches a session-id field; every session-connecting verb reaches a row via
/// [`find_agent_entry`] instead of its own name-only `.find`.
fn session_id_field(harness: &str) -> Option<&'static str> {
    match harness {
        "claude" => Some("short_id"),
        "codex" | "gemini" => Some("harness_session_id"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared identifier resolver (x-1b1e): the Rust mirror of Python
// `registry.resolve_agent`. Every session-connecting verb (resume, attach,
// logs, trace) resolves a token to one row through this, so a session is
// addressable by name/slug, full harness_session_id, or an 8-hex short. Same
// four-rule precedence + ambiguity semantics as the Python resolver; the US4
// parity matrix asserts the two agree.
// ---------------------------------------------------------------------------

const ACCEPTED_FORMS_MSG: &str = "accepted forms: name, 8-hex short id, or full session id";

/// A resolution failure. Verbs map these to their own exit codes (resume/logs
/// 13, attach 2) and never see a panic.
#[derive(Debug)]
pub(crate) enum ResolveError {
    /// The token matched nothing; carries the token (empty when blank input).
    NotFound(String),
    /// Two or more distinct rows matched the same tier; carries the candidate list.
    Ambiguous(String),
}

impl ResolveError {
    /// The one-line message a verb prints (prefix it with its own verb name).
    fn message(&self) -> String {
        match self {
            ResolveError::NotFound(tok) if tok.is_empty() => {
                format!("empty agent token; {ACCEPTED_FORMS_MSG}")
            }
            ResolveError::NotFound(tok) => {
                format!(
                    "no agent matching {}; {ACCEPTED_FORMS_MSG}",
                    py_repr_str(tok)
                )
            }
            ResolveError::Ambiguous(msg) => msg.clone(),
        }
    }
}

/// The canonical full session id, lowercased (x-880e: the per-provider full-id
/// fields are gone; harness_session_id -- back-filled from a legacy row's
/// per-provider key in `load_registry_entries` -- is their single successor).
fn full_session_ids(entry: &Value) -> Vec<String> {
    entry
        .get("harness_session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_ascii_lowercase()])
        .unwrap_or_default()
}

/// The derived canonical short: the first hex group of `harness_session_id`
/// when it is exactly 8 hex (claude's jobId is built the same way). `None` for a
/// row whose id is unresolved or non-hex (e.g. an opencode `ses_...` id).
fn derived_short(entry: &Value) -> Option<String> {
    let hsid = entry.get("harness_session_id").and_then(Value::as_str)?;
    let lead = hsid.split('-').next()?.to_ascii_lowercase();
    (lead.len() == 8 && lead.bytes().all(|b| b.is_ascii_hexdigit())).then_some(lead)
}

/// Return the single matched row, or an ambiguity error. Dedups by `name` (the
/// PK), so the SAME row matching a tier via multiple rules is not ambiguous.
fn one_or_ambiguous<'a>(hits: Vec<&'a Value>, token: &str) -> Result<&'a Value, ResolveError> {
    let mut by_name: std::collections::BTreeMap<&str, &'a Value> =
        std::collections::BTreeMap::new();
    for e in hits {
        let n = e.get("name").and_then(Value::as_str).unwrap_or("?");
        by_name.entry(n).or_insert(e);
    }
    if by_name.len() > 1 {
        let cands = by_name
            .values()
            .map(|e| {
                let n = e.get("name").and_then(Value::as_str).unwrap_or("?");
                let s = e
                    .get("short_id")
                    .and_then(Value::as_str)
                    .filter(|x| !x.is_empty())
                    .unwrap_or("-");
                let p = e.get("provider").and_then(Value::as_str).unwrap_or("?");
                format!("{n} (short={s}, {p})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ResolveError::Ambiguous(format!(
            "token {} is ambiguous across {} agents: {cands}. Disambiguate with the name or full session id.",
            py_repr_str(token),
            by_name.len()
        )));
    }
    Ok(*by_name.values().next().unwrap())
}

/// Resolve `token` (name | full harness_session_id | 8-hex short) to one row.
/// Precedence: exact name, exact full session id (case-insensitive), exact
/// stored short_id (shape-agnostic), derived 8-hex prefix. Name wins first so a
/// hex-shaped name is byte-stable. Mirrors Python `resolve_agent`.
pub(crate) fn find_agent_entry<'a>(
    rows: &'a [Value],
    token: &str,
) -> Result<&'a Value, ResolveError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(ResolveError::NotFound(String::new()));
    }
    let low = token.to_ascii_lowercase();

    let named: Vec<&Value> = rows
        .iter()
        .filter(|e| e.get("name").and_then(Value::as_str) == Some(token))
        .collect();
    if !named.is_empty() {
        return one_or_ambiguous(named, token);
    }

    let by_full: Vec<&Value> = rows
        .iter()
        .filter(|e| full_session_ids(e).iter().any(|i| i == &low))
        .collect();
    if !by_full.is_empty() {
        return one_or_ambiguous(by_full, token);
    }

    let by_short: Vec<&Value> = rows
        .iter()
        .filter(|e| matches!(e.get("short_id").and_then(Value::as_str), Some(s) if !s.is_empty() && s == token))
        .collect();
    if !by_short.is_empty() {
        return one_or_ambiguous(by_short, token);
    }

    if low.len() == 8 && low.bytes().all(|b| b.is_ascii_hexdigit()) {
        let by_derived: Vec<&Value> = rows
            .iter()
            .filter(|e| derived_short(e).as_deref() == Some(low.as_str()))
            .collect();
        if !by_derived.is_empty() {
            return one_or_ambiguous(by_derived, token);
        }
    }

    Err(ResolveError::NotFound(token.to_string()))
}

/// Provider-specific resume argv, mirroring Python `_build_resume_argv`.
/// Returns `None` for unsupported providers.
fn build_resume_argv(provider: &str, session_id: &str) -> Option<Vec<String>> {
    match provider {
        "codex" => Some(vec!["codex".into(), "resume".into(), session_id.into()]),
        "claude" => Some(vec!["claude".into(), "attach".into(), session_id.into()]),
        "gemini" => Some(vec!["gemini".into(), "--resume".into(), session_id.into()]),
        _ => None,
    }
}

/// True iff `s` is a lowercase `8-4-4-4-12` hex UUID (the shape `claude --resume`
/// accepts). Guards the dead-arm argv so a malformed/empty recorded uuid can
/// never reach `claude --resume` (x-9844 Failure Modes / Boundaries).
fn is_uuid_shaped(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == groups.len()
        && parts.iter().zip(groups).all(|(p, n)| {
            p.len() == n
                && p.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        })
}

/// The claude arm of `resume` (x-9844 Fix 1): liveness-probe first, then pick the
/// argv. A live (incl. idle) supervisor -> `claude attach <short_id>` (today's
/// behavior); a dead/absent one -> `claude --resume <uuid>` in the recorded cwd.
/// Probe reality (locate_session + a 250 ms socket connect), never the registry
/// `status` field: a stale-exited row whose supervisor is actually alive must
/// attach, not `--resume` into a second writer on one transcript. The chosen lane
/// is printed to stderr before returning so the operator always knows which
/// fired. `Err(code)` carries the exit code for the uuid-absent refusal.
/// Returns `(argv, claim_uuid)`. `claim_uuid` is `Some(uuid)` only for the
/// dead-arm (`claude --resume`), which the caller must guard with the
/// `session:<uuid>` single-writer claim before exec; the live attach arm returns
/// `None` (claude's own supervisor owns attach safety).
fn claude_resume_argv(
    claude_home: &ClaudeHome,
    entry: &Value,
    name: &str,
) -> Result<(Vec<String>, Option<String>), i32> {
    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    let uuid = entry
        .get("claude_session_uuid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    let live = !short_id.is_empty()
        && locate_session(claude_home, short_id)
            .map(|loc| liveness_probe(&loc.messaging_socket_path))
            .unwrap_or(false);

    if live {
        eprintln!("fno agents resume: {name} is live - attaching");
        Ok((
            vec!["claude".into(), "attach".into(), short_id.into()],
            None,
        ))
    } else if is_uuid_shaped(uuid) {
        eprintln!("fno agents resume: {name} has exited - resuming in your terminal");
        Ok((
            vec!["claude".into(), "--resume".into(), uuid.into()],
            Some(uuid.to_string()),
        ))
    } else {
        eprintln!(
            "fno agents resume: {} has no claude session recorded; nothing to resume.",
            py_repr_str(name)
        );
        Err(13)
    }
}

/// Acquire the `session:<uuid>` single-writer claim for an interactive dead-row
/// resume, anchored to THIS process. `exec` keeps the pid, so the claim is held
/// by the resumed claude and self-releases when the operator quits (no explicit
/// release). Two racing resumers both probe dead, but only one wins this atomic
/// claim; the loser gets `Err` and refuses instead of opening a second writer on
/// one transcript - the residual double-writer window the liveness probe alone
/// cannot close. `root` is `None` in prod (session: keys route to
/// `$FNO_CLAIMS_ROOT`/`$HOME`); tests inject a temp root.
fn acquire_resume_session_claim(uuid: &str, root: Option<&Path>) -> Result<(), (i32, String)> {
    use crate::claims::{acquire, AcquireOpts, AcquireOutcome};
    let holder = format!("resume:{}", std::process::id());
    let opts = AcquireOpts {
        root: root.map(Path::to_path_buf),
        reason: Some("interactive resume single-writer".to_string()),
        ..Default::default()
    };
    match acquire(&format!("session:{uuid}"), &holder, opts) {
        AcquireOutcome::Acquired(_) => Ok(()),
        AcquireOutcome::HeldByOther { holder, pid, host } => Err((
            11,
            format!(
                "fno agents resume: session {uuid} is held live by another writer \
                 ({holder}, pid={pid}, host={host}); not opening a second writer on one transcript."
            ),
        )),
        AcquireOutcome::Error(e) => Err((
            12,
            format!("fno agents resume: could not claim session {uuid}: {e}"),
        )),
    }
}

/// The dead-row pointer for `attach` (x-9844 Fix 2): `Some(message)` when `entry`
/// is a claude row whose supervisor is gone (probe says dead) AND a well-shaped
/// session uuid is recorded - the two revival commands to print instead of
/// dead-ending in claude's own "session not found". `None` when the row is live
/// (fall through to a normal attach) or carries no revivable uuid (nothing to
/// point at - never print an unusable command). Probes reality (locate_session +
/// socket), never the registry `status` field, matching the resume smart verb.
fn claude_attach_pointer(claude_home: &ClaudeHome, entry: &Value, name: &str) -> Option<String> {
    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    let uuid = entry
        .get("claude_session_uuid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if short_id.is_empty() || !is_uuid_shaped(uuid) {
        return None;
    }
    let live = locate_session(claude_home, short_id)
        .map(|loc| liveness_probe(&loc.messaging_socket_path))
        .unwrap_or(false);
    if live {
        return None;
    }
    Some(format!(
        "{name} has exited - fno agents resume {name} (continue it in your terminal)\n\
         or: fno agents spawn {name} --resume {uuid} --substrate bg (detached worker)"
    ))
}

/// POSIX shell quoting matching Python's `shlex.quote`: empty -> `''`; a string
/// of only "safe" chars (`[\w@%+=:,./-]`) is returned as-is; otherwise it is
/// single-quoted with embedded `'` escaped as `'"'"'`.
fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// Python `repr()` of a string: single-quoted, switching to double quotes when
/// the value contains `'` but not `"` (matching CPython). Escapes the backslash,
/// the active quote, and `\t`/`\n`/`\r`; ASCII C0 controls, DEL, and C1 controls
/// are emitted as `\xXX` (lowercase hex), matching CPython for those code points.
///
/// Full `unicodedata` printability for higher code points is not replicated:
/// printable non-ASCII (e.g. accented letters) stays literal, which is correct
/// for every realistic agent name / cwd / short-id input. The rare divergence is
/// non-ASCII code points that are non-printable above the C1 range (cv-b6bd4bf4).
fn py_repr_str(s: &str) -> String {
    let has_single = s.contains('\'');
    let has_double = s.contains('"');
    let quote = if has_single && !has_double { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let cp = c as u32;
        match c {
            '\\' => out.push_str("\\\\"),
            _ if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if cp < 0x20 || cp == 0x7f || (0x80..=0x9f).contains(&cp) => {
                // cp <= 0x9f here, so both nibbles are valid hex digits.
                // char::from_digit avoids a per-char format! allocation
                // (gemini-code-assist medium on PR #367).
                out.push_str("\\x");
                out.push(char::from_digit((cp >> 4) & 0xf, 16).unwrap());
                out.push(char::from_digit(cp & 0xf, 16).unwrap());
            }
            _ => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `shutil.which`-style PATH lookup: true iff `name` resolves to an executable
/// regular file (an absolute/relative path with a separator is checked directly;
/// otherwise each `$PATH` entry is probed).
fn which_on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| -> bool {
        match fs::metadata(p) {
            Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
            Err(_) => false,
        }
    };
    if name.contains('/') {
        return is_exec(Path::new(name));
    }
    // PATH-unset fallback uses CPython's os.defpath directories (`/bin:/usr/bin`
    // on POSIX) but deliberately DROPS os.defpath's leading empty entry, which
    // would resolve to the cwd: searching the cwd for an executable is an
    // untrusted-search-path risk (CWE-426) when run from an attacker-writable
    // directory. The dirs/order match os.defpath; only the cwd entry is omitted
    // (cv-b6bd4bf4; gemini-code-assist security-high on PR #367).
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
    std::env::split_paths(&path).any(|dir| is_exec(&dir.join(name)))
}

/// Append one event line to `state_dir/events.jsonl` with the Python-agents
/// envelope (`{...fields, ts, kind}`, compact). Best-effort: on a write error
/// it warns to stderr and returns, mirroring `agents.events.emit` so a failed
/// telemetry write never blocks the primary command (AC1-FR).
///
/// Deliberately a free function (not a `.emit()` method) so the crate's
/// production-emit-kind scanner (which keys on `.emit(`/`.emit_fields(`) does
/// not treat these Python-side audit kinds as Rust daemon event kinds.
fn append_agents_event(events_path: &Path, kind: &str, fields: &[(&str, Value)]) {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}:{}",
                serde_json::to_string(k).unwrap_or_default(),
                serde_json::to_string(v).unwrap_or_default()
            )
        })
        .collect();
    parts.push(format!(
        "\"ts\":{}",
        serde_json::to_string(&ts).unwrap_or_default()
    ));
    parts.push(format!(
        "\"kind\":{}",
        serde_json::to_string(kind).unwrap_or_default()
    ));
    let line = format!("{{{}}}\n", parts.join(","));

    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = events_path.parent() {
            fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut fh = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path)?;
        fh.write_all(line.as_bytes())
    })();
    if let Err(exc) = result {
        eprintln!(
            "fno agents: warning: events.emit('{kind}') to {}: {exc}",
            events_path.display()
        );
    }
}

/// Read the registry rows for the subprocess-exec verbs. Thin alias over
/// [`load_registry_entries`] (the validation + `"agents"`/`"entries"` key
/// handling lives there) so resume/attach/logs and trace share one reader.
fn read_registry_entries(path: &Path) -> Result<Vec<Value>, String> {
    load_registry_entries(path)
}

// ---------------------------------------------------------------------------
// resume
// ---------------------------------------------------------------------------

/// `fno-agents resume <name> [--print-command]` -- resume an agent in its
/// recorded cwd via the provider's resume CLI (`os.execvp` equivalent), or
/// print the shell snippet with `--print-command`. Mirrors Python `resume_logic`.
pub fn run_resume(rest: &[String], home: &AgentsHome) -> i32 {
    let mut name: Option<String> = None;
    let mut print_command = false;
    for a in rest {
        match a.as_str() {
            "--print-command" => print_command = true,
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown resume flag: {other}");
                return 2;
            }
            other => {
                if name.is_some() {
                    eprintln!("fno-agents: resume takes one NAME (got extra: {other})");
                    return 2;
                }
                name = Some(other.to_string());
            }
        }
    }
    let name = match name {
        Some(n) => n,
        None => {
            eprintln!("fno-agents: resume needs a <name>");
            return 2;
        }
    };

    let entries = match read_registry_entries(&home.registry_json()) {
        Ok(e) => e,
        Err(exc) => {
            eprintln!("fno agents resume: registry read failed: {exc}");
            return 13;
        }
    };
    let entry = match find_agent_entry(&entries, &name) {
        Ok(e) => e,
        Err(err) => {
            eprintln!(
                "fno agents resume: {}. Use `fno agents list` to see registered agents.",
                err.message()
            );
            return 13;
        }
    };

    // Identity is one axis (x-8dfc): resume keys on harness (provider fallback
    // for a not-yet-backfilled row), and the exit-13 errors name the harness,
    // matching Python resume_cli. harness == provider on every current row.
    let harness = entry
        .get("harness")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| entry.get("provider").and_then(Value::as_str))
        .unwrap_or("");
    let cwd = entry.get("cwd").and_then(Value::as_str).unwrap_or("");
    let session_id = session_id_field(harness)
        .and_then(|f| entry.get(f))
        .and_then(Value::as_str)
        .unwrap_or("");

    if cwd.is_empty() {
        eprintln!(
            "fno agents resume: agent {} has no recorded cwd. Run `fno agents rm {}` to clean up.",
            py_repr_str(&name),
            name
        );
        return 13;
    }

    // claude gets the liveness-probed smart fork (US1/US2, x-9844): a live
    // (incl. idle) supervisor -> attach; a dead/absent one -> `claude --resume
    // <uuid>`. Other harnesses keep their settled-session resume CLI, checked
    // BEFORE session_id so an unknown harness surfaces "not supported" rather
    // than a misleading "no session_id".
    let (argv, claim_uuid) = if harness == "claude" {
        match claude_resume_argv(&ClaudeHome::from_env(), entry, &name) {
            Ok(plan) => plan,
            Err(code) => return code,
        }
    } else {
        let v = match build_resume_argv(harness, session_id) {
            Some(v) => v,
            None => {
                eprintln!(
                    "fno agents resume: harness {} resume not supported by this fno version.",
                    py_repr_str(harness)
                );
                return 13;
            }
        };
        if session_id.is_empty() {
            eprintln!(
                "fno agents resume: agent {} has no recorded session_id for harness {}.",
                py_repr_str(&name),
                py_repr_str(harness)
            );
            return 13;
        }
        (v, None)
    };

    if !which_on_path(&argv[0]) {
        eprintln!("fno agents resume: {} CLI not on PATH", argv[0]);
        return 14;
    }

    if print_command {
        let argv_q = argv
            .iter()
            .map(|a| shlex_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        println!("cd {} && exec {}", shlex_quote(cwd), argv_q);
        return 0;
    }

    // Guard a dead-row `claude --resume` with the session single-writer claim
    // before exec (--print-command already returned above, so it never claims).
    // exec keeps this pid, so the claim is held by the resumed claude and
    // self-releases when the operator quits.
    if let Some(uuid) = &claim_uuid {
        if let Err((code, msg)) = acquire_resume_session_claim(uuid, None) {
            eprintln!("{msg}");
            return code;
        }
    }

    // chdir BEFORE the emit so a stale cwd surfaces as exit 13 rather than a
    // misleading "agent_resumed" event followed by a failed exec.
    if let Err(exc) = std::env::set_current_dir(cwd) {
        eprintln!(
            "fno agents resume: cwd {} for agent {} is no longer reachable: {exc}. Run `fno agents rm {}` to clean up.",
            py_repr_str(cwd),
            py_repr_str(&name),
            name
        );
        return 13;
    }

    // Best-effort agent_resumed emit (AC1-FR): a failure warns but does not
    // block the irreversible exec.
    let events_path = trace_events_path(home);
    append_agents_event(
        &events_path,
        "agent_resumed",
        &[
            ("name", Value::String(name.clone())),
            // Event field key stays "provider" (schema parity with Python's
            // emit); the value is the resolved harness (== provider) (x-8dfc).
            ("provider", Value::String(harness.to_string())),
            ("session_id", Value::String(session_id.to_string())),
            ("cwd", Value::String(cwd.to_string())),
        ],
    );

    // Replace the process with the provider CLI (os.execvp equivalent).
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    // exec only returns on failure.
    eprintln!("fno agents resume: failed to exec {}: {err}", argv[0]);
    1
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

/// Reproduce `_validate_lifecycle_name`: returns `Err((exit, message))` on a
/// rejected name (the message is printed to stderr with a trailing newline).
fn validate_lifecycle_name(name: &str) -> Result<(), (i32, String)> {
    if name.is_empty() {
        return Err((2, "agent name must not be empty".to_string()));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err((
            2,
            format!(
                "agent name must not contain path separators or '..': {}",
                py_repr_str(name)
            ),
        ));
    }
    if name.chars().count() > 128 {
        return Err((
            2,
            format!("name must be <=128 chars (got {})", name.chars().count()),
        ));
    }
    Ok(())
}

/// `fno-agents attach <name>` -- interactive attach to a running claude agent
/// (codex/gemini are refused). Mirrors Python `dispatch.attach_agent` + the
/// `cmd_attach` Typer wrapper.
pub fn run_attach(rest: &[String], home: &AgentsHome) -> i32 {
    let mut name: Option<String> = None;
    for a in rest {
        match a.as_str() {
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown attach flag: {other}");
                return 2;
            }
            other => {
                if name.is_some() {
                    eprintln!("fno-agents: attach takes one NAME (got extra: {other})");
                    return 2;
                }
                name = Some(other.to_string());
            }
        }
    }
    let name = match name {
        Some(n) => n,
        None => {
            eprintln!("fno-agents: attach needs a <name>");
            return 2;
        }
    };

    if let Err((code, msg)) = validate_lifecycle_name(&name) {
        eprintln!("{msg}");
        return code;
    }

    let entries = match read_registry_entries(&home.registry_json()) {
        Ok(e) => e,
        Err(exc) => {
            eprintln!("registry read failed: {exc}");
            return 12;
        }
    };
    let entry = match find_agent_entry(&entries, &name) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}", err.message());
            return 2;
        }
    };

    let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("");
    let events_path = trace_events_path(home);

    // Every non-claude provider refuses attach (claude is the only provider
    // with a persistent `--bg` session to attach to). `!= "claude"` instead of
    // an allowlist so a provider added to the roster inherits the refusal
    // rather than falling through to a claude-shaped attach (x-51f6 US1).
    if provider != "claude" {
        eprintln!(
            "{provider} agents are one-shot; no persistent session to attach to. Use 'fno agents logs {name} --follow' for live output. Cross-provider attach is planned for the Phase 6 supervisor."
        );
        append_agents_event(
            &events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.clone())),
                ("provider", Value::String(provider.to_string())),
                (
                    "reason",
                    Value::String("one-shot-provider-no-persistent-session".to_string()),
                ),
            ],
        );
        return 13;
    }

    if provider != "claude" {
        eprintln!(
            "attach for provider {} is not implemented",
            py_repr_str(provider)
        );
        return 2;
    }

    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    if short_id.is_empty() {
        eprintln!(
            "registry entry {} has no short id on file; cannot attach.",
            py_repr_str(&name)
        );
        return 12;
    }

    // Attach stays live-only, but a dead claude row (supervisor gone) with a
    // recorded session uuid refuses with the exact revival commands instead of
    // dead-ending in claude's own "session not found" (US3). The decision is a
    // pure helper so it is testable without the exec path.
    if let Some(msg) = claude_attach_pointer(&ClaudeHome::from_env(), entry, &name) {
        eprintln!("{msg}");
        append_agents_event(
            &events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.clone())),
                ("provider", Value::String("claude".to_string())),
                (
                    "reason",
                    Value::String("exited-revivable-pointer".to_string()),
                ),
            ],
        );
        return 13;
    }

    if !which_on_path("claude") {
        eprintln!("claude CLI not on PATH");
        return 14;
    }

    // Inherit stdio so the claude TUI takes over; mirror its exit code.
    match std::process::Command::new("claude")
        .arg("attach")
        .arg(short_id)
        .status()
    {
        Ok(status) => {
            let exit_code = status.code().unwrap_or(1);
            append_agents_event(
                &events_path,
                "agent_attached",
                &[
                    ("name", Value::String(name.clone())),
                    ("provider", Value::String("claude".to_string())),
                    ("short_id", Value::String(short_id.to_string())),
                    ("claude_exit", Value::from(exit_code)),
                ],
            );
            exit_code
        }
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("claude CLI not on PATH");
            14
        }
        Err(exc) => {
            append_agents_event(
                &events_path,
                "agent_attached",
                &[
                    ("name", Value::String(name.clone())),
                    ("provider", Value::String("claude".to_string())),
                    ("short_id", Value::String(short_id.to_string())),
                    ("claude_exit", Value::Null),
                    ("error", Value::String(exc.to_string())),
                    ("error_type", Value::String("OSError".to_string())),
                ],
            );
            eprintln!("claude attach failed: {exc}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

/// Parsed `logs` flags.
struct LogsArgs {
    name: String,
    tail: i64,
    follow: bool,
    json_out: bool,
}

fn parse_logs_args(rest: &[String]) -> Result<LogsArgs, (i32, String)> {
    let mut name: Option<String> = None;
    let mut tail: i64 = 100;
    let mut follow = false;
    let mut json_out = false;
    let rest = expand_eq(rest);
    let mut it = rest.iter().cloned().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--follow" | "-f" => follow = true,
            "--json" | "-J" => json_out = true, // ab-3ff64151: global-register short
            "--tail" | "-n" => {
                let v = it.next().ok_or((2, "--tail needs a value".to_string()))?;
                tail = v
                    .parse::<i64>()
                    .map_err(|_| (2, format!("--tail needs an integer (got: {v})")))?;
            }
            // Attached short-option form `-n5` (Click/Typer accept it; codex P2).
            s if s.starts_with("-n") && s.len() > 2 => {
                let v = &s[2..];
                tail = v
                    .parse::<i64>()
                    .map_err(|_| (2, format!("--tail needs an integer (got: {v})")))?;
            }
            other
                if other.starts_with('-')
                    && other.len() > 1
                    && !other[1..].chars().next().unwrap().is_ascii_digit() =>
            {
                return Err((2, format!("fno-agents: unknown logs flag: {other}")));
            }
            positional => {
                if name.is_some() {
                    return Err((
                        2,
                        format!("fno-agents: logs takes one NAME (got extra: {positional})"),
                    ));
                }
                name = Some(positional.to_string());
            }
        }
    }
    let name = name.ok_or((2, "logs needs a <name>".to_string()))?;
    // cmd_logs: `--tail must be >= 0`.
    if tail < 0 {
        return Err((2, format!("--tail must be >= 0 (got {tail})")));
    }
    Ok(LogsArgs {
        name,
        tail,
        follow,
        json_out,
    })
}

/// Last `tail` lines of `path` with their line endings preserved, appending a
/// trailing newline to any line lacking one (mirrors Python `_read_jsonl_tail`
/// + `read_logs`'s write loop). `tail <= 0` yields no lines.
///
/// Reads line-by-line into a bounded ring of capacity `tail` rather than loading
/// the whole file, so memory stays O(tail) not O(file) -- matching Python's
/// `collections.deque(fh, maxlen=tail)` (codex P2: a full-file read OOMs on large
/// agent logs).
fn tail_lines_keepends(path: &Path, tail: i64) -> std::io::Result<String> {
    use std::collections::VecDeque;
    use std::io::BufRead;
    if tail <= 0 {
        return Ok(String::new());
    }
    let cap = tail as usize;
    let mut reader = std::io::BufReader::new(fs::File::open(path)?);
    let mut ring: VecDeque<String> = VecDeque::with_capacity(cap.min(1024));
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF
        }
        if ring.len() == cap {
            ring.pop_front();
        }
        ring.push_back(std::mem::take(&mut line));
    }
    let mut out = String::new();
    for l in &ring {
        out.push_str(l);
        if !l.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Slice the last `tail` lines (keepends) of an in-memory string, used for the
/// claude capture path (Python slices `result.stdout` the same way).
fn tail_lines_of_str(s: &str, tail: i64) -> String {
    if tail == 0 {
        return String::new();
    }
    if tail < 0 || s.is_empty() {
        return s.to_string();
    }
    let lines: Vec<&str> = s.split_inclusive('\n').collect();
    let start = lines.len().saturating_sub(tail as usize);
    lines[start..].concat()
}

/// `fno-agents logs <name> [--tail N] [--follow] [--json]`.
pub async fn run_logs(rest: &[String], home: &AgentsHome) -> i32 {
    let args = match parse_logs_args(rest) {
        Ok(a) => a,
        Err((code, msg)) => {
            eprintln!("{msg}");
            return code;
        }
    };

    let entries = match read_registry_entries(&home.registry_json()) {
        Ok(e) => e,
        Err(exc) => {
            // RegistryVersionError parity: exit 1 with a WARN line.
            eprintln!("WARN: {exc}");
            return 1;
        }
    };
    let entry = match find_agent_entry(&entries, &args.name) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}", err.message());
            return 13;
        }
    };
    let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("");

    if provider == "claude" {
        return run_logs_claude(entry, &args);
    }

    // codex / gemini: read the tee'd JSONL file. Retrieval IS implemented
    // (proven by test_logs_codex_oneshot_parity); the only failure left here is
    // a genuinely-absent log file, so report that honestly instead of the stale
    // "ships in Phase 3 US4" stub that made codex look unsupported (ab-65c3e60d).
    // Byte-parity with read.py's matching branch.
    let log_path = entry.get("log_path").and_then(Value::as_str).unwrap_or("");
    if log_path.is_empty() || !Path::new(log_path).exists() {
        let where_ = if log_path.is_empty() {
            "(no log_path recorded)"
        } else {
            log_path
        };
        eprintln!(
            "no logs for {provider} agent {}: no log file at {where_}",
            args.name
        );
        return 13;
    }

    // One-shot tail block (printed for both `logs` and `logs --follow`).
    match tail_lines_keepends(Path::new(log_path), args.tail) {
        Ok(block) => print!("{block}"),
        Err(exc) => {
            eprintln!("failed to read {log_path}: {exc}");
            return 1;
        }
    }

    if args.follow {
        // Stream subsequent lines via the agent.logs daemon RPC (Locked Decision #5).
        // The daemon looks up log_path by exact registry `name`, so carry the
        // RESOLVED row's canonical name (x-1b1e: args.name may be a short/session
        // id) rather than the raw token, or the follow attach silently misses.
        let resolved_name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(args.name.as_str());
        return crate::logs_client::follow(home, resolved_name).await;
    }
    0
}

/// Map a child `claude logs --follow` exit status to this process's exit code.
///
/// An operator stopping a `--follow` stream with Ctrl-C is a clean stop, matching
/// Python `read.py`'s follow path (`KeyboardInterrupt -> EXIT_OK`). SIGINT reaches
/// the whole foreground process group, so `claude` either catches it and exits 130
/// (`128 + SIGINT`) or is terminated by it (`status.signal() == SIGINT`); map both
/// to 0. Any other exit code is preserved. codex/gemini already return 0 via
/// `logs_client::follow` (cv-02da195d).
fn follow_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if status.signal() == Some(libc::SIGINT) {
        return 0;
    }
    match status.code() {
        Some(130) => 0,
        Some(c) => c,
        None => 1,
    }
}

/// Claude `logs` path: client-side subprocess passthrough with in-process tail
/// slicing (mirrors `providers.claude.logs`).
fn run_logs_claude(entry: &Value, args: &LogsArgs) -> i32 {
    if args.json_out {
        eprintln!(
            "WARN: JSON output for Claude logs not implemented in US3; falling back to raw passthrough"
        );
    }
    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    if short_id.is_empty() {
        let created = entry
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        eprintln!(
            "claude agent {} (created {created}) has no short id on file; cannot read logs. This entry may predate US1's short-id capture; try re-dispatching with `fno agents ask`.",
            args.name
        );
        return 1;
    }

    if args.follow {
        // Stream claude's output directly (inherited stdio) and map the exit code
        // via follow_exit_code so an operator Ctrl-C is a clean stop (cv-02da195d).
        //
        // Ctrl-C reaches the whole foreground process group, so without
        // intervention SIGINT would terminate THIS parent (Rust installs no
        // handler; the default disposition kills it) before Command::status()
        // returns and follow_exit_code can run. Ignore SIGINT in the parent for
        // the duration of the wait so it survives to map the child's status; the
        // child resets SIGINT to its default via pre_exec so `claude` still sees
        // Ctrl-C (ignored dispositions are inherited across exec, so the child
        // must undo it). (codex P2 on PR #367.)
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("logs").arg(short_id).arg("--follow");
        // SAFETY: pre_exec runs in the forked child before exec; libc::signal is
        // async-signal-safe and no other process state is touched here.
        unsafe {
            cmd.pre_exec(|| {
                if libc::signal(libc::SIGINT, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // SAFETY: install SIG_IGN for the wait, then restore the prior handler.
        let prev_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let status = cmd.status();
        unsafe {
            libc::signal(libc::SIGINT, prev_sigint);
        }
        match status {
            Ok(status) => follow_exit_code(status),
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "claude logs: claude binary not found on PATH; install claude or check $PATH"
                );
                127
            }
            Err(exc) => {
                eprintln!(
                    "claude logs {}: OSError invoking claude: {exc}",
                    py_repr_str(short_id)
                );
                1
            }
        }
    } else {
        let output = std::process::Command::new("claude")
            .arg("logs")
            .arg(short_id)
            .output();
        let output = match output {
            Ok(o) => o,
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "claude logs: claude binary not found on PATH; install claude or check $PATH"
                );
                return 127;
            }
            Err(exc) => {
                eprintln!(
                    "claude logs {}: OSError invoking claude: {exc}",
                    py_repr_str(short_id)
                );
                return 1;
            }
        };
        let raw_stdout = String::from_utf8_lossy(&output.stdout);
        let raw_stderr = String::from_utf8_lossy(&output.stderr);
        let sliced = tail_lines_of_str(&raw_stdout, args.tail);
        print!("{sliced}");
        if !raw_stderr.is_empty() {
            eprint!("{raw_stderr}");
        }
        let rc = output.status.code().unwrap_or(1);
        if rc != 0 && raw_stderr.is_empty() {
            eprintln!(
                "claude logs {} exited {rc} with no stderr output",
                py_repr_str(short_id)
            );
        }
        rc
    }
}

// ---------------------------------------------------------------------------
// report (inside-leg state push, E3.2)
// ---------------------------------------------------------------------------

/// Parse a `report` invocation into the `agent.report` params object, or an
/// error string for a malformed call (mapped to exit 2). Split out from
/// [`run_report`] so the flag/validation grammar is unit-testable without a
/// daemon: required `--session-id`/`--seq`/`--state`, optional
/// `--reason`/`--ttl-ms`.
fn build_report_params(rest: &[String]) -> Result<Value, String> {
    let args = expand_eq(rest);
    let mut session_id: Option<String> = None;
    let mut seq: Option<u64> = None;
    let mut state: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut ttl_ms: Option<u64> = None;

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session-id" => session_id = it.next(),
            "--state" => state = it.next(),
            "--reason" => reason = it.next(),
            "--seq" => {
                seq = Some(
                    it.next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or("--seq needs a non-negative integer")?,
                )
            }
            "--ttl-ms" => {
                ttl_ms = Some(
                    it.next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or("--ttl-ms needs a non-negative integer")?,
                )
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return Err("report needs --session-id".into()),
    };
    let seq = seq.ok_or("report needs --seq")?;
    let state = match state.as_deref() {
        Some("working") => "working",
        Some("blocked") => "blocked",
        Some("done") => "done",
        _ => return Err("report needs --state working|blocked|done".into()),
    };

    let mut params = serde_json::Map::new();
    params.insert("session_id".into(), Value::String(session_id));
    params.insert("seq".into(), Value::Number(seq.into()));
    params.insert("state".into(), Value::String(state.into()));
    if let Some(r) = reason {
        params.insert("reason".into(), Value::String(r));
    }
    if let Some(t) = ttl_ms {
        params.insert("ttl_ms".into(), Value::Number(t.into()));
    }
    Ok(Value::Object(params))
}

/// `fno-agents report --session-id <uuid> --seq <n> --state
/// working|blocked|done [--reason <text>] [--ttl-ms <n>]` -- the inside-leg state
/// push (E3.2). A per-turn hook calls this; it builds the `agent.report` RPC and
/// sends it to an ALREADY-RUNNING daemon (never lazy-starts one -- a hook must
/// not boot the daemon). Fire-and-forget: a down daemon is exit 0 (no grid to
/// report to), a successful store/drop is exit 0; only a malformed invocation
/// (exit 2) or a real transport error (exit 1) is loud, so a per-turn hook never
/// reds a turn.
pub async fn run_report(rest: &[String], home: &AgentsHome) -> i32 {
    let params = match build_report_params(rest) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };
    let req = crate::protocol::Request::new(1, "agent.report", params);
    match crate::client::call_if_running(home, &req).await {
        Ok(_) => 0,
        Err(crate::client::ClientError::DaemonNotRunning) => 0,
        Err(e) => {
            eprintln!("fno-agents: report failed: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// claim (hidden debug verb over the native claims module)
// ---------------------------------------------------------------------------

/// `fno-agents claim <acquire|release|status> <key> [flags]` — a thin front
/// over [`crate::claims`], the native lockfile-protocol implementation.
///
/// Purpose: (a) the cross-impl compatibility matrix
/// (`cli/tests/integration/test_claims_cross_impl.py`) drives the Rust side
/// of the protocol through it, and (b) an ops escape hatch when the Python
/// CLI is unavailable. It is deliberately HIDDEN — dispatched via `matches!`
/// in `bin/client.rs` (the `mail-inject` pattern) so it stays out of
/// `CLIENT_VERB_USAGE` / `RUST_CLIENT_VERBS`; `fno claim` remains the only
/// operator CLI for claims.
///
/// Output is one JSON object on stdout. Exit codes: 0 success, 1 held by
/// another live writer, 2 usage/validation/io error.
pub fn run_claim(args: &[String]) -> i32 {
    let Some(op) = args.first().map(String::as_str) else {
        eprintln!("fno-agents: claim requires an operation: acquire|release|status|sweep");
        return 2;
    };
    if op == "sweep" {
        return run_claim_sweep(&args[1..]);
    }
    let Some(key) = args.get(1).filter(|k| !k.starts_with("--")).cloned() else {
        eprintln!("fno-agents: claim {op} requires a key argument");
        return 2;
    };

    let mut holder: Option<String> = None;
    let mut opts = crate::claims::AcquireOpts::default();
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        let mut take = |name: &str| -> Option<String> {
            let v = it.next().cloned();
            if v.is_none() {
                eprintln!("fno-agents: claim: {name} requires a value");
            }
            v
        };
        match a.as_str() {
            "--holder" => holder = take("--holder"),
            "--pid" => match take("--pid").and_then(|v| v.parse::<u32>().ok()) {
                Some(p) => opts.pid = Some(p),
                None => return 2,
            },
            "--ttl-ms" => match take("--ttl-ms").and_then(|v| v.parse::<i64>().ok()) {
                Some(t) => opts.ttl_ms = Some(t),
                None => return 2,
            },
            "--reason" => match take("--reason") {
                Some(r) => opts.reason = Some(r),
                None => return 2,
            },
            "--metadata" => {
                let Some(raw) = take("--metadata") else {
                    return 2;
                };
                match serde_json::from_str::<Value>(&raw) {
                    Ok(Value::Object(m)) => opts.metadata = Some(m),
                    _ => {
                        eprintln!("fno-agents: claim: --metadata must be a JSON object");
                        return 2;
                    }
                }
            }
            "--root" => match take("--root") {
                Some(r) => opts.root = Some(PathBuf::from(r)),
                None => return 2,
            },
            "--json" | "-J" => {} // output is always JSON; accepted for symmetry
            other => {
                eprintln!("fno-agents: claim: unknown flag {other}");
                return 2;
            }
        }
    }

    match op {
        "acquire" => {
            let Some(holder) = holder else {
                eprintln!("fno-agents: claim acquire requires --holder");
                return 2;
            };
            match crate::claims::acquire(&key, &holder, opts) {
                crate::claims::AcquireOutcome::Acquired(rec) => {
                    let mut out = serde_json::to_value(&rec)
                        .unwrap_or_else(|_| Value::Object(Default::default()));
                    if let Value::Object(m) = &mut out {
                        m.insert("outcome".into(), Value::String("acquired".into()));
                    }
                    println!("{out}");
                    0
                }
                crate::claims::AcquireOutcome::HeldByOther { holder, pid, host } => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "outcome": "held_by_other",
                            "holder": holder, "pid": pid, "host": host,
                        })
                    );
                    1
                }
                crate::claims::AcquireOutcome::Error(e) => {
                    eprintln!("fno-agents: claim acquire failed: {e}");
                    2
                }
            }
        }
        "release" => {
            let Some(holder) = holder else {
                eprintln!("fno-agents: claim release requires --holder");
                return 2;
            };
            match crate::claims::release(
                &key,
                &holder,
                opts.root.as_deref(),
                opts.events_dir.as_deref(),
            ) {
                Ok(()) => {
                    println!("{}", serde_json::json!({"outcome": "released", "key": key}));
                    0
                }
                Err(e) => {
                    eprintln!("fno-agents: claim release failed: {e}");
                    2
                }
            }
        }
        "status" => {
            let (state, rec) = crate::claims::status(&key, opts.root.as_deref());
            // Mirror the `fno claim status -J` dict shape so the compat
            // matrix can diff the two implementations field-by-field.
            let mut out = serde_json::Map::new();
            out.insert("key".into(), Value::String(key));
            out.insert("state".into(), Value::String(state.as_str().into()));
            if let Some(rec) = rec {
                out.insert("holder".into(), Value::String(rec.holder));
                out.insert("pid".into(), Value::Number(rec.pid.into()));
                out.insert("host".into(), Value::String(rec.host));
                out.insert("acquired_at".into(), Value::Number(rec.acquired_at.into()));
                out.insert(
                    "expires_at".into(),
                    rec.expires_at.map(Value::from).unwrap_or(Value::Null),
                );
                if let Some(r) = rec.reason {
                    out.insert("reason".into(), Value::String(r));
                }
                if let Some(h) = rec.harness {
                    out.insert("harness".into(), Value::String(h));
                }
                if !rec.metadata.is_empty() {
                    out.insert("metadata".into(), Value::Object(rec.metadata));
                }
            }
            println!("{}", Value::Object(out));
            0
        }
        other => {
            eprintln!(
                "fno-agents: unknown claim operation: {other} (use acquire|release|status|sweep)"
            );
            2
        }
    }
}

/// `fno-agents claim sweep [--json] [--root <dir>]` — read every `node:` /
/// `dispatch:` lockfile in the claims dir, classify each with the canonical
/// [`crate::claims::classify`], and print ONE JSON object:
/// `{"claims": [{"key", "state", "holder", "host", "pid"}, ...]}`.
///
/// The mux shells this (bounded, fail-open) to overlay in-flight state onto
/// work-queue cards — the verdict shape above is a pinned contract (additive
/// fields allowed, renames are not; `state` uses `ClaimState::as_str`
/// vocabulary and consumers treat only `"live"` as in-flight).
///
/// A missing/unreadable claims dir is an EMPTY sweep (exit 0), not an error:
/// no claims means no overlay. Unparseable/newer-schema lockfiles are
/// excluded from the payload and logged to stderr (never fatal).
fn run_claim_sweep(args: &[String]) -> i32 {
    let mut root: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => match it.next() {
                Some(r) => root = Some(PathBuf::from(r)),
                None => {
                    eprintln!("fno-agents: claim sweep: --root requires a value");
                    return 2;
                }
            },
            "--json" | "-J" => {} // output is always JSON; accepted for symmetry
            other => {
                eprintln!("fno-agents: claim sweep: unknown flag {other}");
                return 2;
            }
        }
    }
    let Some(dir) = crate::claims::claims_dir_for(root.as_deref()) else {
        // No resolvable claims root: same as an empty dir (fail-open).
        println!("{}", serde_json::json!({"claims": []}));
        return 0;
    };
    println!("{}", claim_sweep_payload(&dir));
    0
}

/// Pure(ish) core of `claim sweep`: scan `dir` for `node:` / `dispatch:`
/// lockfiles and build the pinned verdict object. Separated from
/// [`run_claim_sweep`] so tests can drive it against a temp dir.
fn claim_sweep_payload(dir: &Path) -> Value {
    // Filename prefilter: keys are percent-encoded (`:` -> `%3A`), so only
    // read files that can be node/dispatch claims; `.expired/` is a subdir
    // and non-`.lock` names are skipped by the same test.
    let node_pfx = crate::claims::encode_key("node:");
    let dispatch_pfx = crate::claims::encode_key("dispatch:");
    let mut claims: Vec<Value> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return serde_json::json!({ "claims": [] }),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".lock")
            || !(name.starts_with(&node_pfx) || name.starts_with(&dispatch_pfx))
        {
            continue;
        }
        match crate::claims::read_claim_file(&entry.path()) {
            Ok(rec) => {
                // Trust the record's own key over the filename decode; a
                // record whose key does not carry a sweep prefix is excluded
                // (filename lied — treat like corruption, minus the noise).
                if !(rec.key.starts_with("node:") || rec.key.starts_with("dispatch:")) {
                    continue;
                }
                let state = crate::claims::classify(&rec, None);
                claims.push(serde_json::json!({
                    "key": rec.key,
                    "state": state.as_str(),
                    "holder": rec.holder,
                    "host": rec.host,
                    "pid": rec.pid,
                }));
            }
            Err(crate::claims::ReadError::GoneAway) => continue,
            Err(crate::claims::ReadError::Corrupted(e)) => {
                eprintln!("fno-agents: claim sweep: skipping {name}: {e}");
                continue;
            }
        }
    }
    claims.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    serde_json::json!({ "claims": claims })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- find_agent_entry (x-1b1e): parity with Python resolve_agent ----------

    fn claude_row(name: &str, short: &str, uuid: &str) -> Value {
        json!({
            "name": name, "provider": "claude", "cwd": "/w", "log_path": "/l",
            "short_id": short, "claude_session_uuid": uuid, "harness_session_id": uuid,
        })
    }

    const RESOLVE_UUID: &str = "7c5dcf5d-c078-4b53-a8c9-7199b831eae4";

    #[test]
    fn find_agent_entry_resolves_all_three_forms() {
        // AC1-HP: name, full uuid (case-insensitive), and 8-hex short all hit one row.
        let rows = vec![claude_row("billing", "7c5dcf5d", RESOLVE_UUID)];
        for tok in [
            "billing",
            RESOLVE_UUID,
            &RESOLVE_UUID.to_uppercase(),
            "7c5dcf5d",
        ] {
            let e = find_agent_entry(&rows, tok).expect("resolves");
            assert_eq!(e["name"], "billing");
        }
    }

    #[test]
    fn find_agent_entry_daemon_and_derived_short_both_resolve() {
        // AC2-HP: a codex row resolves by its name-derived daemon short AND by
        // the derived 8-hex prefix of its thread id.
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        let row = json!({
            "name": "reviewer", "provider": "codex", "cwd": "/w", "log_path": "/l",
            "short_id": "billingf", "codex_session_id": uuid, "harness_session_id": uuid,
        });
        let rows = vec![row];
        assert_eq!(
            find_agent_entry(&rows, "billingf").unwrap()["name"],
            "reviewer"
        );
        assert_eq!(
            find_agent_entry(&rows, "a1b2c3d4").unwrap()["name"],
            "reviewer"
        );
    }

    #[test]
    fn find_agent_entry_name_precedence_over_hex() {
        // AC1-EDGE: a hex-shaped name wins over a different row's short_id.
        let rows = vec![
            claude_row(
                "deadbeef",
                "aaaa0000",
                "aaaa0000-0000-0000-0000-000000000000",
            ),
            claude_row("other", "deadbeef", "deadbeef-1111-1111-1111-111111111111"),
        ];
        assert_eq!(
            find_agent_entry(&rows, "deadbeef").unwrap()["name"],
            "deadbeef"
        );
    }

    #[test]
    fn find_agent_entry_ambiguous_same_tier_short_collision() {
        // AC2-ERR: two rows sharing a short_id error as ambiguous, never first-match.
        let rows = vec![
            claude_row("aa", "abcd1234", "11111111-0000-0000-0000-000000000000"),
            claude_row("bb", "abcd1234", "22222222-0000-0000-0000-000000000000"),
        ];
        assert!(matches!(
            find_agent_entry(&rows, "abcd1234"),
            Err(ResolveError::Ambiguous(_))
        ));
    }

    #[test]
    fn find_agent_entry_unknown_and_empty_and_boundary() {
        // AC1-ERR: unknown token; empty token; 7/9-hex are not shorts.
        let rows = vec![claude_row("billing", "7c5dcf5d", RESOLVE_UUID)];
        for tok in ["nope", "", "   ", "7c5dcf5", "7c5dcf5dd"] {
            assert!(matches!(
                find_agent_entry(&rows, tok),
                Err(ResolveError::NotFound(_))
            ));
        }
    }

    #[test]
    fn find_agent_entry_opencode_row_degrades_to_name_and_full_id() {
        // An opencode ses_ id has no hex prefix: resolvable by name/full-id only.
        let ses = "ses_7f3a9b2c1d0e";
        let row = json!({
            "name": "oc", "provider": "opencode", "cwd": "/w", "log_path": "/l",
            "harness_session_id": ses,
        });
        let rows = vec![row];
        assert_eq!(find_agent_entry(&rows, "oc").unwrap()["name"], "oc");
        assert_eq!(find_agent_entry(&rows, ses).unwrap()["name"], "oc");
        assert!(matches!(
            find_agent_entry(&rows, "7f3a9b2c"),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn report_params_full_payload() {
        let p = build_report_params(&[
            "--session-id".into(),
            "uuid-x".into(),
            "--seq".into(),
            "7".into(),
            "--state".into(),
            "blocked".into(),
            "--reason".into(),
            "awaiting input".into(),
            "--ttl-ms".into(),
            "5000".into(),
        ])
        .unwrap();
        assert_eq!(p["session_id"], "uuid-x");
        assert_eq!(p["seq"], 7);
        assert_eq!(p["state"], "blocked");
        assert_eq!(p["reason"], "awaiting input");
        assert_eq!(p["ttl_ms"], 5000);
    }

    #[test]
    fn report_params_minimal_omits_optionals() {
        let p = build_report_params(&[
            "--session-id=uuid-y".into(), // also exercises --k=v expansion
            "--seq".into(),
            "1".into(),
            "--state".into(),
            "working".into(),
        ])
        .unwrap();
        assert_eq!(p["session_id"], "uuid-y");
        assert!(p.get("reason").is_none());
        assert!(p.get("ttl_ms").is_none());
    }

    #[test]
    fn report_params_rejects_bad_input() {
        assert!(build_report_params(&[
            "--seq".into(),
            "1".into(),
            "--state".into(),
            "working".into()
        ])
        .is_err()); // no session
        assert!(build_report_params(&[
            "--session-id".into(),
            "x".into(),
            "--state".into(),
            "working".into()
        ])
        .is_err()); // no seq
        assert!(build_report_params(&[
            "--session-id".into(),
            "x".into(),
            "--seq".into(),
            "1".into()
        ])
        .is_err()); // no state
        assert!(build_report_params(&[
            "--session-id".into(),
            "x".into(),
            "--seq".into(),
            "1".into(),
            "--state".into(),
            "idle".into()
        ])
        .is_err()); // bad state
        assert!(build_report_params(&[
            "--session-id".into(),
            "x".into(),
            "--seq".into(),
            "nope".into(),
            "--state".into(),
            "working".into()
        ])
        .is_err()); // non-int seq
    }

    #[test]
    fn python_json_uses_spaced_separators() {
        #[derive(Serialize)]
        struct S {
            active: bool,
            sessions: Vec<u8>,
        }
        let out = to_python_json(&S {
            active: false,
            sessions: vec![],
        });
        assert_eq!(out, r#"{"active": false, "sessions": []}"#);
    }

    #[test]
    fn drive_auth_json_shape_matches_python() {
        let out = DriveAuthOut {
            active: true,
            sessions: vec![DriveAuthSession {
                short_id: "wkI".into(),
                session_id: Value::String("d-1".into()),
                mode: "interactive".into(),
            }],
        };
        assert_eq!(
            to_python_json(&out),
            r#"{"active": true, "sessions": [{"short_id": "wkI", "session_id": "d-1", "mode": "interactive"}]}"#
        );
    }

    #[test]
    fn json_truthy_matches_python() {
        assert!(!json_truthy(None));
        assert!(!json_truthy(Some(&Value::Null)));
        assert!(!json_truthy(Some(&json!(false))));
        assert!(json_truthy(Some(&json!(true))));
        assert!(!json_truthy(Some(&json!(0))));
        assert!(json_truthy(Some(&json!(1))));
        assert!(!json_truthy(Some(&json!(""))));
        assert!(json_truthy(Some(&json!("x"))));
    }

    #[test]
    fn parse_iso8601_handles_z_and_naive() {
        let z = parse_iso8601("2026-05-26T10:30:45Z").unwrap();
        let off = parse_iso8601("2026-05-26T10:30:45+00:00").unwrap();
        assert_eq!(z, off);
        // naive assumed UTC
        let naive = parse_iso8601("2026-05-26T10:30:45").unwrap();
        assert_eq!(naive, z);
        assert!(parse_iso8601("not-a-date").is_none());
    }

    #[test]
    fn slice_limit_matches_python_slicing() {
        assert_eq!(slice_limit(vec![1, 2, 3, 4], 2), vec![1, 2]);
        assert_eq!(slice_limit(vec![1, 2, 3, 4], 0), Vec::<i32>::new());
        assert_eq!(slice_limit(vec![1, 2, 3, 4], 10), vec![1, 2, 3, 4]);
        // Python list[:-1] drops the last element.
        assert_eq!(slice_limit(vec![1, 2, 3, 4], -1), vec![1, 2, 3]);
        // Over-large negative -> empty.
        assert_eq!(slice_limit(vec![1, 2, 3, 4], -10), Vec::<i32>::new());
    }

    #[test]
    fn trace_name_required_without_all() {
        let args = TraceArgs {
            name: None,
            request_id: None,
            all_agents: false,
            json_out: false,
            limit: 200,
            since: None,
        };
        let r = trace_logic(&args, Path::new("/nonexistent"), Path::new("/nonexistent"));
        assert_eq!(r.exit_code, 2);
        assert!(r.stderr.contains("agent NAME is required unless --all"));
    }

    #[test]
    fn trace_all_empty_events_says_no_events() {
        let args = TraceArgs {
            name: None,
            request_id: None,
            all_agents: true,
            json_out: false,
            limit: 200,
            since: None,
        };
        let r = trace_logic(
            &args,
            Path::new("/nonexistent/events.jsonl"),
            Path::new("/nonexistent"),
        );
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "no events yet\n");
    }

    #[test]
    fn session_id_field_and_resume_argv_match_python() {
        assert_eq!(session_id_field("claude"), Some("short_id"));
        assert_eq!(session_id_field("codex"), Some("harness_session_id"));
        assert_eq!(session_id_field("gemini"), Some("harness_session_id"));
        assert_eq!(session_id_field("unknown"), None);

        assert_eq!(
            build_resume_argv("codex", "uuid-1"),
            Some(vec!["codex".into(), "resume".into(), "uuid-1".into()])
        );
        assert_eq!(
            build_resume_argv("claude", "abc123"),
            Some(vec!["claude".into(), "attach".into(), "abc123".into()])
        );
        assert_eq!(
            build_resume_argv("gemini", "g-1"),
            Some(vec!["gemini".into(), "--resume".into(), "g-1".into()])
        );
        assert_eq!(build_resume_argv("opencode", "x"), None);
    }

    #[test]
    fn is_uuid_shaped_accepts_only_lowercase_8_4_4_4_12_hex() {
        assert!(is_uuid_shaped("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9"));
        assert!(!is_uuid_shaped("")); // empty
        assert!(!is_uuid_shaped("not-a-uuid"));
        assert!(!is_uuid_shaped("0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9")); // uppercase
        assert!(!is_uuid_shaped("0a1b2c3d4e5f6071829 3a4b5c6d7e8f9")); // no dashes
        assert!(!is_uuid_shaped("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f")); // 11-char tail
    }

    // Fixture: an auto-cleaned temp dir used as a fake $HOME under which the
    // tests write bg session files. Returns a tempfile::TempDir (the pattern the
    // rest of this module's tests use) so a panicking test never leaks a /tmp
    // tree.
    fn cv_tmpdir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn claude_resume_argv_live_attaches_dead_resumes_absent_refuses() {
        use std::os::unix::net::UnixListener;
        let uuid = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";

        // Dead row (no session file) with a recorded uuid -> `claude --resume`.
        let home = cv_tmpdir();
        let ch = ClaudeHome::at(home.path());
        let entry = serde_json::json!({
            "name": "w", "provider": "claude",
            "short_id": "7c5dcf5d", "claude_session_uuid": uuid,
        });
        assert_eq!(
            claude_resume_argv(&ch, &entry, "w").unwrap(),
            (
                vec!["claude".to_string(), "--resume".into(), uuid.into()],
                Some(uuid.to_string()), // dead-arm carries the uuid to claim
            )
        );

        // uuid absent -> refuse (Err 13), never `claude --resume ""`.
        let entry_no_uuid = serde_json::json!({
            "name": "w", "provider": "claude", "short_id": "7c5dcf5d",
        });
        assert_eq!(claude_resume_argv(&ch, &entry_no_uuid, "w"), Err(13));

        // Live supervisor (socket answers) beats a stale "exited" registry ->
        // `claude attach <short_id>`, no --resume (AC1-EDGE).
        let home2 = cv_tmpdir();
        let sessions = home2.path().join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let sock = home2.path().join("live.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        fs::write(
            sessions.join("222.json"),
            format!(
                "{{\"jobId\":\"7c5dcf5d\",\"kind\":\"bg\",\"messagingSocketPath\":\"{}\",\"sessionId\":\"s\",\"cwd\":\"/tmp\"}}",
                sock.to_str().unwrap()
            ),
        )
        .unwrap();
        let ch2 = ClaudeHome::at(home2.path());
        assert_eq!(
            claude_resume_argv(&ch2, &entry, "w").unwrap(),
            (
                vec!["claude".to_string(), "attach".into(), "7c5dcf5d".into()],
                None, // live attach arm claims nothing
            )
        );
    }

    #[test]
    fn acquire_resume_session_claim_refuses_when_held_by_other() {
        use crate::claims::{acquire, AcquireOpts, AcquireOutcome};
        let uuid = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let root = cv_tmpdir();

        // A different live writer already holds the session claim.
        let pre = acquire(
            &format!("session:{uuid}"),
            "other-writer",
            AcquireOpts {
                root: Some(root.path().to_path_buf()),
                ..Default::default()
            },
        );
        assert!(matches!(pre, AcquireOutcome::Acquired(_)));

        // The racing resumer loses: refuses (exit 11) instead of a 2nd writer.
        let err = acquire_resume_session_claim(uuid, Some(root.path())).unwrap_err();
        assert_eq!(err.0, 11);
        assert!(err.1.contains("held live by another writer"));

        // A session with no holder: the resumer wins.
        let uuid2 = "1111abcd-2222-3333-4444-555566667777";
        assert!(acquire_resume_session_claim(uuid2, Some(root.path())).is_ok());
    }

    #[test]
    fn claude_attach_pointer_only_for_dead_revivable_claude_row() {
        use std::os::unix::net::UnixListener;
        let uuid = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let dead = cv_tmpdir();
        let ch_dead = ClaudeHome::at(dead.path());

        // Dead row + uuid -> pointer naming both revival commands.
        let entry = serde_json::json!({
            "name": "w", "provider": "claude",
            "short_id": "7c5dcf5d", "claude_session_uuid": uuid,
        });
        let msg = claude_attach_pointer(&ch_dead, &entry, "w").expect("dead row -> pointer");
        assert!(msg.contains("fno agents resume w"));
        assert!(msg.contains(&format!("--resume {uuid} --substrate bg")));

        // No uuid -> no pointer (never print an unusable command).
        let no_uuid = serde_json::json!({
            "name": "w", "provider": "claude", "short_id": "7c5dcf5d",
        });
        assert_eq!(claude_attach_pointer(&ch_dead, &no_uuid, "w"), None);

        // Live supervisor -> no pointer (fall through to a real attach).
        let live_home = cv_tmpdir();
        let sessions = live_home.path().join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let sock = live_home.path().join("live.sock");
        let _l = UnixListener::bind(&sock).unwrap();
        fs::write(
            sessions.join("222.json"),
            format!(
                "{{\"jobId\":\"7c5dcf5d\",\"kind\":\"bg\",\"messagingSocketPath\":\"{}\",\"sessionId\":\"s\",\"cwd\":\"/tmp\"}}",
                sock.to_str().unwrap()
            ),
        )
        .unwrap();
        assert_eq!(
            claude_attach_pointer(&ClaudeHome::at(live_home.path()), &entry, "w"),
            None
        );
    }

    #[test]
    fn shlex_quote_matches_python() {
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("/Users/foo/code"), "/Users/foo/code");
        assert_eq!(shlex_quote("abc-def_123"), "abc-def_123");
        assert_eq!(shlex_quote("a b"), "'a b'");
        // embedded single quote -> '"'"'
        assert_eq!(shlex_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn py_repr_str_matches_cpython_common_cases() {
        assert_eq!(py_repr_str("worker-A"), "'worker-A'");
        // contains ' but not " -> double-quoted
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        // backslash is doubled in both quote forms (the old double-quote branch
        // skipped this).
        assert_eq!(py_repr_str("a\\b"), "'a\\\\b'");
        assert_eq!(py_repr_str("it's\\x"), "\"it's\\\\x\"");
        // control chars escape like CPython repr: \t \n \r then \xXX (lowercase).
        assert_eq!(py_repr_str("a\nb"), "'a\\nb'");
        assert_eq!(py_repr_str("tab\there"), "'tab\\there'");
        assert_eq!(py_repr_str("x\u{7f}y"), "'x\\x7fy'");
        assert_eq!(py_repr_str("\u{1b}["), "'\\x1b['");
        // printable non-ASCII stays literal, matching CPython repr('café').
        assert_eq!(py_repr_str("café"), "'café'");
    }

    #[test]
    fn tail_lines_of_str_matches_python_slice() {
        // tail 0 -> empty; tail > 0 -> last N lines keepends; over-large -> all.
        assert_eq!(tail_lines_of_str("a\nb\nc\n", 0), "");
        assert_eq!(tail_lines_of_str("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail_lines_of_str("a\nb\nc\n", 10), "a\nb\nc\n");
        // last line without trailing newline is preserved as-is here (the file
        // reader is what appends the missing newline).
        assert_eq!(tail_lines_of_str("a\nb", 1), "b");
    }

    #[test]
    fn tail_lines_keepends_appends_missing_newline() {
        let dir = std::env::temp_dir().join(format!(
            "abi-cv-logs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("log.jsonl");
        // Three lines, last without a trailing newline.
        fs::write(&f, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}").unwrap();
        assert_eq!(tail_lines_keepends(&f, 0).unwrap(), "");
        assert_eq!(
            tail_lines_keepends(&f, 2).unwrap(),
            "{\"b\":2}\n{\"c\":3}\n" // missing newline on last line appended
        );
        assert_eq!(
            tail_lines_keepends(&f, 10).unwrap(),
            "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn follow_exit_code_maps_ctrl_c_to_zero() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        // claude caught SIGINT and exited 130 (128 + SIGINT): clean stop -> 0.
        assert_eq!(follow_exit_code(ExitStatus::from_raw(130 << 8)), 0);
        // claude terminated directly by SIGINT (signal in the low bits): -> 0.
        assert_eq!(follow_exit_code(ExitStatus::from_raw(libc::SIGINT)), 0);
        // clean exit stays 0.
        assert_eq!(follow_exit_code(ExitStatus::from_raw(0)), 0);
        // a genuine non-zero exit is preserved (not masked to 0).
        assert_eq!(follow_exit_code(ExitStatus::from_raw(2 << 8)), 2);
        // terminated by a different signal (SIGTERM=15) is not a clean Ctrl-C;
        // there is no exit code so it falls through to 1.
        assert_eq!(follow_exit_code(ExitStatus::from_raw(libc::SIGTERM)), 1);
    }

    #[test]
    fn parse_logs_args_defaults_and_rejects_negative_tail() {
        let a = parse_logs_args(&["worker-A".to_string()]).unwrap();
        assert_eq!(a.name, "worker-A");
        assert_eq!(a.tail, 100);
        assert!(!a.follow);
        let a = parse_logs_args(&[
            "w".to_string(),
            "-n".to_string(),
            "5".to_string(),
            "-f".to_string(),
        ])
        .unwrap();
        assert_eq!(a.tail, 5);
        assert!(a.follow);
        // Attached short form `-n5` (codex P2) and the `--tail=N` equals form.
        assert_eq!(
            parse_logs_args(&["w".to_string(), "-n5".to_string()])
                .unwrap()
                .tail,
            5
        );
        assert_eq!(
            parse_logs_args(&["w".to_string(), "--tail=7".to_string()])
                .unwrap()
                .tail,
            7
        );
        let err = parse_logs_args(&["w".to_string(), "--tail".to_string(), "-3".to_string()]);
        assert!(matches!(err, Err((2, _))));
    }

    #[test]
    fn parse_logs_args_accepts_json_short() {
        // ab-3ff64151 (codex P2, PR #431): -J must parse like --json on the
        // Rust-routed `logs` path, not fall through to "unknown flag".
        let a = parse_logs_args(&["w".to_string(), "-J".to_string()]).unwrap();
        assert!(a.json_out);
    }

    #[test]
    fn parse_trace_args_accepts_global_register_shorts() {
        // ab-3ff64151 (codex P2, PR #431): -A/-J must parse identically to
        // --all/--json on the Rust-routed `trace` path.
        let short = parse_trace_args(&["-A".to_string(), "-J".to_string()]).unwrap();
        let long = parse_trace_args(&["--all".to_string(), "--json".to_string()]).unwrap();
        assert!(short.all_agents && short.json_out);
        assert_eq!(short.all_agents, long.all_agents);
        assert_eq!(short.json_out, long.json_out);
    }

    #[test]
    fn load_registry_entries_reads_agents_key_and_validates() {
        let dir = std::env::temp_dir().join(format!(
            "abi-cv-reg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");

        // Missing file -> empty (not an error).
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 0);

        // Real Python format: top-level "agents". A valid row carries the
        // required AgentEntry fields (name/provider/cwd/log_path).
        let valid = r#"{"name":"cx","provider":"codex","cwd":"/tmp/x","log_path":"/tmp/x/l","status":"live"}"#;
        fs::write(
            &reg,
            format!(r#"{{"schema_version":3,"agents":[{valid}]}}"#),
        )
        .unwrap();
        let rows = load_registry_entries(&reg).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "cx");

        // Daemon format: "entries" fallback.
        let valid_g = r#"{"name":"e","provider":"gemini","cwd":"/tmp/x","log_path":"/tmp/x/l","status":"live"}"#;
        fs::write(
            &reg,
            format!(r#"{{"schema_version":3,"entries":[{valid_g}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);

        // Current v8 (canonical-identity bump, x-ec59), v5 (inside_leg), and the
        // prior v4 (host_mode bump) are accepted, and v1 back-compat reads are
        // retained (the widened accepted set).
        fs::write(
            &reg,
            format!(r#"{{"schema_version":8,"agents":[{valid}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);
        fs::write(
            &reg,
            format!(r#"{{"schema_version":5,"agents":[{valid}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);
        fs::write(
            &reg,
            format!(r#"{{"schema_version":4,"agents":[{valid}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);
        fs::write(
            &reg,
            format!(r#"{{"schema_version":1,"agents":[{valid}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);

        // Unknown schema_version -> Err (Python RegistryVersionError -> exit 12/13).
        // v11 is the future-drift case a pre-bump reader would have on v10.
        fs::write(&reg, r#"{"schema_version":99,"agents":[]}"#).unwrap();
        assert!(load_registry_entries(&reg).is_err());
        fs::write(&reg, r#"{"schema_version":11,"agents":[]}"#).unwrap();
        assert!(load_registry_entries(&reg).is_err());

        // x-8dfc: an unknown provider no longer bricks the read -- it loads as
        // an undispatchable identity row (aider: a real CLI we deliberately do
        // not host). Capability is refused later at the spawn seam, not here.
        fs::write(
            &reg,
            r#"{"schema_version":3,"agents":[{"name":"x","provider":"aider","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);

        // Corrupt identity (empty provider AND no harness) still bricks (AC1-ERR).
        fs::write(
            &reg,
            r#"{"schema_version":3,"agents":[{"name":"x","provider":"","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        assert!(load_registry_entries(&reg).is_err());

        // Unknown status -> Err.
        fs::write(
            &reg,
            r#"{"schema_version":3,"agents":[{"name":"x","provider":"codex","cwd":"/x","log_path":"/l","status":"zombie"}]}"#,
        )
        .unwrap();
        assert!(load_registry_entries(&reg).is_err());

        // `exited` (and the other projected AgentStatus values) MUST be
        // accepted: the daemon writes `status:"exited"` when a worker exits
        // and retains the row until rm. A too-narrow {live,orphaned} set
        // hard-errored every read until the row was removed (ab-3c063856
        // grid testing surfaced this). Spot-check the previously-rejected
        // statuses now load cleanly.
        for st in [
            "exited",
            "idle",
            "spawning",
            "busy",
            "restarting",
            "failed",
            "permanent_dead",
            "ready",
        ] {
            fs::write(
                &reg,
                format!(
                    r#"{{"schema_version":3,"agents":[{{"name":"x","provider":"codex","cwd":"/x","log_path":"/l","status":"{st}"}}]}}"#
                ),
            )
            .unwrap();
            assert_eq!(
                load_registry_entries(&reg).unwrap().len(),
                1,
                "registry status {st:?} must be accepted (projection of state.status)"
            );
        }

        // Missing required field (no log_path) -> Err (Python AgentEntry TypeError).
        fs::write(
            &reg,
            r#"{"schema_version":3,"agents":[{"name":"x","provider":"codex","cwd":"/x","status":"live"}]}"#,
        )
        .unwrap();
        assert!(load_registry_entries(&reg).is_err());

        // agents not a list -> Err.
        fs::write(&reg, r#"{"schema_version":3,"agents":{}}"#).unwrap();
        assert!(load_registry_entries(&reg).is_err());

        // Invalid UTF-8 -> Err (strict decode, codex P2).
        fs::write(&reg, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load_registry_entries(&reg).is_err());

        fs::remove_dir_all(&dir).ok();
    }

    /// x-8dfc load-gate relaxation, the Rust half of the cross-language parity
    /// (AC1-FR): this reader accepts the same alien-harness fixture Python's
    /// `test_load_gate` accepts, and refuses the same corrupt fixture -- both
    /// directions pinned. Also covers AC1-EDGE (provider-less post-v10 shape)
    /// and AC2-ERR (divergence loads).
    #[test]
    fn load_registry_gate_shape_check_x8dfc() {
        let dir = std::env::temp_dir().join(format!(
            "abi-cv-reg8dfc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");

        // AC2-HP: an alien harness row (provider == harness == "newharness")
        // loads instead of bricking. Same fixture the Python parity test uses.
        fs::write(
            &reg,
            r#"{"schema_version":9,"agents":[{"name":"nh","provider":"newharness","harness":"newharness","harness_session_id":"deadbeefcafef00d","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        let rows = load_registry_entries(&reg).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["provider"], "newharness");

        // AC1-EDGE: a provider-less row (post-v10 writer shape, harness only)
        // loads with provider backfilled from harness.
        fs::write(
            &reg,
            r#"{"schema_version":9,"agents":[{"name":"pv","harness":"claude","harness_session_id":"aaaabbbbccccdddd","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        let rows = load_registry_entries(&reg).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["provider"], "claude");

        // AC2-ERR: a diverged row (provider != harness) LOADS (warning only).
        fs::write(
            &reg,
            r#"{"schema_version":9,"agents":[{"name":"dv","provider":"claude","harness":"codex","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);

        // Heal: a truthy-but-corrupt harness (whitespace) is replaced from the
        // valid provider, so resume (which reads through this) never keys on a
        // corrupt harness.
        fs::write(
            &reg,
            r#"{"schema_version":9,"agents":[{"name":"heal","provider":"claude","harness":"c x","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        let rows = load_registry_entries(&reg).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["harness"], "claude");

        // AC1-ERR: an empty-identity row (empty provider, no harness) still
        // bricks -- the corruption guard survives the relaxation.
        fs::write(
            &reg,
            r#"{"schema_version":9,"agents":[{"name":"bad","provider":"","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        assert!(load_registry_entries(&reg).is_err());

        // Whitespace-bearing identity is corruption, not an alien token.
        fs::write(
            &reg,
            r#"{"schema_version":9,"agents":[{"name":"ws","provider":"a b","cwd":"/x","log_path":"/l","status":"live"}]}"#,
        )
        .unwrap();
        assert!(load_registry_entries(&reg).is_err());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expand_eq_splits_long_options_only() {
        assert_eq!(
            expand_eq(&["--limit=5".to_string(), "w".to_string()]),
            vec!["--limit".to_string(), "5".to_string(), "w".to_string()]
        );
        // Value containing '=' keeps the rest intact.
        assert_eq!(
            expand_eq(&["--since=2026-01-01T00:00:00Z".to_string()]),
            vec!["--since".to_string(), "2026-01-01T00:00:00Z".to_string()]
        );
        // Positionals and short attached forms pass through unchanged.
        assert_eq!(expand_eq(&["a=b".to_string()]), vec!["a=b".to_string()]);
        assert_eq!(expand_eq(&["-n5".to_string()]), vec!["-n5".to_string()]);
    }

    #[test]
    fn append_agents_event_writes_python_envelope() {
        let dir = std::env::temp_dir().join(format!(
            "abi-cv-event-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let events = dir.join("events.jsonl");
        append_agents_event(
            &events,
            "agent_resumed",
            &[
                ("name", Value::String("worker-A".into())),
                ("provider", Value::String("codex".into())),
            ],
        );
        let content = fs::read_to_string(&events).unwrap();
        let line = content.trim_end();
        // data fields first, ts + kind last; compact (no spaces).
        assert!(line.starts_with(r#"{"name":"worker-A","provider":"codex","ts":"#));
        assert!(line.ends_with(r#""kind":"agent_resumed"}"#));
        let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(parsed["kind"], "agent_resumed");
        fs::remove_dir_all(&dir).ok();
    }

    // ---- claim sweep (x-54fa) --------------------------------------------

    fn sweep_acquire(root: &std::path::Path, key: &str) {
        let opts = crate::claims::AcquireOpts {
            root: Some(root.to_path_buf()),
            events_dir: Some(root.to_path_buf()),
            ..Default::default()
        };
        match crate::claims::acquire(key, "test-holder", opts) {
            crate::claims::AcquireOutcome::Acquired(_) => {}
            other => panic!("acquire {key} failed: {other:?}"),
        }
    }

    fn sweep_dir(root: &std::path::Path) -> PathBuf {
        crate::claims::claims_dir_for(Some(root)).unwrap()
    }

    #[test]
    fn claim_sweep_empty_or_missing_dir_is_empty_payload() {
        let td = tempfile::TempDir::new().unwrap();
        // Dir does not exist yet: empty payload, not an error (Boundaries:
        // "must handle an empty claims directory").
        let payload = claim_sweep_payload(&sweep_dir(td.path()));
        assert_eq!(payload, serde_json::json!({"claims": []}));
    }

    #[test]
    fn claim_sweep_reports_live_node_and_dispatch_claims() {
        let td = tempfile::TempDir::new().unwrap();
        sweep_acquire(td.path(), "node:x-ef41");
        sweep_acquire(td.path(), "dispatch:x-ef41");
        sweep_acquire(td.path(), "session:not-swept"); // out-of-scope prefix
        let payload = claim_sweep_payload(&sweep_dir(td.path()));
        let claims = payload["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 2, "session: claim must be excluded");
        // Sorted by key: dispatch: before node:.
        assert_eq!(claims[0]["key"], "dispatch:x-ef41");
        assert_eq!(claims[1]["key"], "node:x-ef41");
        for c in claims {
            // Acquired by THIS live process => live.
            assert_eq!(c["state"], "live");
            assert_eq!(c["holder"], "test-holder");
            assert_eq!(c["pid"], std::process::id());
            assert!(c["host"].as_str().is_some_and(|h| !h.is_empty()));
        }
    }

    #[test]
    fn claim_sweep_excludes_corrupted_and_newer_schema_lockfiles() {
        let td = tempfile::TempDir::new().unwrap();
        sweep_acquire(td.path(), "node:x-good");
        let dir = sweep_dir(td.path());
        // Corrupted YAML under a sweep-prefixed name.
        fs::write(dir.join("node%3Ax-bad.lock"), "{not yaml: [").unwrap();
        // Newer schema writer: parse refuses, sweep excludes (does not crash).
        fs::write(
            dir.join("node%3Ax-newer.lock"),
            "schema_version: 999\nkey: node:x-newer\nholder: h\nacquired_at: 1\npid: 1\nhost: x\n",
        )
        .unwrap();
        // Non-lock and dot files are skipped.
        fs::write(dir.join("node%3Ax-tmp.partial"), "x").unwrap();
        let payload = claim_sweep_payload(&dir);
        let claims = payload["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0]["key"], "node:x-good");
    }
}
