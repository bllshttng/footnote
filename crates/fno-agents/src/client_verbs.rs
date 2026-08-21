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

use crate::claude_ask::{
    family1_truth_state, family1_truth_state_for_resume, liveness_probe, locate_session, ClaudeHome,
};
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
const ACCEPTED_SCHEMA_VERSIONS: &[u64] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

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
    // READ FORWARD, matching Python's load_registry. This store is global to
    // every agent on the machine, so refusing a newer writer bricked every
    // deployed reader at once rather than only the one that was behind. Rows are
    // read as raw `Value` here and unknown keys are already ignored, so a newer
    // store costs this path nothing but the fields it cannot see.
    //
    // The announcement is required, not courtesy: silently reading a partial row
    // makes it indistinguishable from a complete one, and a routing decision
    // taken on one leaves no trace. Fixing only Python would have left this
    // path, the daemon, and mux still failing closed on the same file.
    let on_disk_version = obj.get("schema_version").and_then(Value::as_u64);
    let mut read_forward = false;
    match on_disk_version {
        Some(v) if ACCEPTED_SCHEMA_VERSIONS.contains(&v) => {}
        Some(v) if v > REGISTRY_SCHEMA_VERSION as u64 => {
            read_forward = true;
            eprintln!(
                "fno agents: registry at {} is schema_version={v}, ahead of the \
                 schema_version={REGISTRY_SCHEMA_VERSION} this fno understands. \
                 Reading the fields it knows and ignoring the rest; writes are \
                 refused until this fno is upgraded. Rows may be incomplete.",
                registry_path.display()
            );
        }
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
    // Under read-forward a row-level refusal skips THAT ROW rather than the whole
    // registry. Tolerating an added key was only half the fix: widening the
    // `status` enum or dropping a field that is required today would still have
    // failed every reader on the machine, which is the brick this path was just
    // changed to prevent. At or below our own schema each of these stays fatal,
    // where it means a writer bug rather than a version gap.
    let mut skipped: Vec<usize> = Vec::new();
    let mut kept: Vec<Value> = Vec::with_capacity(rows.len());
    for (i, row_value) in rows.iter().enumerate() {
        match validate_registry_row(i, row_value, on_disk_version.unwrap_or(0) < 15) {
            Ok(()) => kept.push(row_value.clone()),
            Err(_) if read_forward => skipped.push(i),
            Err(e) => return Err(e),
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "fno agents: registry at {}: skipped row(s) {skipped:?} this fno cannot \
             represent at this schema_version. Those agents are invisible to this \
             process until it is upgraded.",
            registry_path.display()
        );
    }
    let mut out = kept;
    for row in &mut out {
        if let Some(obj) = row.as_object_mut() {
            backfill_row_aliases(obj, on_disk_version.unwrap_or(0) < 15);
        }
    }
    Ok(out)
}

/// One registry row's shape checks, split out of [`load_registry_entries`] so a
/// newer-schema read can skip a single unrepresentable row instead of refusing
/// the shared file. Returns the same messages the inline checks used to return.
fn validate_registry_row(
    i: usize,
    row_value: &Value,
    legacy_provider_semantics: bool,
) -> Result<(), String> {
    {
        let row = row_value
            .as_object()
            .ok_or_else(|| format!("registry row {i} is not a JSON object"))?;
        // Before v15 provider was a harness alias. At v15 it is the separate
        // model-provider axis, so only harness establishes row identity.
        let provider = row.get("provider").and_then(Value::as_str);
        let harness = row.get("harness").and_then(Value::as_str);
        let valid_legacy_alias = legacy_provider_semantics && is_identity_token(provider);
        if !(valid_legacy_alias || is_identity_token(harness)) {
            return Err(format!(
                "registry row {i} has no valid identity token (provider={provider:?}, harness={harness:?})"
            ));
        }
        if provider.is_some() && !is_identity_token(provider) {
            return Err(format!(
                "registry row {i} has invalid provider={provider:?}"
            ));
        }
        // Divergence is meaningful only while both fields name the harness.
        if legacy_provider_semantics
            && is_identity_token(provider)
            && is_identity_token(harness)
            && provider != harness
        {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            eprintln!(
                "fno agents: warning: registry row {name:?} has provider={provider:?} and harness={harness:?} (diverged); harness wins for identity"
            );
        }
        // Absent and present-but-not-a-string are different answers. Folding them
        // together let a structured status from a newer writer silently become
        // "live" and KEEP the row, so the three readers disagreed about the same
        // file: this one listed the agent as live, the typed daemon path skipped
        // the row, and Python raised. Reject a non-string so the read-forward skip
        // above fires instead, and all three land on "cannot represent this row".
        let status = match row.get("status") {
            None | Some(Value::Null) => "live",
            Some(Value::String(s)) => s.as_str(),
            Some(other) => {
                return Err(format!("registry row {i} has non-string status={other}"));
            }
        };
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
    Ok(())
}

/// Reconcile one row's identity aliases so every verb body reads the same
/// fields regardless of the schema version that wrote the row.
///
/// Extracted from [`load_registry_entries`] because a row healed from a harness
/// store (x-da8c) never passes through that loader, and a row that skips this
/// is subtly broken in ways the verb reports as something else: `logs`/`attach`
/// read `provider`, which v10 no longer stores, and `claude_resume_argv` reads
/// `claude_session_uuid`, which v10 replaced with `harness_session_id`. One
/// shared backfill is why a healed row resumes exactly like a native one.
///
/// Caller obligation: at least one of `provider` / `harness` is a valid identity
/// token. `load_registry_entries` checks that before calling; `heal_token` gets
/// it from the healer, which only ever writes a known harness.
fn backfill_row_aliases(obj: &mut serde_json::Map<String, Value>, legacy_provider_semantics: bool) {
    // Lockstep alias heal (x-8dfc), mirroring Python `load_registry`:
    // the two identity fields are the same token in the skew window, so
    // heal whichever is missing OR corrupt (shape-checked, not truthy)
    // from the valid sibling. Both directions, because resume reads
    // through this same healed value -- a truthy-corrupt harness would
    // otherwise resolve session_id to None.
    let provider_valid = is_identity_token(obj.get("provider").and_then(Value::as_str));
    let harness_valid = is_identity_token(obj.get("harness").and_then(Value::as_str));
    if legacy_provider_semantics && !provider_valid && harness_valid {
        if let Some(h) = obj
            .get("harness")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            obj.insert("provider".into(), Value::String(h));
        }
    } else if legacy_provider_semantics && !harness_valid && provider_valid {
        if let Some(p) = obj
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            obj.insert("harness".into(), Value::String(p));
        }
    }
    // v10 (x-880e) accept-on-read, the raw-Value mirror of the typed
    // backfill_harness_aliases: TWO-WAY sync harness_session_id <-> the
    // harness-matching legacy per-provider key (canonical wins). A v1..=v9
    // row's legacy id back-fills the canonical field; a v10 row's canonical
    // id mirrors BACK into the legacy field so the raw resume/attach helpers
    // (which still read e.g. claude_session_uuid) resolve a harness-only row.
    let legacy_key = match obj.get("harness").and_then(Value::as_str) {
        Some("claude") => Some("claude_session_uuid"),
        Some("codex") => Some("codex_session_id"),
        Some("gemini") => Some("gemini_session_id"),
        _ => None,
    };
    if let Some(k) = legacy_key {
        let nonempty = |v: Option<&str>| {
            v.filter(|s| !s.is_empty() && *s != "null")
                .map(str::to_string)
        };
        let hsid = nonempty(obj.get("harness_session_id").and_then(Value::as_str));
        let legacy_val = nonempty(obj.get(k).and_then(Value::as_str));
        match (hsid, legacy_val) {
            // canonical wins: mirror it into the legacy key the helpers read
            (Some(h), _) => {
                obj.insert(k.into(), Value::String(h));
            }
            // no canonical yet: adopt the legacy id
            (None, Some(l)) => {
                obj.insert("harness_session_id".into(), Value::String(l));
            }
            (None, None) => {}
        }
    }
    // v9 transport-key backfill (x-1b1e), the raw-Value mirror of Python
    // `load_registry` popping `claude_short_id` into `short_id`: a legacy row's
    // jobId moves into an empty `short_id` and the old key is dropped so no verb
    // body reads it. A conflicting pair keeps `short_id` and warns once.
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
                    Ok(_) => match resolve_entry_with_heal(&rows, token, registry_path) {
                        Ok(e) => {
                            resolved_name = Some(
                                e.get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or(token)
                                    .to_string(),
                            );
                        }
                        Err(err) => {
                            return TraceResult {
                                exit_code: 13,
                                output: String::new(),
                                stderr: format!("fno agents trace: {}\n", err.message()),
                            };
                        }
                    },
                    Err(err) => {
                        let detail = match err {
                            ResolveError::Ambiguous(_) => err.message(),
                            ResolveError::NotFound(_) => {
                                format!("agent '{token}' not found in registry")
                            }
                        };
                        return TraceResult {
                            exit_code: 13,
                            output: String::new(),
                            stderr: format!("fno agents trace: {detail}\n"),
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
        "codex" | "gemini" | "opencode" => Some("harness_session_id"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared identifier resolver (x-1b1e): the Rust mirror of Python
// `registry.resolve_agent`. Every session-connecting verb (resume, attach,
// logs, trace) resolves a token to one row through this, so a session is
// addressable by name/slug, full harness_session_id, or an 8-hex short. Same
// full-id precedence + shared-short-namespace ambiguity semantics as the Python
// resolver; the US4 parity matrix asserts the two agree.
// ---------------------------------------------------------------------------

const ACCEPTED_FORMS_MSG: &str =
    "accepted forms: name, canonical handle, transport short id, or full session id";

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
    pub(crate) fn message(&self) -> String {
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

use crate::identity::session_handle_tier;

fn entry_session_tier(entry: &Value, token: &str) -> Option<u8> {
    let session_id = entry.get("harness_session_id").and_then(Value::as_str)?;
    session_handle_tier(token, session_id)
}

/// Return the single matched row, or an ambiguity error. Dedup only repeated
/// references to the same loaded row: a corrupt registry may contain one name
/// on two distinct session rows, and the intended PK cannot prove identity.
fn one_or_ambiguous<'a>(hits: Vec<&'a Value>, token: &str) -> Result<&'a Value, ResolveError> {
    let mut distinct: Vec<&Value> = Vec::new();
    for entry in hits {
        if !distinct
            .iter()
            .any(|existing| std::ptr::eq(*existing, entry))
        {
            distinct.push(entry);
        }
    }
    if distinct.len() > 1 {
        let cands = distinct
            .iter()
            .map(|e| {
                let n = e.get("name").and_then(Value::as_str).unwrap_or("?");
                let s = e
                    .get("short_id")
                    .and_then(Value::as_str)
                    .filter(|x| !x.is_empty())
                    .unwrap_or("-");
                let p = e
                    .get("harness")
                    .and_then(Value::as_str)
                    .or_else(|| e.get("provider").and_then(Value::as_str))
                    .unwrap_or("?");
                format!("{n} (short={s}, {p})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ResolveError::Ambiguous(format!(
            "token {} is ambiguous across {} agents: {cands}. Disambiguate with the name or full session id.",
            py_repr_str(token),
            distinct.len()
        )));
    }
    Ok(distinct[0])
}

/// Resolve a name, full session id, transport short id, canonical handle, or
/// legacy prefix to one row. A full id is explicit and resolves first; every
/// shorter address category is unioned before uniqueness is decided.
pub(crate) fn find_agent_entry<'a>(
    rows: &'a [Value],
    token: &str,
) -> Result<&'a Value, ResolveError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(ResolveError::NotFound(String::new()));
    }
    let by_full: Vec<&Value> = rows
        .iter()
        .filter(|e| entry_session_tier(e, token) == Some(0))
        .collect();
    if !by_full.is_empty() {
        return one_or_ambiguous(by_full, token);
    }

    let mut short_namespace: Vec<&Value> = rows
        .iter()
        .filter(|e| e.get("name").and_then(Value::as_str) == Some(token))
        .collect();
    short_namespace.extend(rows
        .iter()
        .filter(|e| matches!(e.get("short_id").and_then(Value::as_str), Some(s) if !s.is_empty() && s == token))
    );
    short_namespace.extend(
        rows.iter()
            .filter(|e| entry_session_tier(e, token) == Some(1)),
    );
    short_namespace.extend(
        rows.iter()
            .filter(|e| entry_session_tier(e, token) == Some(2)),
    );
    if !short_namespace.is_empty() {
        return one_or_ambiguous(short_namespace, token);
    }

    Err(ResolveError::NotFound(token.to_string()))
}

// ---------------------------------------------------------------------------
// All-source short-token resolution (x-da8c). The registry is a cache of reality,
// not a gate in front of it: store-only sessions participate in the same
// ambiguity namespace as registry rows. Rust lifecycle verbs reach the Python
// resolver through a shellout rather than growing a second store prober.
// ---------------------------------------------------------------------------

/// True for a token worth probing a harness store with -- the Rust mirror of
/// `store_fallback.is_session_shaped`. A plain unknown NAME never probes, so a
/// typo keeps today's refusal instead of paying for three store reads.
fn is_session_shaped(token: &str) -> bool {
    let token = token.trim();
    if let Some(rest) = token.strip_prefix("ses_") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric());
    }
    (token.len() == 8 && token.bytes().all(|b| b.is_ascii_alphanumeric()))
        || is_uuid_shaped(&token.to_ascii_lowercase())
}

fn token_helper_output(token: &str, registry_path: &Path) -> std::io::Result<std::process::Output> {
    use std::process::Command;

    let mut command = Command::new("fno");
    command
        .args(["agents", "heal-token", token])
        .arg("--registry")
        .arg(registry_path)
        .arg("--all-sources")
        .env("FNO_AGENTS_RUNTIME", "python");
    command.output()
}

/// Ask the Python resolver to union registry and harness-store candidates.
///
/// `Ok(Some(row))` on resolution, `Ok(None)` only on the helper's documented
/// clean miss, and `Err(msg)` on ambiguity or unavailable/incomplete coverage.
/// `FNO_AGENTS_RUNTIME=python` pins the child to the Python dispatch so the
/// shellout cannot recurse back into this binary.
fn heal_token(token: &str, registry_path: &Path) -> Result<Option<Value>, String> {
    let out = match token_helper_output(token, registry_path) {
        Ok(o) => o,
        Err(exc) => {
            return Err(format!(
                "cannot safely resolve token {} because the all-source identity helper could not run: {exc}. Use the full session id.",
                py_repr_str(token)
            ));
        }
    };
    // The healer adopts best-effort: a failed registry write still returns the
    // row, with the reason on stderr. Swallowing that would make the degradation
    // invisible -- the verb works, the roster silently does not.
    let parsed = parse_heal_token_output(token, &out);
    if matches!(&parsed, Ok(Some(_))) {
        let warn = String::from_utf8_lossy(&out.stderr);
        if !warn.trim().is_empty() {
            eprint!("{warn}");
        }
    }
    parsed
}

/// Enforce the Python helper's output contract without collapsing unavailable
/// coverage into a clean miss. Kept pure so malformed/off-contract subprocess
/// results are mechanically testable without mutating PATH.
fn parse_heal_token_output(
    token: &str,
    out: &std::process::Output,
) -> Result<Option<Value>, String> {
    const AMBIGUOUS: i32 = 3;
    const MISS: i32 = 13;

    if out.status.code() == Some(AMBIGUOUS) {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!(
                "token {} is ambiguous across harness stores",
                py_repr_str(token)
            )
        } else {
            detail
        });
    }
    if out.status.code() == Some(MISS) {
        return Ok(None);
    }
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let first = why
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        return Err(format!(
            "cannot safely resolve token {} because the all-source identity helper failed (exit {}){}. Use the full session id.",
            py_repr_str(token),
            out.status.code().unwrap_or(-1),
            if first.is_empty() { String::new() } else { format!(": {}", first.trim()) },
        ));
    }
    // The LAST non-empty line, not the whole buffer: a first-run `fno` may print
    // a setup-migration banner ahead of the payload.
    let text = String::from_utf8_lossy(&out.stdout);
    let line = match text.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(l) => l,
        None => {
            return Err(format!(
                "cannot safely resolve token {} because the all-source identity helper returned no row. Use the full session id.",
                py_repr_str(token)
            ))
        }
    };
    match serde_json::from_str::<Value>(line) {
        Ok(mut row) if row.is_object() => {
            // The healed row skipped `load_registry_entries`, so it gets neither
            // that loader's alias reconciliation nor its validation. Apply both:
            // without the backfill the row has no `claude_session_uuid` (resume's
            // dead arm would refuse); without the field bar, an exit-0 helper returning `{}` or a
            // partial object would resolve as a SUCCESS and surface as a confusing
            // missing-cwd error three frames later instead of a clean not-found.
            let obj = match row.as_object_mut() {
                Some(o) => o,
                None => unreachable!("object guard above"),
            };
            backfill_row_aliases(obj, false);
            let has_identity = is_identity_token(obj.get("harness").and_then(Value::as_str));
            let has_fields = ["name", "cwd", "log_path"]
                .iter()
                .all(|k| obj.contains_key(*k));
            if !has_identity || !has_fields {
                return Err(format!(
                    "cannot safely resolve token {} because the all-source identity helper returned an incomplete row. Use the full session id.",
                    py_repr_str(token)
                ));
            }
            Ok(Some(row))
        }
        _ => Err(format!(
            "cannot safely resolve token {} because the all-source identity helper returned malformed JSON. Use the full session id.",
            py_repr_str(token)
        )),
    }
}

/// [`find_agent_entry`], plus all-source resolution for session-shaped tokens.
///
/// The one choke point the session-connecting verbs resolve through. Returns an
/// OWNED row because the shared Python resolver may synthesize a healed row;
/// full ids and non-session-shaped registry hits clone their small local row.
/// Registry-gated trace calls this only after a local hit, so it gains the same
/// store-collision refusal without adopting a store-only row that has no events.
pub(crate) fn resolve_entry_with_heal(
    rows: &[Value],
    token: &str,
    registry_path: &Path,
) -> Result<Value, ResolveError> {
    match find_agent_entry(rows, token) {
        Ok(e) => {
            if entry_session_tier(e, token) == Some(0) || !is_session_shaped(token) {
                return Ok(e.clone());
            }
            match heal_token(token, registry_path) {
                Ok(Some(row)) => Ok(row),
                Ok(None) => Err(ResolveError::Ambiguous(format!(
                    "cannot safely resolve token {} because the harness stores could not be checked. Use the full session id.",
                    py_repr_str(token)
                ))),
                Err(candidates) => Err(ResolveError::Ambiguous(candidates)),
            }
        }
        // An ambiguous REGISTRY is not a miss: healing would pick the winner the
        // registry deliberately refused to pick.
        Err(err @ ResolveError::Ambiguous(_)) => Err(err),
        Err(err) => {
            if !is_session_shaped(token) {
                return Err(err);
            }
            match heal_token(token, registry_path) {
                Ok(Some(row)) => Ok(row),
                Ok(None) => Err(err),
                Err(candidates) => Err(ResolveError::Ambiguous(candidates)),
            }
        }
    }
}

/// Identity parsed from a `.fno/target-state.md` manifest, the durable evidence
/// source for adopting an orphaned `/target` session by its harness session id
/// (plan x-0358 US1). Pure (no IO) so the match and the field extraction are
/// tested without a live manifest. IDENTITY ONLY: the manifest's
/// `target_claim_*` / `owner_pid` fields are an init-time snapshot and are never
/// read as ownership or liveness truth (AGENTS.md pitfalls corpus, "Orienter
/// output, claim snapshots, and liveness probes have all lied").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ManifestIdentity {
    harness: String,
    harness_session_id: String,
    claude_session_id: String,
    codex_thread_id: String,
    owner_cwd: String,
    fno_id: String,
}

impl ManifestIdentity {
    /// The session-id fields, in canonical-first precedence.
    fn session_ids(&self) -> [&str; 3] {
        [
            self.harness_session_id.as_str(),
            self.claude_session_id.as_str(),
            self.codex_thread_id.as_str(),
        ]
    }

    /// Does `session_id` equal any harness-session-id the manifest records? The
    /// legacy `claude_session_id` / `codex_thread_id` aliases are kept for one
    /// release, so a pre-rename manifest still matches its session.
    fn matches(&self, session_id: &str) -> bool {
        let sid = session_id.trim();
        !sid.is_empty()
            && self
                .session_ids()
                .iter()
                .any(|f| !f.is_empty() && f.trim() == sid)
    }

    /// The id an adopted row is keyed on: canonical `harness_session_id` when
    /// present, else the legacy alias that carries it. init writes
    /// `harness_session_id: ${_HARNESS_SESSION:-null}`, so a real manifest can
    /// record the session under `claude_session_id` alone; keying on the
    /// canonical field alone would mint a row with an empty session id, empty
    /// short_id and the name `target-`.
    fn canonical_session_id(&self) -> &str {
        self.session_ids()
            .into_iter()
            .find(|f| !f.is_empty())
            .unwrap_or("")
    }
}

/// First non-empty wins (frontmatter precedes body); never overwrite a real
/// value with a later blank or an explicit `null`.
fn set_first(slot: &mut String, val: &str) {
    if slot.is_empty() && !val.is_empty() && val != "null" {
        *slot = val.to_string();
    }
}

/// Scan manifest content (frontmatter AND body) for the session-identity keys.
/// Lines inside the multi-line `input` quoted scalar are UNTRUSTED (their keys
/// never assign) so a `/target` argument containing `harness: ...` cannot forge
/// an identity field -- the same forgery surface `finalize::parse_manifest_fields`
/// guards for the merge posture. The manifest is not strict YAML, `fno do target
/// init` writes quoted scalars, so a line scan matches the writer rather than a
/// YAML lib. Kept here rather than folded into finalize because finalize owns
/// completion/merge fields and this owns session-identity fields; the scan is a
/// handful of lines and the two concerns stay decoupled.
///
/// The terminator line is itself untrusted but still ADVANCES the scan rather
/// than being consumed, exactly as finalize does: for an input ending in a lone
/// backslash the closing quote is ambiguous and the real terminator is the next
/// `plan_path: "..."` line. Consuming it (an unconditional skip) would leave the
/// scalar open to EOF and silently drop every identity key below `input:` --
/// init writes `input` before `harness`/`harness_session_id`/`owner_cwd`, so
/// adopt would report "no evidence" for a manifest that matches.
fn parse_manifest_identity(content: &str) -> ManifestIdentity {
    let mut m = ManifestIdentity::default();
    let mut in_input_scalar = false;
    for line in content.lines() {
        let line = line.trim();
        let line_untrusted = in_input_scalar;
        if in_input_scalar && line_closes_quoted_scalar(line) {
            in_input_scalar = false;
        }
        if line.is_empty() || line.starts_with('#') || line == "---" {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let raw = v.trim();
        // A multi-line `input: "..."` opens the scalar here. `len >= 2` so a bare
        // opening quote is not read as its own terminator.
        if !line_untrusted
            && k == "input"
            && raw.starts_with('"')
            && !(raw.len() >= 2 && line_closes_quoted_scalar(raw))
        {
            in_input_scalar = true;
        }
        if line_untrusted {
            continue;
        }
        let val = raw.trim_matches(|c| c == '"' || c == '\'');
        match k {
            "harness" => set_first(&mut m.harness, val),
            "harness_session_id" => set_first(&mut m.harness_session_id, val),
            "claude_session_id" => set_first(&mut m.claude_session_id, val),
            "codex_thread_id" => set_first(&mut m.codex_thread_id, val),
            "owner_cwd" => set_first(&mut m.owner_cwd, val),
            "fno_id" => set_first(&mut m.fno_id, val),
            _ => {}
        }
    }
    m
}

/// Does `raw` (the text after `input:`) close its quoted scalar on the same
/// line? Mirrors `finalize::ends_quoted_scalar`: a trailing quote with no
/// preceding backslash is the terminator, because init prepends exactly one
/// backslash to every user quote.
fn line_closes_quoted_scalar(raw: &str) -> bool {
    let Some(rest) = raw.strip_suffix('"') else {
        return false;
    };
    !rest.ends_with('\\')
}

// ---------------------------------------------------------------------------
// adopt: synthesize a registry entry from durable evidence (plan x-0358)
// ---------------------------------------------------------------------------

/// All non-bare git worktrees of the repo at `cwd`. The durable evidence for an
/// adopted /target orphan lives in `<worktree>/.fno/target-state.md`, so the
/// adopt path scans these. Mirrors [`crate::paths::canonical_repo_root`] but
/// returns every worktree, not just the main checkout. Empty outside a git repo
/// (callers also fall back to `cwd`).
/// claude's projects-dir slug for a cwd: both '/' and '.' replaced with '-'
/// (matches Python's `fno.provenance.resolver._slug`). Not reversible, so the
/// resume path resolves cwd by trying candidates and slug-checking rather than
/// decoding a slug back to a path.
fn claude_cwd_slug(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-").replace('.', "-")
}

/// The cwd `claude --resume <uuid>` must run in: the dir whose projects-dir
/// slug actually holds the transcript, not the (possibly stale) recorded
/// registration cwd. A session that ran `EnterWorktree` after registration has
/// its transcript under the worktree's project dir, while the recorded cwd is
/// the pre-`EnterWorktree` canonical - resuming there looks for the transcript
/// in the wrong dir and lands on the wrong branch.
///
/// Tries the recorded cwd then its git worktrees; the first whose
/// `<projects>/<slug>/<uuid>.jsonl` exists wins. Falls back to the recorded cwd
/// and names which branch was taken on stderr (a probe that does not name the
/// store it read is the trap the king's own SKILL.md warns about).
fn resolve_resume_cwd(claude_home: &ClaudeHome, recorded: &str, uuid: &str) -> PathBuf {
    if uuid.is_empty() {
        return PathBuf::from(recorded);
    }
    let projects = claude_home.projects_dir();
    let transcript = format!("{}.jsonl", uuid);
    // Probe the recorded cwd first: the common case (no EnterWorktree) keeps
    // the transcript under its own project dir, and a stat is far cheaper than
    // spawning `git worktree list` on every resume. Only on a miss do we
    // enumerate worktrees.
    let recorded_pb = PathBuf::from(recorded);
    let recorded_slug = claude_cwd_slug(&recorded_pb);
    if projects.join(&recorded_slug).join(&transcript).exists() {
        return recorded_pb;
    }
    let candidates: Vec<PathBuf> = git_worktree_paths(Path::new(recorded));
    for cand in &candidates {
        let slug = claude_cwd_slug(cand);
        if projects.join(&slug).join(&transcript).exists() {
            eprintln!(
                "fno agents resume: cwd resolved from the transcript's project dir ({})",
                cand.display()
            );
            return cand.clone();
        }
    }
    eprintln!(
        "fno agents resume: no transcript found under any candidate for {uuid}; \
         using the recorded cwd ({})",
        recorded
    );
    recorded_pb
}

fn git_worktree_paths(cwd: &Path) -> Vec<PathBuf> {
    let out = match std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = match String::from_utf8(out.stdout) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut paths = Vec::new();
    for record in stdout.split("\n\n") {
        let mut lines = record.lines();
        let first = match lines.next() {
            Some(l) => l,
            None => continue,
        };
        let path_str = match first.strip_prefix("worktree ") {
            Some(p) => p.trim(),
            None => continue,
        };
        if lines.any(|l| l.trim() == "bare") || path_str.is_empty() {
            continue;
        }
        paths.push(Path::new(path_str).to_path_buf());
    }
    paths
}

fn paths_eq(a: &Path, b: &Path) -> bool {
    // Both-unresolvable must NOT compare equal (`None == None`): two different
    // stale paths would read as the same directory.
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Find the `.fno/target-state.md` whose session id matches, scanning the cwd's
/// git worktrees (and cwd itself). Returns the parsed identity (cwd + fno_id +
/// the harness-appropriate session id) or `None` when no manifest matches.
/// Same-project by design; a cross-project orphan is not found here (the row's
/// `cwd` still links it once adopted by full id another way).
fn find_manifest_for_session(session_id: &str) -> Option<ManifestIdentity> {
    let cwd = std::env::current_dir().ok()?;
    let mut candidates = git_worktree_paths(&cwd);
    if !candidates.iter().any(|p| paths_eq(p, &cwd)) {
        candidates.push(cwd);
    }
    for wt in &candidates {
        let manifest = wt.join(".fno").join("target-state.md");
        let Ok(content) = fs::read_to_string(&manifest) else {
            continue;
        };
        let mut id = parse_manifest_identity(&content);
        if id.matches(session_id) {
            // `owner_cwd` is optional in the manifest schema; without it the
            // minted row has an empty cwd and `resume` refuses ("no recorded
            // cwd. Run `fno agents rm ...`") on the row it just wrote. The
            // worktree the manifest was found in IS that cwd.
            if id.owner_cwd.is_empty() {
                id.owner_cwd = wt.to_string_lossy().into_owned();
            }
            return Some(id);
        }
    }
    None
}

/// Collision-safe 8-char handle from a session id (the final-eight convention),
/// falling back to the whole trimmed id when shorter. The row's `short_id`, so
/// `peek`/`ask`/`resume` resolve the adopted orphan.
fn derived_short_id(session_id: &str) -> String {
    let s = session_id.trim();
    let len = s.chars().count();
    if len <= 8 {
        s.to_string()
    } else {
        s.chars().skip(len - 8).collect()
    }
}

/// Derivable, stable row name for a synthesized entry so re-adopting upserts one
/// row (the upsert keys on `harness_session_id`; the name is for display + name
/// addressing). `target-` tags the synthesis source (a /target orphan).
fn synthesized_name(short: &str) -> String {
    format!("target-{short}")
}

/// Build the registry row for an orphan adopted from a target manifest. Harness-
/// generic ([`crate::claude_adopt::mint_adopted_entry`] is claude+RosterWorker-
/// specific): the harness-appropriate session id comes from the manifest, claude
/// also records the full uuid for its dead-arm `claude --resume`, and `fno_id`
/// links the row to its node. `status: Idle`, no pid, default `exec` host_mode:
/// a registered-but-not-driven row the GC keeps (non-terminal, no confirmed-dead
/// pid -> `gc_action` Keep).
fn mint_synthesized_entry(id: &ManifestIdentity, now: &str) -> crate::state::RegistryEntry {
    use crate::state::RegistryEntry;
    let harness = if !id.harness.is_empty() {
        id.harness.clone()
    } else if id.harness_session_id.is_empty()
        && id.claude_session_id.is_empty()
        && !id.codex_thread_id.is_empty()
    {
        // No harness recorded and only the codex alias carries the session:
        // defaulting to claude would mint an unresumable row.
        "codex".to_string()
    } else {
        "claude".to_string()
    };
    let session = id.canonical_session_id().to_string();
    let short = derived_short_id(&session);
    let is_claude = harness == "claude";
    RegistryEntry {
        name: synthesized_name(&short),
        short_id: short,
        legacy_provider: String::new(),
        provider: None,
        model: None,
        effort: None,
        harness: Some(harness),
        harness_session_id: Some(session.clone()),
        cwd: id.owner_cwd.clone(),
        project_root: id.owner_cwd.clone(),
        session_id: None,
        claude_session_uuid: if is_claude { Some(session) } else { None },
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: None,
        status: crate::AgentStatus::Idle,
        last_message_at: Some(now.to_string()),
        created_at: now.to_string(),
        pid: None,
        pid_start_time: None,
        log_path: None,
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: if id.fno_id.is_empty() {
            None
        } else {
            Some(id.fno_id.clone())
        },
        delivery_policy: None,
        // Synthesized from a session identity that arrived without a row, so
        // nothing here observed how that session started. `None` is the honest
        // never-recorded, not a claim that no human is sitting in it.
        origin: None,
        spawn_trigger: None,
        legacy_claude_short_id: None,
    }
}

/// Upsert a synthesized row, keyed on the canonical `harness_session_id`
/// (covers claude too: its uuid syncs there). Reuses
/// [`crate::state::update_registry`] (the one locked writer) -- not a second
/// registry writer.
fn upsert_synthesized_row(
    registry_path: &Path,
    entry: crate::state::RegistryEntry,
) -> Result<(), crate::state::StateError> {
    crate::state::update_registry(registry_path, |reg| {
        let key = entry.harness_session_id.as_deref();
        let idx = key.and_then(|k| {
            reg.entries
                .iter()
                .position(|e| e.harness_session_id.as_deref() == Some(k))
        });
        match idx {
            Some(i) => {
                // Adopt knows IDENTITY, never runtime state. A row can already
                // exist under the canonical id while the operator adopts by a
                // legacy alias (`resolve_entry_with_heal` misses the alias), so
                // a wholesale replace would downgrade a live row to Idle and
                // drop its pid / log_path / mux. Liveness stays with reconcile.
                let mut merged = entry;
                let old = &reg.entries[i];
                merged.name = old.name.clone();
                merged.created_at = old.created_at.clone();
                merged.status = old.status;
                merged.pid = old.pid;
                merged.pid_start_time = old.pid_start_time;
                merged.log_path = old.log_path.clone();
                merged.host_mode = old.host_mode.clone();
                merged.mux = old.mux.clone();
                merged.exited_at = old.exited_at.clone();
                reg.entries[i] = merged;
            }
            None => reg.entries.push(entry),
        }
    })
}

/// Where an adoption's evidence came from (the receipt line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdoptSource {
    Registry,
    Manifest,
    HarnessStore,
}

impl AdoptSource {
    fn label(self) -> &'static str {
        match self {
            AdoptSource::Registry => "registry",
            AdoptSource::Manifest => "target manifest",
            AdoptSource::HarnessStore => "harness store",
        }
    }
}

/// Why an adoption did not complete.
#[derive(Debug)]
enum AdoptError {
    /// No evidence in any source.
    NoEvidence,
    /// A registry read/write or harness-store consultation failed.
    Io(String),
}

fn persist_manifest_identity(
    id: &ManifestIdentity,
    home: &AgentsHome,
) -> Result<Value, AdoptError> {
    let entry = mint_synthesized_entry(id, &crate::daemon::now_rfc3339_like());
    upsert_synthesized_row(&home.registry_json(), entry.clone())
        .map_err(|error| AdoptError::Io(error.to_string()))?;
    serde_json::to_value(&entry).map_err(|error| AdoptError::Io(error.to_string()))
}

/// Resolve `session_id` to one registry row, minting one if needed, through the
/// plan precedence: an existing registry row; a `.fno/target-state.md` whose
/// session id matches; then the harness session stores (the heal-token shellout,
/// which adopts best-effort). Identity only. Returns the row (as JSON), any
/// `fno_id` carried, and the source.
fn synthesize_and_adopt(
    session_id: &str,
    home: &AgentsHome,
) -> Result<(Value, Option<String>, AdoptSource), AdoptError> {
    let registry_path = home.registry_json();
    let entries = read_registry_entries(&registry_path).map_err(AdoptError::Io)?;
    // 1. Already registered (name / full id / short resolution, no store heal yet).
    if let Ok(e) = find_agent_entry(&entries, session_id) {
        let fno_id = e
            .get("fno_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok((e.clone(), fno_id, AdoptSource::Registry));
    }
    // 2. Target manifest.
    if let Some(id) = find_manifest_for_session(session_id) {
        let fno_id = (!id.fno_id.is_empty()).then(|| id.fno_id.clone());
        let value = persist_manifest_identity(&id, home)?;
        return Ok((value, fno_id, AdoptSource::Manifest));
    }
    // 3. Harness session stores (heal-token adopts best-effort and writes the row).
    match heal_token(session_id, &registry_path) {
        Ok(Some(row)) => Ok((row, None, AdoptSource::HarnessStore)),
        Ok(None) => Err(AdoptError::NoEvidence),
        Err(msg) => Err(AdoptError::Io(msg)),
    }
}

/// Manifest-only adoption used as the `resume` fallback: `resolve_entry_with_heal`
/// already consulted the registry + harness stores, so this is just the manifest
/// path. Returns the minted row (already upserted), `None` when no manifest
/// matches, or the actual registry/serialization failure.
fn adopt_from_manifest(session_id: &str, home: &AgentsHome) -> Result<Option<Value>, AdoptError> {
    let Some(id) = find_manifest_for_session(session_id) else {
        return Ok(None);
    };
    persist_manifest_identity(&id, home).map(Some)
}

/// Provider-specific resume argv, mirroring Python `_build_resume_argv`.
/// Returns `None` for unsupported providers.
fn build_resume_argv(provider: &str, session_id: &str, cwd: Option<&str>) -> Option<Vec<String>> {
    let mut argv = crate::harness_capabilities::render_session_argv(
        provider,
        "interactive_resume",
        Some(session_id),
    )
    .ok()?;
    // codex's bounded sandbox re-resolves from config on `resume` (it accepts
    // neither `--sandbox` nor `--add-dir`), so the git + plan grants must ride
    // as `-c` tokens spliced right after the `codex` binary token.
    if provider == "codex" {
        if let Some(cwd) = cwd {
            let grant = crate::provider::codex_writable_config_args(Path::new(cwd));
            if !grant.is_empty() {
                argv.splice(1..1, grant);
            }
        }
    }
    Some(argv)
}

fn interactive_resume_supported(provider: &str) -> bool {
    crate::harness_capabilities::HarnessContract::packaged()
        .ok()
        .and_then(|contract| {
            contract.capabilities(provider).ok().and_then(|caps| {
                caps.resume_strategy
                    .forms
                    .get("interactive_resume")
                    .map(|form| form.kind != "unsupported")
            })
        })
        .unwrap_or(false)
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
    claude_resume_argv_with_truth(claude_home, entry, name, family1_truth_state_for_resume)
}

fn claude_resume_argv_with_truth<F>(
    claude_home: &ClaudeHome,
    entry: &Value,
    name: &str,
    truth_fn: F,
) -> Result<(Vec<String>, Option<String>), i32>
where
    F: Fn(&str) -> Option<String>,
{
    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    let uuid = entry
        .get("claude_session_uuid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let has_uuid = is_uuid_shaped(uuid);

    let socket_live = !short_id.is_empty()
        && locate_session(claude_home, short_id)
            .map(|loc| liveness_probe(&loc.messaging_socket_path))
            .unwrap_or(false);
    // Probe on the canonical uuid whenever one is recorded. This used to also
    // short-circuit on an empty short_id, so a pane worker (no short_id by
    // design: _validate_single_live_ref enforces mux XOR worker XOR bg) never
    // probed and reported "liveness is inconclusive" for a session whose uuid
    // was resolvable - the x-b84f bug. The attach arm below gates on a present
    // short_id, so dropping the short_id term lets a mux row probe without ever
    // issuing a bare `claude attach ""`.
    let truth_state = if socket_live || uuid.is_empty() {
        None
    } else {
        truth_fn(uuid)
    };
    let live = socket_live
        || matches!(
            truth_state.as_deref(),
            Some("working" | "watching" | "your-move")
        );
    let dead = matches!(truth_state.as_deref(), Some("done" | "stalled"));

    if live && !short_id.is_empty() {
        // Deliberately silent on mechanism: the caller (`run_resume`) decides
        // AFTER this returns whether the row gets --print-command'd, the
        // Python headless wake-and-verify delegation, or (a mux pane row
        // never reaches this arm, so that leaves) nothing else -- an
        // "attaching" claim printed here was true when this arm always led
        // to a bare `claude attach` exec, and stayed on the screen after the
        // delegation replaced that exec with a wake that never attaches at
        // all. The caller's own downstream output (the printed command, or
        // fno-py's before -> after line) is what actually describes what
        // happened.
        eprintln!("fno agents resume: {name} is live");
        Ok((
            vec!["claude".into(), "attach".into(), short_id.into()],
            None,
        ))
    } else if dead && has_uuid {
        // x-ae2d: this arm RELAUNCHES (the live arm above only attaches), so it
        // is the one door on this verb that can lose a route. A row that records
        // one gets it re-applied through `--settings`, the same mechanism the
        // original spawn used; a recorded file that is gone refuses rather than
        // relaunching on the default Anthropic account, which works, bills the
        // wrong vendor, and reports nothing.
        let route_settings = entry
            .get("route_settings_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let mut argv: Vec<String> = vec!["claude".into()];
        if let Some(path) = route_settings {
            // Present is not enough. The file is the auth-scrub floor with the
            // route written on top, and claude reads an empty settings value as
            // UNSET - so a floor-only or malformed file hands claude a settings
            // file that selects nothing and the worker comes back on the default
            // account in silence. That is the same outcome as a missing file, so
            // it takes the same refusal. Python's `read_route_settings` applies
            // the identical rule; a check here that only tested existence would
            // make these two doors disagree while the docs call them equivalent.
            let usable = fs::read_to_string(path).ok().and_then(|raw| {
                serde_json::from_str::<Value>(&raw).ok().map(|v| {
                    v.get("env").and_then(Value::as_object).is_some_and(|env| {
                        env.values()
                            .any(|x| x.as_str().is_some_and(|s| !s.is_empty()))
                    })
                })
            });
            if usable != Some(true) {
                let why = match usable {
                    None => "cannot be read as a route settings file",
                    _ => "records no route",
                };
                eprintln!(
                    "fno agents resume: {name} was launched on the route recorded at \
                     {path}, and it {why}; refusing to relaunch it on the default \
                     account. Re-spawn with an explicit --route/-P to choose one."
                );
                return Err(13);
            }
            eprintln!("fno agents resume: restoring recorded route from {path}");
            argv.push("--settings".into());
            argv.push(path.into());
        }
        eprintln!("fno agents resume: {name} has exited - resuming in your terminal");
        argv.push("--resume".into());
        argv.push(uuid.into());
        Ok((argv, Some(uuid.to_string())))
    } else if !has_uuid {
        // No resumable uuid and no live socket to attach through: name the cause.
        // AC2: an id-less row is a definite "nothing to resume", never the
        // "liveness is inconclusive" that printed an unrunnable empty-id hint and
        // hid the real bug.
        eprintln!("fno agents resume: {name} has no session id recorded; nothing to resume.");
        Err(13)
    } else if live {
        // Probe-live but no short_id to attach through: a pane/mux worker that
        // is already running. There is no resume action here - `claude attach`
        // needs a short_id this row does not carry, and relaunching would open a
        // second writer on one transcript. Do not call this "inconclusive": the
        // probe just answered live, and the old hint sent the operator to re-run
        // a probe whose answer contradicts the message.
        eprintln!(
            "fno agents resume: {name} is live but has no attach short_id \
             (a pane worker); it is already running - drive it via its mux session, \
             or re-spawn with `fno agents spawn`."
        );
        Err(13)
    } else {
        // has_uuid but neither attachable-live nor affirmatively dead: genuinely
        // inconclusive (a silent-unreachable worker that may still be alive).
        // Name the uuid the operator can probe, not the empty short_id the old
        // hint interpolated.
        eprintln!(
            "fno agents resume: {name} liveness is inconclusive; refusing to open a second writer. Run 'fno agents truth {uuid}'."
        );
        Err(13)
    }
}

/// Build the `mux pane run` argv (everything after the `fno` binary) that
/// relaunches `claude_argv` on a new pane in `session` at `cwd`. The `--` fence
/// keeps a `--resume <uuid>` (or any flag-shaped inner arg) out of the mux
/// parser, so the resumed command is transported verbatim - the one-verb form
/// of the manual `fno mux pane run 'cd <wt> && exec claude --resume <uuid>'`
/// recovery recipe (x-b84f D3).
fn mux_pane_run_argv(session: &str, cwd: &str, claude_argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "mux".into(),
        "pane".into(),
        "run".into(),
        "--session".into(),
        session.into(),
        "--cwd".into(),
        cwd.into(),
        "--".into(),
    ];
    v.extend(claude_argv.iter().cloned());
    v
}

/// Acquire the `session:<uuid>` single-writer claim for an interactive dead-row
/// resume, anchored to THIS process. `exec` keeps the pid, so the claim is held
/// by the resumed claude and self-releases when the operator quits (no explicit
/// release). Two racing resumers both probe dead, but only one wins this atomic
/// claim; the loser gets `Err` and refuses instead of opening a second writer on
/// one transcript - the residual double-writer window the liveness probe alone
/// cannot close. `root` is `None` in prod (session: keys route to
/// `$FNO_CLAIMS_ROOT`/`$HOME`); tests inject a temp root.
/// How long the session single-writer claim guards a mux-pane relaunch.
/// The launching process exits once the pane is up, so the claim cannot ride
/// the holder pid the way the in-terminal exec's does (a PID-only claim goes
/// Stale the moment that pid dies, so a second resumer would steal it before
/// the resumed claude is probe-live). This TTL keeps the claim Live across
/// that launch-to-probe-live window; once claude is probe-live the truth probe
/// (not this claim) stops a second relaunch. Picked wide against slow startup;
/// after it expires, a crashed worker can be re-resumed rather than blocked.
const MUX_RESUME_CLAIM_TTL_MS: u64 = 120_000;

fn acquire_resume_session_claim(
    uuid: &str,
    root: Option<&Path>,
    ttl_ms: Option<u64>,
) -> Result<(), (i32, String)> {
    acquire_named_session_claim(&format!("session:{uuid}"), uuid, root, ttl_ms)
}

/// Used directly by the dead-row `claude --resume` relaunch (keyed
/// `session:{uuid}`). The live-row headless wake uses the matching
/// `resume-attach:{short_id}` key too, but acquires it Python-side
/// (`resume_cli.py`'s `_resume_claude_wake`, gated on skip-eligibility) --
/// this Rust arm delegates the wake itself and does not call this function
/// for that key. Two different key prefixes by design: a live wake and a
/// dead relaunch are mutually exclusive outcomes of one truth-state read,
/// never racing each other for the same row, but two concurrent resumes
/// both landing on the SAME arm for the same row do race -- each key only
/// needs to guard against its own arm's double-writer.
fn acquire_named_session_claim(
    key: &str,
    label: &str,
    root: Option<&Path>,
    ttl_ms: Option<u64>,
) -> Result<(), (i32, String)> {
    use crate::claims::{acquire, AcquireOpts, AcquireOutcome};
    let holder = format!("resume:{}", std::process::id());
    let opts = AcquireOpts {
        root: root.map(Path::to_path_buf),
        reason: Some("interactive resume single-writer".to_string()),
        ttl_ms: ttl_ms.map(|t| t as i64),
        ..Default::default()
    };
    match acquire(key, &holder, opts) {
        AcquireOutcome::Acquired(_) => Ok(()),
        AcquireOutcome::HeldByOther { holder, pid, host } => Err((
            11,
            format!(
                "fno agents resume: session {label} is held live by another writer \
                 ({holder}, pid={pid}, host={host}); not opening a second writer on one transcript."
            ),
        )),
        AcquireOutcome::Error(e) => Err((
            12,
            format!("fno agents resume: could not claim session {label}: {e}"),
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
    claude_attach_pointer_with_truth(claude_home, entry, name, family1_truth_state)
}

fn claude_attach_pointer_with_truth<F>(
    claude_home: &ClaudeHome,
    entry: &Value,
    name: &str,
    truth_fn: F,
) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    let uuid = entry
        .get("claude_session_uuid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if short_id.is_empty() || !is_uuid_shaped(uuid) {
        return None;
    }
    let socket_live = locate_session(claude_home, short_id)
        .map(|loc| liveness_probe(&loc.messaging_socket_path))
        .unwrap_or(false);
    if socket_live {
        return None;
    }
    if !matches!(truth_fn(uuid).as_deref(), Some("done" | "stalled")) {
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

/// `fno mux pane run` exit code when the mux never answered the control read
/// (crates/fno `EXIT_CONTROL_UNANSWERED`). Duplicated here rather than
/// imported: this crate does not depend on `fno`, and `fno mux pane run` is
/// invoked as a subprocess, not a library call.
const MUX_CONTROL_UNANSWERED: i32 = 20;

/// Map a failed `fno mux pane run` (the resume launcher's subprocess) to its
/// stderr message. Kept pure so the exit-code split is mechanically testable
/// without mutating PATH or shelling out (parse_heal_token_output's pattern).
///
/// `EXIT_CONTROL_UNANSWERED` gets the truthful "may have started" message:
/// the verb REACHED the server, so "(no pane started)" is false for this
/// code alone. Adoption stays in the Python spawn path's
/// `_reconcile_unanswered_run` (one place owns candidate matching); this
/// launcher only names the inspect command, on the same reasoning.
fn mux_pane_run_failure_message(
    name: &str,
    session: &str,
    status: std::process::ExitStatus,
) -> String {
    if status.code() == Some(MUX_CONTROL_UNANSWERED) {
        return format!(
            "fno agents resume: the mux never answered the run for {name}; a pane \
             MAY have started. Check `fno mux pane ls --session {session}` before \
             retrying."
        );
    }
    format!(
        "fno agents resume: mux pane run for {name} exited {} (no pane started)",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string())
    )
}

/// True for the one `claude_resume_argv_with_truth` arm that returns
/// `(["claude", "attach", short_id], None)`: a live, short_id-addressable
/// claude row with no mux ref. `claim_uuid` is `Some` only on the dead-relaunch
/// arm; every other arm already returns `Err` before this is checked.
fn should_delegate_claude_live_attach(
    harness: &str,
    claim_uuid: &Option<String>,
    mux_session: &Option<String>,
) -> bool {
    harness == "claude" && claim_uuid.is_none() && mux_session.is_none()
}

/// `fno-agents resume <name> [--print-command]` -- resume an agent in its
/// recorded cwd via the provider's resume CLI (`os.execvp` equivalent), or
/// print the shell snippet with `--print-command`. Mirrors Python `resume_logic`.
/// Pure parse of `resume`'s argv: `NAME [--print-command] [--message|-m VALUE]`.
/// `--message`/`-m` (Python's `cmd_resume`) only matters on the claude
/// live-attach delegation in `run_resume`, but it must be ACCEPTED here or
/// every `fno agents resume <name> --message ...` invocation dies with exit 2
/// before that delegation is ever reached -- resume auto-routes to this
/// binary by default (`RUST_CLIENT_VERBS`), so this parser is the only door.
/// Extracted as a pure function (mirrors `should_delegate_claude_live_attach`)
/// so the flag grammar is unit-testable without an `AgentsHome`/registry
/// fixture; on error it prints the same diagnostic `run_resume` used to print
/// inline and returns the exit code to propagate.
fn parse_resume_args(rest: &[String]) -> Result<(String, bool, Option<String>), i32> {
    let mut name: Option<String> = None;
    let mut print_command = false;
    let mut message: Option<String> = None;
    // Every sibling parser in this file (`parse_trace_args`, `parse_logs_args`)
    // expands `--flag=value` into `--flag value` before iterating; without it
    // `--message=continue` falls into the `starts_with("--")` unknown-flag arm
    // instead of being recognized.
    let rest = expand_eq(rest);
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--print-command" => print_command = true,
            "--message" | "-m" => {
                message = Some(match iter.next() {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("fno-agents: {a} needs a value");
                        return Err(2);
                    }
                });
            }
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown resume flag: {other}");
                return Err(2);
            }
            other => {
                if name.is_some() {
                    eprintln!("fno-agents: resume takes one NAME (got extra: {other})");
                    return Err(2);
                }
                name = Some(other.to_string());
            }
        }
    }
    match name {
        Some(n) => Ok((n, print_command, message)),
        None => {
            eprintln!("fno-agents: resume needs a <name>");
            Err(2)
        }
    }
}

pub fn run_resume(rest: &[String], home: &AgentsHome) -> i32 {
    let (name, print_command, message) = match parse_resume_args(rest) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let entries = match read_registry_entries(&home.registry_json()) {
        Ok(e) => e,
        Err(exc) => {
            eprintln!("fno agents resume: registry read failed: {exc}");
            return 13;
        }
    };
    let entry = match resolve_entry_with_heal(&entries, &name, &home.registry_json()) {
        Ok(e) => e,
        Err(err) => {
            // Session-shaped miss: try adopting from a target manifest (the durable
            // evidence heal-token does not consult) before refusing, so
            // `fno agents resume <session-id>` revives a /target orphan. A plain
            // name keeps today's refusal (AC7-HP: byte-identical for name args).
            let adopted = if is_session_shaped(&name) {
                adopt_from_manifest(&name, home)
            } else {
                Ok(None)
            };
            match adopted {
                Ok(Some(e)) => e,
                // `adopt_from_manifest` only ever returns `Ok(None)` or
                // `Err(Io)` -- `NoEvidence` is `synthesize_and_adopt`'s
                // variant, unreachable through this call, kept here only for
                // exhaustiveness over `AdoptError`.
                Ok(None) | Err(AdoptError::NoEvidence) => {
                    eprintln!(
                        "fno agents resume: {}. Use `fno agents list` to see registered agents, or pass a full session id to resume an orphaned session.",
                        err.message()
                    );
                    return 13;
                }
                Err(AdoptError::Io(message)) => {
                    eprintln!("fno agents resume: manifest adoption failed: {message}");
                    return 13;
                }
            }
        }
    };
    let entry = &entry;

    // Identity is one axis (x-8dfc): resume keys on harness (provider fallback
    // for a not-yet-backfilled row), and the exit-13 errors name the harness,
    // matching Python resume_cli. harness == provider on every current row.
    let harness = entry
        .get("harness")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| entry.get("provider").and_then(Value::as_str))
        .unwrap_or("");
    let recorded_cwd = entry.get("cwd").and_then(Value::as_str).unwrap_or("");
    // claude keys transcript dirs by the session cwd, so a session that ran
    // EnterWorktree after registration has its transcript under a different
    // project dir than the recorded (pre-EnterWorktree) cwd. Resolve the cwd
    // from where the transcript actually is; other harnesses keep the recorded.
    let resolved_cwd = if harness == "claude" {
        let claude_uuid = entry
            .get("claude_session_uuid")
            .and_then(Value::as_str)
            .unwrap_or("");
        resolve_resume_cwd(&ClaudeHome::from_env(), recorded_cwd, claude_uuid)
            .to_string_lossy()
            .into_owned()
    } else {
        recorded_cwd.to_string()
    };
    let cwd: &str = &resolved_cwd;
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
    // <uuid>`. Other harnesses keep their settled-session resume CLI. Check
    // support before session_id so an unknown harness surfaces "not supported",
    // then check identity before rendering so a supported harness with no bound
    // session reports the missing binding instead of an invalid argv.
    let (argv, claim_uuid) = if harness == "claude" {
        match claude_resume_argv(&ClaudeHome::from_env(), entry, &name) {
            Ok(plan) => plan,
            Err(code) => return code,
        }
    } else {
        if !interactive_resume_supported(harness) {
            eprintln!(
                "fno agents resume: harness {} resume not supported by this fno version.",
                py_repr_str(harness)
            );
            return 13;
        }
        if session_id.is_empty() {
            eprintln!(
                "fno agents resume: agent {} has no recorded session_id for harness {}.",
                py_repr_str(&name),
                py_repr_str(harness)
            );
            return 13;
        }
        let v = match build_resume_argv(harness, session_id, Some(cwd)) {
            Some(v) => v,
            None => {
                eprintln!(
                    "fno agents resume: harness {} resume contract is invalid.",
                    py_repr_str(harness)
                );
                return 13;
            }
        };
        (v, None)
    };

    // A pane (mux) row carries the session it was launched on; resume puts the
    // worker back THERE via `fno mux pane run`, not in this terminal, so the
    // operator keeps their shell and the resumed session stays drivable from the
    // mux (x-b84f D3). A row with no mux ref keeps the in-terminal exec.
    let mux_session = entry
        .get("mux")
        .and_then(|m| m.get("session"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if !which_on_path(&argv[0]) {
        eprintln!("fno agents resume: {} CLI not on PATH", argv[0]);
        return 14;
    }

    if print_command {
        if let Some(session) = mux_session.as_deref() {
            // Pane form: `fno mux pane run ... -- claude ...`. Path only; nothing
            // from inside the route file reaches the printed command (AC5).
            let pane = mux_pane_run_argv(session, cwd, &argv);
            let pane_q = pane
                .iter()
                .map(|a| shlex_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            println!("fno {}", pane_q);
        } else {
            let argv_q = argv
                .iter()
                .map(|a| shlex_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            println!("cd {} && exec {}", shlex_quote(cwd), argv_q);
        }
        return 0;
    }

    // Validate cwd for ALL paths before claiming, delegating, or launching. A
    // deleted worktree is a cleanup job, not a resume. The exec path
    // re-checks via set_current_dir below (race-free for its own chdir), but
    // bailing here means a gone cwd never acquires the session claim, and
    // never burns a wake attempt, on the failure path.
    if !Path::new(cwd).is_dir() {
        eprintln!(
            "fno agents resume: cwd {} for {} is no longer reachable. Run `fno agents rm {}` to clean up.",
            py_repr_str(cwd),
            py_repr_str(&name),
            name
        );
        return 13;
    }

    // Live claude row (short_id, no mux ref): `claim_uuid` is None only on
    // this arm (the dead-relaunch arm above sets Some(uuid); the two error
    // arms already returned). A bare `claude attach` exec here has no pty,
    // no route-settings restore, and no post-exec verification -- the same
    // gap the Python fallback (`resume_cli.py::_resume_claude_wake`) closed.
    // Delegating to it rather than re-deriving that pty/bracketed-paste/
    // retry recipe natively keeps ONE implementation instead of two: a fix
    // landed only here would leave `fno agents resume` (this binary) fixed
    // and `fno-agents resume` still printing "Attaching..." and exiting,
    // which is the guard-on-one-of-N-paths trap this repo already tracks.
    if should_delegate_claude_live_attach(harness, &claim_uuid, &mux_session) {
        // No claim acquired here (unlike the dead-relaunch arm below):
        // acquiring it unconditionally, before knowing whether the row is
        // even skip-eligible (already Working/Idle/Done, needing no wake at
        // all), raced two concurrent no-op resumes into a spurious "held by
        // another writer" on a lock that guards a pty write neither was
        // making. That skip decision requires the live-status truth-read
        // this arm deliberately does not duplicate (see above); re-deriving
        // it here just to gate the claim would be the same duplicate-truth
        // problem this delegation exists to avoid. `resume_cli.py`'s own
        // `_resume_claude_wake` acquires the identical `resume-attach:
        // {short_id}` key itself, gated on that same skip check, once exec'd
        // below -- the wake this arm delegates to stays guarded either way,
        // whether reached through this Rust delegation or as the standalone
        // Python entrypoint (FNO_AGENTS_RUNTIME=python, no Rust binary
        // installed), which never runs this arm at all.
        // Route through `fno` (the wrapper every install puts on PATH, which
        // resolves the `fno-py` console script by absolute path -- see
        // crates/fno/src/bootstrap.rs), not a bare `fno-py`: this binary has
        // no PATH-robust resolver of its own, and a bare `fno-py` fails on a
        // cargo-only install where only the mux (`fno`) is on PATH (the same
        // gap cli/src/fno/_subprocess_util.py's `fno_py_cmd()` closes on the
        // Python side). `FNO_AGENTS_RUNTIME=python` pins the child to Python
        // dispatch, mirroring `token_helper_output` above -- without it,
        // `resume` (in RUST_CLIENT_VERBS) would auto-route straight back into
        // this same binary and loop.
        //
        // exec(), not status(): this REPLACES the process rather than
        // spawning a child, matching every other `Command::new("fno")...exec()`
        // delegation in this crate (`bin/client.rs:523-534`, `:544-554`) --
        // same exit-127-on-failure convention, and it sidesteps process-group
        // signal-propagation questions a spawned child would raise, since
        // there is no separate child to propagate a signal to.
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("fno");
        command
            // --cwd carries the EnterWorktree-resolved cwd computed above
            // (`resolved_cwd`), not the raw registry value: Python has no
            // equivalent of `resolve_resume_cwd` and would otherwise re-derive
            // the stale pre-EnterWorktree cwd from the registry entry itself.
            .args(["agents", "resume", &name, "--cwd", cwd])
            .env("FNO_AGENTS_RUNTIME", "python");
        if let Some(msg) = &message {
            command.args(["--message", msg]);
        }
        let err = command.exec();
        eprintln!(
            "fno agents resume: delegating {name} to fno-py failed: {err}. \
             Install the fno front door or run `fno-py agents resume {name}` directly."
        );
        return 127;
    }

    // Guard a dead-row `claude --resume` with the session single-writer claim
    // before exec (--print-command already returned above, so it never claims).
    // The in-terminal exec keeps this pid, so a PID-only claim (ttl=None) lives
    // as long as claude does. The mux path exits after pane dispatch, so it
    // passes a TTL: without one the claim would go Stale on the dead holder and
    // a second resumer would steal it before the resumed claude is probe-live.
    if let Some(uuid) = &claim_uuid {
        let ttl = if mux_session.is_some() {
            Some(MUX_RESUME_CLAIM_TTL_MS)
        } else {
            None
        };
        if let Err((code, msg)) = acquire_resume_session_claim(uuid, None, ttl) {
            eprintln!("{msg}");
            return code;
        }
    }

    // Pane relaunch (claude only): the mux owns the cwd (--cwd) and the pane,
    // and this process returns after the launch so the operator's terminal
    // stays free. stdin is null'd so a mux pane run that reads stdin cannot
    // stall against this terminal. The claim above carries a TTL (not a pid) on
    // this path, so it stays Live across the launch-to-probe-live window; once
    // the resumed claude is probe-live the truth probe (not the claim) stops a
    // second relaunch. Emit only on a successful launch so a failed pane start
    // does not record a misleading agent_resumed.
    //
    // Scoped to claude: the session claim that guards this path is claude-only,
    // and launching a non-claude pane worker on an unguarded pane would widen
    // that pre-existing no-claim gap. A non-claude pane row falls through to
    // the in-terminal exec below (its prior behavior).
    let mux_session = if harness == "claude" {
        mux_session
    } else {
        None
    };
    if let Some(session) = mux_session.as_deref() {
        let pane = mux_pane_run_argv(session, cwd, &argv);
        match std::process::Command::new("fno")
            .args(&pane)
            .stdin(std::process::Stdio::null())
            .status()
        {
            Ok(s) if s.success() => {
                // session_id is the transport short_id, empty on a pane row;
                // the resumed session's id is the uuid (claim_uuid).
                let resumed_id = claim_uuid.as_deref().unwrap_or(session_id);
                append_agents_event(
                    &trace_events_path(home),
                    "agent_resumed",
                    &[
                        ("name", Value::String(name.clone())),
                        ("provider", Value::String(harness.to_string())),
                        ("session_id", Value::String(resumed_id.to_string())),
                        ("cwd", Value::String(cwd.to_string())),
                    ],
                );
                eprintln!("fno agents resume: {name} relaunched on mux session {session}");
                return 0;
            }
            Ok(s) => {
                eprintln!("{}", mux_pane_run_failure_message(&name, session, s));
                return 1;
            }
            Err(e) => {
                eprintln!("fno agents resume: failed to launch {name} on a mux pane: {e}");
                return 1;
            }
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
// adopt
// ---------------------------------------------------------------------------

/// `fno-agents adopt <session-id>` -- register an orphaned session (known only by
/// its session id) so it is addressable by `peek`/`ask`/`resume`/mail. Exit 13 on
/// no evidence (naming the sources searched), 2 on bad args. The row is minted
/// through the existing registry writer; adopt takes NO single-writer claim (its
/// process is transient -- the claim is acquired by `resume`'s dead arm, whose
/// pid survives exec; a transient-pid claim would recreate the reanchor bug).
pub fn run_adopt(rest: &[String], home: &AgentsHome) -> i32 {
    let mut session_id: Option<String> = None;
    for a in rest {
        match a.as_str() {
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown adopt flag: {other}");
                return 2;
            }
            other => {
                if session_id.is_some() {
                    eprintln!("fno-agents: adopt takes one SESSION_ID (got extra: {other})");
                    return 2;
                }
                session_id = Some(other.to_string());
            }
        }
    }
    let session_id = match session_id {
        Some(s) => s,
        None => {
            eprintln!("fno-agents: adopt needs a <session-id>");
            eprintln!(
                "fno agents adopt: accepts a harness session id; resolves the registry, .fno/target-state.md, then harness stores."
            );
            return 2;
        }
    };

    match synthesize_and_adopt(&session_id, home) {
        Ok((row, fno_id, source)) => {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("");
            let short = row.get("short_id").and_then(Value::as_str).unwrap_or("");
            // Name on stdout so it composes; provenance + ids on stderr.
            println!("{name}");
            if !short.is_empty() {
                eprintln!(
                    "fno agents adopt: short_id={short} (resolved from {})",
                    source.label()
                );
            }
            if let Some(fid) = fno_id {
                if !fid.is_empty() {
                    eprintln!("fno agents adopt: fno_id={fid}");
                }
            }
            0
        }
        Err(AdoptError::NoEvidence) => {
            eprintln!(
                "fno agents adopt: no evidence for session {}. Searched: the registry, .fno/target-state.md across git worktrees, and the harness session stores. Pass the full harness session id, or `fno agents list` to see registered agents.",
                py_repr_str(&session_id)
            );
            13
        }
        Err(AdoptError::Io(msg)) => {
            eprintln!("fno agents adopt: could not adopt {session_id}: {msg}");
            13
        }
    }
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
    let entry = match resolve_entry_with_heal(&entries, &name, &home.registry_json()) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}", err.message());
            return 2;
        }
    };
    let entry = &entry;

    let harness = entry
        .get("harness")
        .and_then(Value::as_str)
        .or_else(|| entry.get("provider").and_then(Value::as_str))
        .unwrap_or("");
    let events_path = trace_events_path(home);

    // Every non-claude harness refuses attach (claude is the only harness
    // with a persistent `--bg` session to attach to). `!= "claude"` instead of
    // an allowlist so a provider added to the roster inherits the refusal
    // rather than falling through to a claude-shaped attach (x-51f6 US1).
    if harness != "claude" {
        eprintln!(
            "{harness} agents are one-shot; no persistent session to attach to. Use 'fno agents logs {name} --follow' for live output. Cross-provider attach is planned for the Phase 6 supervisor."
        );
        append_agents_event(
            &events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.clone())),
                ("provider", Value::String(harness.to_string())),
                (
                    "reason",
                    Value::String("one-shot-provider-no-persistent-session".to_string()),
                ),
            ],
        );
        return 13;
    }

    if harness != "claude" {
        eprintln!(
            "attach for harness {} is not implemented",
            py_repr_str(harness)
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
    let entry = match resolve_entry_with_heal(&entries, &args.name, &home.registry_json()) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}", err.message());
            return 13;
        }
    };
    let entry = &entry;
    let harness = entry
        .get("harness")
        .and_then(Value::as_str)
        .or_else(|| entry.get("provider").and_then(Value::as_str))
        .unwrap_or("");

    if harness == "claude" {
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
            "no logs for {harness} agent {}: no log file at {where_}",
            args.name
        );
        return 13;
    }

    // Arm SIGINT BEFORE the one-shot tail block, never inside `follow`. The
    // block below is printed for a `--follow` invocation too, and arming inside
    // `follow` left it unprotected: a Ctrl-C landing there took the default
    // handler and killed the process at 130, while the Python twin exits clean.
    // A guard on one of two reachable paths for one verb is decorative, so the
    // receiver is armed here and threaded in.
    //
    // Arming replaces the default disposition, so a Ctrl-C during the tail read
    // is DEFERRED rather than lost: it fires the moment `follow`'s select! runs,
    // which is the clean exit this wants. It is dropped only when the tail read
    // itself fails and we return 1 below. Reading a queued signal on that branch
    // needs a manual poll of the Signal stream, which is more machinery than an
    // error path already printing its cause deserves.
    let sigint = if args.follow {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok()
    } else {
        None
    };

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
        return crate::logs_client::follow(home, resolved_name, sigint).await;
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
/// slicing (mirrors `harnesses.claude.logs`).
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
/// report to), a wedged-but-live daemon that never answers is exit 0 too (the
/// report is lost, not the turn), a successful store/drop is exit 0; only a
/// malformed invocation (exit 2) or a real transport error (exit 1) is loud,
/// so a per-turn hook never reds a turn. Bounded at its OWN short deadline,
/// not `client::RESPONSE_DEADLINE` -- that one is sized for the human-facing
/// blocking rm/stop RPCs, and inheriting it here would let a wedged daemon
/// stall a turn for a minute-plus before this call's own permissive fallback
/// ever gets to run.
const REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn run_report(rest: &[String], home: &AgentsHome) -> i32 {
    let params = match build_report_params(rest) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };
    let req = crate::protocol::Request::new(1, "agent.report", params);
    match tokio::time::timeout(REPORT_TIMEOUT, crate::client::call_if_running(home, &req)).await {
        // Our own short deadline elapsed, or the client's longer one did
        // (defense in depth: correct either way, never reached today since
        // REPORT_TIMEOUT is far shorter) -- both are a lost report, not a
        // turn failure.
        Err(_) => 0,
        Ok(Ok(_)) => 0,
        Ok(Err(crate::client::ClientError::DaemonNotRunning)) => 0,
        Ok(Err(crate::client::ClientError::DaemonUnresponsive { .. })) => 0,
        Ok(Err(e)) => {
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
                // Liveness compares this, not host. Omitting it here would leave a
                // caller that classifies ownership from status JSON on the mutable
                // hostname, so the fix would not reach that path at all.
                out.insert(
                    "machine_id".into(),
                    rec.machine_id.map(Value::from).unwrap_or(Value::Null),
                );
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

    const CLAUDE_UUID_FIXTURE: &str = "a1b2c3d4-1111-2222-3333-444455556666";

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
    fn find_agent_entry_daemon_and_canonical_handle_both_resolve() {
        // AC2-HP: a codex row resolves by its name-derived daemon short AND by
        // the canonical random tail of its thread id.
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
            find_agent_entry(&rows, "55556666").unwrap()["name"],
            "reviewer"
        );
    }

    #[test]
    fn canonical_handle_and_legacy_prefix_are_ambiguous() {
        let canonical = claude_row(
            "canonical",
            "transport1",
            "ffffffff-0000-0000-0000-abcd1234",
        );
        let legacy_a = claude_row("legacy-a", "transport2", "abcd1234-0000-0000-0000-11111111");
        assert!(matches!(
            find_agent_entry(&[legacy_a.clone(), canonical], "abcd1234"),
            Err(ResolveError::Ambiguous(_))
        ));
        let legacy_b = claude_row("legacy-b", "transport3", "abcd1234-0000-0000-0000-22222222");
        assert!(matches!(
            find_agent_entry(&[legacy_a, legacy_b], "abcd1234"),
            Err(ResolveError::Ambiguous(_))
        ));
    }

    #[test]
    fn find_agent_entry_name_and_short_id_collision_is_ambiguous() {
        let rows = vec![
            claude_row(
                "deadbeef",
                "aaaa0000",
                "aaaa0000-0000-0000-0000-000000000000",
            ),
            claude_row("other", "deadbeef", "deadbeef-1111-1111-1111-111111111111"),
        ];
        assert!(matches!(
            find_agent_entry(&rows, "deadbeef"),
            Err(ResolveError::Ambiguous(_))
        ));
    }

    #[test]
    fn find_agent_entry_duplicate_name_distinct_sessions_is_ambiguous() {
        let rows = vec![
            claude_row("same", "transport1", "aaaaaaaa-1111-7222-8333-4444deadbeef"),
            claude_row("same", "transport2", "bbbbbbbb-1111-7222-8333-4444cafefeed"),
        ];

        assert!(matches!(
            find_agent_entry(&rows, "same"),
            Err(ResolveError::Ambiguous(_))
        ));
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
    fn ambiguity_diagnostic_uses_v10_harness_field() {
        let rows = vec![
            json!({
                "name": "one", "harness": "codex", "cwd": "/w", "log_path": "/l",
                "short_id": "deadbeef", "harness_session_id": "aaaaaaaa-0000-0000-0000-000000000001",
            }),
            json!({
                "name": "two", "harness": "opencode", "cwd": "/w", "log_path": "/l",
                "short_id": "deadbeef", "harness_session_id": "ses_worker00000001",
            }),
        ];

        let message = find_agent_entry(&rows, "deadbeef")
            .expect_err("shared transport token is ambiguous")
            .message();

        assert!(message.contains("codex"));
        assert!(message.contains("opencode"));
        assert!(!message.contains("(?)"));
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
    fn find_agent_entry_opencode_row_preserves_canonical_handle_case() {
        let ses = "ses_7f3a9b2cAbCd1234";
        let row = json!({
            "name": "oc", "provider": "opencode", "cwd": "/w", "log_path": "/l",
            "harness_session_id": ses,
        });
        let rows = vec![row];
        assert_eq!(find_agent_entry(&rows, "oc").unwrap()["name"], "oc");
        assert_eq!(find_agent_entry(&rows, ses).unwrap()["name"], "oc");
        assert_eq!(find_agent_entry(&rows, "AbCd1234").unwrap()["name"], "oc");
        assert!(matches!(
            find_agent_entry(&rows, "abcd1234"),
            Err(ResolveError::NotFound(_))
        ));
    }

    // --- registry-miss heal (x-da8c) -----------------------------------------

    #[test]
    fn session_shape_gate_admits_only_probeable_tokens() {
        for t in [
            "a1b2c3d4",
            "A1B2C3D4",
            "ses_7f3a9b2c1d0e",
            CLAUDE_UUID_FIXTURE,
            // A canonical OpenCode tail may be eight alphabetic characters,
            // so a same-shaped registry name must join the store namespace.
            "reviewer",
        ] {
            assert!(is_session_shaped(t), "{t} should be probeable");
        }
        // Short tokens of the wrong width remain outside the store seam.
        for t in ["a1b2c3", "a1b2c3d45", "", "ses_", "SES_7f3a9b2c1d0e"] {
            assert!(!is_session_shaped(t), "{t} should not be probeable");
        }
    }

    #[test]
    fn heal_wrapper_preserves_registry_hit_and_clean_miss_results() {
        // No `fno` is stubbed here, so any shellout would degrade to NotFound
        // anyway; what this pins is that a hit returns the ROW and a store miss
        // returns the original resolution error.
        let rows = vec![claude_row("billing", "a1b2c3d4", CLAUDE_UUID_FIXTURE)];
        assert_eq!(
            resolve_entry_with_heal(&rows, "billing", Path::new("/nonexistent/registry.json"))
                .unwrap()["name"],
            "billing"
        );
        let err = resolve_entry_with_heal(&rows, "ghost", Path::new("/nonexistent/registry.json"))
            .unwrap_err();
        assert_eq!(
            err.message(),
            "no agent matching 'ghost'; accepted forms: name, canonical handle, transport short id, or full session id"
        );
    }

    #[test]
    fn heal_wrapper_keeps_an_ambiguous_registry_ambiguous() {
        // Healing an ambiguous registry would pick the winner the registry
        // deliberately refused to pick.
        let rows = vec![
            claude_row("one", "abcd1234", CLAUDE_UUID_FIXTURE),
            claude_row("two", "abcd1234", "abcd1234-9999-8888-7777-666655554444"),
        ];
        assert!(matches!(
            resolve_entry_with_heal(&rows, "abcd1234", Path::new("/nonexistent/registry.json")),
            Err(ResolveError::Ambiguous(_))
        ));
    }

    #[test]
    fn heal_output_distinguishes_clean_miss_from_broken_coverage() {
        use std::process::Command;

        let miss = Command::new("sh").args(["-c", "exit 13"]).output().unwrap();
        assert!(parse_heal_token_output("deadbeef", &miss)
            .unwrap()
            .is_none());

        let off_contract = Command::new("sh")
            .args(["-c", "echo probe-broke >&2; exit 7"])
            .output()
            .unwrap();
        let message = parse_heal_token_output("deadbeef", &off_contract).unwrap_err();
        assert!(message.contains("cannot safely resolve"));
        assert!(message.contains("probe-broke"));

        let malformed = Command::new("sh")
            .args(["-c", "printf 'not-json\\n'"])
            .output()
            .unwrap();
        assert!(parse_heal_token_output("deadbeef", &malformed)
            .unwrap_err()
            .contains("malformed JSON"));
    }

    #[test]
    fn backfill_gives_a_healed_v10_row_the_fields_the_verbs_read() {
        // The shape `fno agents heal-token` emits: harness-only, no `provider`
        // and no `claude_session_uuid` (v10 removed both from disk). Without the
        // backfill, `logs` would take the codex branch and resume's dead arm
        // would refuse "no claude session recorded".
        let mut row = json!({
            "name": "fno-a1b2c3d4", "harness": "claude", "cwd": "/w", "log_path": "",
            "short_id": "a1b2c3d4", "harness_session_id": CLAUDE_UUID_FIXTURE,
            "status": "orphaned",
        });
        backfill_row_aliases(row.as_object_mut().unwrap(), false);
        assert!(row.get("provider").is_none());
        assert_eq!(row["claude_session_uuid"], CLAUDE_UUID_FIXTURE);
    }

    #[test]
    fn backfill_covers_the_non_claude_healed_row_too() {
        // The healer adopts codex rows as readily as claude ones, and their
        // resume path reads the legacy per-provider key just the same.
        let mut row = json!({
            "name": "fno-a1b2c3d4", "harness": "codex", "cwd": "/w", "log_path": "",
            "harness_session_id": CLAUDE_UUID_FIXTURE, "status": "orphaned",
        });
        backfill_row_aliases(row.as_object_mut().unwrap(), false);
        assert!(row.get("provider").is_none());
        assert_eq!(row["codex_session_id"], CLAUDE_UUID_FIXTURE);
        assert!(row.get("claude_session_uuid").is_none());
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
    fn trace_surfaces_registry_ambiguity_instead_of_not_found() {
        let td = tempfile::TempDir::new().unwrap();
        let registry = td.path().join("registry.json");
        fs::write(
            &registry,
            serde_json::to_vec(&json!({
                "schema_version": REGISTRY_SCHEMA_VERSION,
                "agents": [
                    {
                        "name": "one",
                        "harness": "codex",
                        "cwd": "/one",
                        "log_path": "/tmp/one.log",
                        "harness_session_id": "aaaaaaaa-1111-7222-8333-4444deadbeef"
                    },
                    {
                        "name": "two",
                        "harness": "opencode",
                        "cwd": "/two",
                        "log_path": "/tmp/two.log",
                        "harness_session_id": "ses_1111111111111111deadbeef"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let args = TraceArgs {
            name: Some("deadbeef".to_string()),
            request_id: None,
            all_agents: false,
            json_out: false,
            limit: 200,
            since: None,
        };

        let result = trace_logic(&args, &td.path().join("events.jsonl"), &registry);

        assert_eq!(result.exit_code, 13);
        assert!(result.stderr.contains("ambiguous across 2 agents"));
        assert!(result.stderr.contains("one"));
        assert!(result.stderr.contains("two"));
        assert!(!result.stderr.contains("not found"));
    }

    #[test]
    fn session_id_field_and_resume_argv_match_python() {
        assert_eq!(session_id_field("claude"), Some("short_id"));
        assert_eq!(session_id_field("codex"), Some("harness_session_id"));
        assert_eq!(session_id_field("gemini"), Some("harness_session_id"));
        assert_eq!(session_id_field("opencode"), Some("harness_session_id"));
        assert_eq!(session_id_field("unknown"), None);

        assert_eq!(
            build_resume_argv("codex", "uuid-1", Some("/path/that/does/not/exist")),
            Some(vec![
                "codex".into(),
                "-c".into(),
                "sandbox_workspace_write.writable_roots=[\"/path/that/does/not/exist/.fno/plans\"]"
                    .into(),
                "resume".into(),
                "uuid-1".into(),
            ])
        );
        assert_eq!(
            build_resume_argv("claude", "abc123", None),
            Some(vec!["claude".into(), "attach".into(), "abc123".into()])
        );
        assert_eq!(
            build_resume_argv("gemini", "g-1", None),
            Some(vec!["gemini".into(), "--resume".into(), "g-1".into()])
        );
        assert_eq!(
            build_resume_argv("opencode", "ses_1", None),
            Some(vec!["opencode".into(), "--session".into(), "ses_1".into()])
        );
        assert_eq!(build_resume_argv("agy", "x", None), None);
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
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("done".into())).unwrap(),
            (
                vec!["claude".to_string(), "--resume".into(), uuid.into()],
                Some(uuid.to_string()), // dead-arm carries the uuid to claim
            )
        );

        // uuid absent -> refuse (Err 13), never `claude --resume ""`.
        let entry_no_uuid = serde_json::json!({
            "name": "w", "provider": "claude", "short_id": "7c5dcf5d",
        });
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry_no_uuid, "w", |_| Some("done".into())),
            Err(13)
        );

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
            claude_resume_argv_with_truth(&ch2, &entry, "w", |_| None).unwrap(),
            (
                vec!["claude".to_string(), "attach".into(), "7c5dcf5d".into()],
                None, // live attach arm claims nothing
            )
        );
    }

    #[test]
    fn live_claude_attach_delegates_dead_and_non_claude_and_mux_do_not() {
        // The live-attach arm ((["claude","attach",short_id], None)) is the one
        // this binary used to exec bare, with no pty/route/verification. It is
        // the only combination that should delegate to `fno-py agents resume`.
        assert!(should_delegate_claude_live_attach("claude", &None, &None,));
        // Dead-relaunch arm carries Some(uuid) -> Rust already restores the
        // route and relaunches itself; must not also delegate.
        assert!(!should_delegate_claude_live_attach(
            "claude",
            &Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string()),
            &None,
        ));
        // A mux-hosted row relaunches through the pane path; must not delegate
        // even if some future change ever paired it with claim_uuid == None.
        assert!(!should_delegate_claude_live_attach(
            "claude",
            &None,
            &Some("session-1".to_string()),
        ));
        // Every non-claude harness keeps its own provider resume CLI.
        assert!(!should_delegate_claude_live_attach("codex", &None, &None));
    }

    #[test]
    fn resume_args_accept_message_flag_long_and_short() {
        // code-review finding: --message/-m must not die with "unknown resume
        // flag" -- resume auto-routes to this binary by default, so this
        // parser is the only door the claude wake's --message option has.
        let (name, print_command, message) = parse_resume_args(&[
            "alpha".to_string(),
            "--message".to_string(),
            "continue please".to_string(),
        ])
        .unwrap();
        assert_eq!(name, "alpha");
        assert!(!print_command);
        assert_eq!(message.as_deref(), Some("continue please"));

        let (name, _, message) =
            parse_resume_args(&["-m".to_string(), "hi".to_string(), "beta".to_string()]).unwrap();
        assert_eq!(name, "beta");
        assert_eq!(message.as_deref(), Some("hi"));

        // No --message given: still parses, message is None (unchanged
        // pre-fix behavior for every other flag combination).
        let (name, print_command, message) =
            parse_resume_args(&["gamma".to_string(), "--print-command".to_string()]).unwrap();
        assert_eq!(name, "gamma");
        assert!(print_command);
        assert_eq!(message, None);
    }

    #[test]
    fn resume_args_message_flag_needs_a_value() {
        assert_eq!(
            parse_resume_args(&["alpha".to_string(), "--message".to_string()]),
            Err(2)
        );
    }

    #[test]
    fn resume_args_still_rejects_unknown_flags() {
        assert_eq!(
            parse_resume_args(&["alpha".to_string(), "--bogus".to_string()]),
            Err(2)
        );
    }

    #[test]
    fn claude_resume_dead_arm_restores_a_recorded_route_or_refuses() {
        // x-ae2d: the dead arm RELAUNCHES, so it is the one door on this verb
        // that can lose a route. Untested, the branch is a guard on paper: the
        // Python spawn door has its own tests and neither covers this one.
        let uuid = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let home = cv_tmpdir();
        let ch = ClaudeHome::at(home.path());
        let route = home.path().join("route-settings-abc123.json");
        fs::write(&route, r#"{"env":{"ANTHROPIC_BASE_URL":"https://z"}}"#).unwrap();
        let entry = serde_json::json!({
            "name": "w", "provider": "claude",
            "short_id": "7c5dcf5d", "claude_session_uuid": uuid,
            "route_settings_path": route.to_str().unwrap(),
        });

        // Recorded + present -> `--settings <path>` ahead of `--resume`, the
        // same mechanism (and the same flag order) the original spawn used.
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("done".into())).unwrap(),
            (
                vec![
                    "claude".to_string(),
                    "--settings".into(),
                    route.to_str().unwrap().into(),
                    "--resume".into(),
                    uuid.into(),
                ],
                Some(uuid.to_string()),
            )
        );

        // Recorded but FLOOR-ONLY -> refuse too. claude reads an empty settings
        // value as unset, so this file selects nothing and the worker would come
        // back on the default account in silence - the same outcome as a missing
        // file, and the same rule Python's read_route_settings applies.
        fs::write(&route, r#"{"env":{"ANTHROPIC_API_KEY":""}}"#).unwrap();
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("done".into())),
            Err(13)
        );

        // Recorded but unparseable -> refuse, never a partial re-apply.
        fs::write(&route, "{not json").unwrap();
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("done".into())),
            Err(13)
        );

        // Recorded but gone -> refuse. Relaunching would work, bill the default
        // Anthropic account, and report nothing.
        fs::remove_file(&route).unwrap();
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("done".into())),
            Err(13)
        );

        // The live arm never relaunches, so a recorded route changes nothing
        // there - it must stay a bare `claude attach`.
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("working".into())).unwrap(),
            (
                vec!["claude".into(), "attach".into(), "7c5dcf5d".into()],
                None
            )
        );
    }

    #[test]
    fn claude_resume_argv_mux_row_relaunches_on_a_gone_verdict() {
        // x-b84f: a pane worker carries a canonical uuid but NO short_id (empty
        // by design: _validate_single_live_ref enforces mux XOR worker XOR bg, so
        // a mux row never gets the transport key). The loader backfill mirrors
        // harness_session_id -> claude_session_uuid, so the uuid IS resolvable.
        // What stood between it and the relaunch arm is the empty short_id, which
        // short-circuited the truth probe to None and printed "liveness is
        // inconclusive" for a session the operator can see is gone. A pane-gone
        // worker is affirmatively dead, so resume relaunches it (--resume <uuid>
        // plus the recorded route) instead of refusing. AC1, AC3.
        let uuid = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let home = cv_tmpdir();
        let ch = ClaudeHome::at(home.path());
        let route = home.path().join("route-settings-mux.json");
        fs::write(
            &route,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid"}}"#,
        )
        .unwrap();
        let entry = serde_json::json!({
            "name": "pane-worker", "harness": "claude",
            "claude_session_uuid": uuid,
            "route_settings_path": route.to_str().unwrap(),
            // short_id deliberately absent: this is the mux-row shape.
        });

        // truth_fn returns the lowered "stalled" that
        // family1_truth_state_for_resume produces for a working + unreachable +
        // pane-gone verdict (proven by the lowering unit test in claude_ask).
        let (argv, claim) =
            claude_resume_argv_with_truth(&ch, &entry, "pane-worker", |_| Some("stalled".into()))
                .expect("a gone pane worker relaunches rather than refusing");
        assert_eq!(claim.as_deref(), Some(uuid));
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--settings".into(),
                route.to_str().unwrap().into(),
                "--resume".into(),
                uuid.into(),
            ]
        );

        // AC2: a row with no session id in any field must refuse, and the return
        // is Err(13) regardless of message - but it must not be reachable via the
        // dead arm (no uuid to relaunch).
        let entry_idless = serde_json::json!({
            "name": "idless", "harness": "claude",
        });
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry_idless, "idless", |_| Some("done".into())),
            Err(13)
        );
    }

    #[test]
    fn mux_pane_run_argv_fences_the_resumed_command() {
        // x-b84f D3: the one-verb form of the manual `fno mux pane run` recovery.
        // The `--` fence keeps the inner `--resume <uuid>` (and any flag-shaped
        // arg) out of the mux parser, so the resumed command is transported
        // verbatim. AC5: only a path appears, never a value from inside the file.
        let claude = vec![
            "claude".to_string(),
            "--settings".into(),
            "/route/path.json".into(),
            "--resume".into(),
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
        ];
        let pane = mux_pane_run_argv("main", "/wt", &claude);
        assert_eq!(
            pane,
            vec![
                "mux".to_string(),
                "pane".into(),
                "run".into(),
                "--session".into(),
                "main".into(),
                "--cwd".into(),
                "/wt".into(),
                "--".into(),
                "claude".into(),
                "--settings".into(),
                "/route/path.json".into(),
                "--resume".into(),
                "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
            ]
        );
        // The fence sits exactly between the mux transport and the command.
        assert_eq!(pane.iter().position(|a| a == "--"), Some(7));
    }

    #[test]
    fn claude_resume_argv_live_pane_row_is_not_called_inconclusive() {
        // x-b84f review #4: a live pane worker has no short_id, so the live
        // attach arm (which gates on a present short_id) does not fire. Pre-fix
        // it fell through to the else arm and printed "liveness is
        // inconclusive" for a session the probe JUST answered live, then told
        // the operator to re-run a probe that contradicts the message. The live
        // arm now names the real state and points at the mux.
        let uuid = "3c4d5e6f-7081-9203-a4b5-c6d7e8f9a0b1";
        let home = cv_tmpdir();
        let ch = ClaudeHome::at(home.path());
        let entry = serde_json::json!({
            "name": "live-pane", "harness": "claude",
            "claude_session_uuid": uuid,
            // short_id deliberately absent: a live mux row.
        });
        // Pairs with the gone-verdict test: gone -> relaunch (Ok), live -> refuse
        // (Err 13). The message is the actual fix (it no longer says
        // "inconclusive" for a session the probe answered live); the return code
        // pins that a live pane row neither attaches nor relaunches.
        let code =
            claude_resume_argv_with_truth(&ch, &entry, "live-pane", |_| Some("working".into()))
                .expect_err("a live pane worker refuses cleanly instead of attaching");
        assert_eq!(code, 13);
    }

    fn _git(repo: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {:?} in {} failed", args, repo.display());
    }

    #[test]
    fn resolve_resume_cwd_picks_the_transcripts_worktree_over_the_stale_recorded_cwd() {
        // Registered at the canonical checkout; transcript under a worktree's
        // project dir (the EnterWorktree case). Resume must resolve to the
        // worktree, not the pre-EnterWorktree recorded cwd.
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize: macOS houses tempfile under /var/folders (a symlink to
        // /private/var/folders), and `git` records the resolved /private/var
        // path while the test's PathBuf carries /var - the slugs would diverge.
        let home = tmp.path().canonicalize().unwrap();
        let canonical = home.join("repo");
        std::fs::create_dir_all(&canonical).unwrap();
        _git(&canonical, &["init", "-q"]);
        _git(&canonical, &["config", "user.email", "t@t"]);
        _git(&canonical, &["config", "user.name", "t"]);
        _git(&canonical, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let wt = home.join("wt");
        _git(
            &canonical,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                wt.to_str().unwrap(),
            ],
        );

        let uuid = "9d2874cb-9365-48c0-aeb6-9e1d244f4cd3";
        let wt_project = home
            .join(".claude")
            .join("projects")
            .join(claude_cwd_slug(&wt));
        std::fs::create_dir_all(&wt_project).unwrap();
        std::fs::write(wt_project.join(format!("{uuid}.jsonl")), "[]").unwrap();

        let resolved = resolve_resume_cwd(&ClaudeHome::at(home), canonical.to_str().unwrap(), uuid);
        assert_eq!(
            resolved, wt,
            "resolved to the transcript's worktree, not the recorded cwd"
        );
    }

    #[test]
    fn resolve_resume_cwd_falls_back_to_recorded_when_no_transcript_exists() {
        // No transcript under any candidate: fall back to the recorded cwd and
        // say so on stderr. An absent number beats a guessed one.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let recorded = home.join("recorded");
        std::fs::create_dir_all(&recorded).unwrap();

        let resolved = resolve_resume_cwd(
            &ClaudeHome::at(home),
            recorded.to_str().unwrap(),
            "deadbeef-0000-0000-0000-000000000000",
        );
        assert_eq!(resolved, recorded);
    }

    #[test]
    fn resolve_resume_cwd_confirms_recorded_when_its_slug_holds_the_transcript() {
        // The transcript under the recorded cwd's own slug confirms it; no
        // worktree enumeration needed.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let recorded = home.join("recorded");
        std::fs::create_dir_all(&recorded).unwrap();
        let uuid = "aaaaaaaa-0000-0000-0000-000000000000";
        let project = home
            .join(".claude")
            .join("projects")
            .join(claude_cwd_slug(&recorded));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{uuid}.jsonl")), "[]").unwrap();

        let resolved = resolve_resume_cwd(&ClaudeHome::at(home), recorded.to_str().unwrap(), uuid);
        assert_eq!(resolved, recorded);
    }

    #[test]
    fn claude_resume_socket_miss_requires_family1_death() {
        let home = cv_tmpdir();
        let ch = ClaudeHome::at(home.path());
        let entry = serde_json::json!({
            "name": "w", "provider": "claude", "short_id": "7c5dcf5d",
            "claude_session_uuid": "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9",
        });
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| None),
            Err(13)
        );
        assert_eq!(
            claude_resume_argv_with_truth(&ch, &entry, "w", |_| Some("working".into())).unwrap(),
            (
                vec!["claude".into(), "attach".into(), "7c5dcf5d".into()],
                None
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
        let err = acquire_resume_session_claim(uuid, Some(root.path()), None).unwrap_err();
        assert_eq!(err.0, 11);
        assert!(err.1.contains("held live by another writer"));

        // A session with no holder: the resumer wins.
        let uuid2 = "1111abcd-2222-3333-4444-555566667777";
        assert!(acquire_resume_session_claim(uuid2, Some(root.path()), None).is_ok());
    }

    #[test]
    fn acquire_resume_session_claim_records_an_expiry_for_the_mux_path() {
        // The mux relaunch exits after pane dispatch, so its session claim cannot
        // ride the holder pid. A PID-only claim (ttl=None) goes Stale the moment
        // that pid dies, and a second resumer steals it before the resumed claude
        // is probe-live. The TTL keeps it Live across that window, which only
        // holds if the record actually carries an expires_at.
        use crate::claims::{claim_path, read_claim_file};

        let uuid = "2b3c4d5e-6f70-8192-03a4-b5c6d7e8f9a0";
        let root = cv_tmpdir();
        acquire_resume_session_claim(uuid, Some(root.path()), Some(MUX_RESUME_CLAIM_TTL_MS))
            .expect("acquire with a ttl succeeds");
        let path =
            claim_path(&format!("session:{uuid}"), Some(root.path())).expect("claim path resolves");
        let rec = read_claim_file(&path).expect("claim file is readable");
        assert!(
            rec.expires_at.is_some(),
            "a TTL claim records an expiry; a PID-only claim would not"
        );
    }

    #[test]
    fn acquire_named_session_claim_guards_resume_attach_keys() {
        // The live-attach delegation itself acquires no claim (Python's
        // `_resume_claude_wake` does, gated on skip-eligibility, once exec'd)
        // -- but the "resume-attach:{short_id}" key format this exercises is
        // still the shared contract: Python's own claim uses the identical
        // key so the two runtimes contend for the same lock on the same row
        // whichever one ends up acquiring it. Verify that key independently
        // refuses a second concurrent writer, the same contract
        // acquire_resume_session_claim already has for its own key.
        use crate::claims::{acquire, AcquireOpts, AcquireOutcome};
        let short_id = "deadbeef";
        let root = cv_tmpdir();

        // A different live writer already holds the claim (matching this
        // process's own holder string would just re-acquire, not conflict).
        let pre = acquire(
            &format!("resume-attach:{short_id}"),
            "other-writer",
            AcquireOpts {
                root: Some(root.path().to_path_buf()),
                ..Default::default()
            },
        );
        assert!(matches!(pre, AcquireOutcome::Acquired(_)));

        let err = acquire_named_session_claim(
            &format!("resume-attach:{short_id}"),
            short_id,
            Some(root.path()),
            None,
        )
        .unwrap_err();
        assert_eq!(err.0, 11);
        assert!(err.1.contains("held live by another writer"));

        // A different short_id: an unrelated row's wake is never blocked by
        // this one's claim.
        let other = acquire_named_session_claim(
            "resume-attach:other-id",
            "other-id",
            Some(root.path()),
            None,
        );
        assert!(other.is_ok());
    }

    #[test]
    fn parse_manifest_identity_reads_canonical_fields() {
        let content = "---\n\
            fno_id: 20260804T202518Z-cl99002-4e0236\n\
            input: \"x-0358\"\n\
            harness: claude\n\
            harness_session_id: c7dc6218-493a-4299-916a-330ec0b0b055\n\
            owner_cwd: \"/Users/x/code/wt\"\n\
            claude_session_id: c7dc6218-493a-4299-916a-330ec0b0b055\n\
            codex_thread_id: null\n\
            ---\n\
            graph_node_id: x-0358\n";
        let m = parse_manifest_identity(content);
        assert_eq!(m.harness, "claude");
        assert_eq!(m.harness_session_id, "c7dc6218-493a-4299-916a-330ec0b0b055");
        assert_eq!(m.owner_cwd, "/Users/x/code/wt");
        assert_eq!(m.fno_id, "20260804T202518Z-cl99002-4e0236");
        // codex_thread_id: null stays empty, never "null".
        assert_eq!(m.codex_thread_id, "");
        // Matches on the canonical id and the legacy claude alias.
        assert!(m.matches("c7dc6218-493a-4299-916a-330ec0b0b055"));
        assert!(!m.matches("nope"));
        assert!(!m.matches(""));
    }

    #[test]
    fn manifest_identity_matches_codex_legacy_alias() {
        let m = ManifestIdentity {
            codex_thread_id: "thread-abc".into(),
            ..Default::default()
        };
        assert!(m.matches("thread-abc"));
    }

    #[test]
    fn parse_manifest_identity_skips_forged_keys_in_input_scalar() {
        // A `/target` argument whose text spills across lines and contains
        // `key: value` continuations must NOT forge identity fields: the real
        // harness / harness_session_id (written after input) must win.
        let content = "---\n\
            fno_id: real-run\n\
            input: \"some feature\n\
            harness: forged\n\
            harness_session_id: forged-id\n\
            \"\n\
            harness: claude\n\
            harness_session_id: real-id\n\
            ---\n";
        let m = parse_manifest_identity(content);
        assert_eq!(m.harness, "claude");
        assert_eq!(m.harness_session_id, "real-id");
        assert_eq!(m.fno_id, "real-run");
        // The forged session id is never matchable.
        assert!(!m.matches("forged-id"));
    }

    #[test]
    fn parse_manifest_identity_survives_ambiguous_scalar_terminator() {
        // `/target 'ship it \'` -> init writes an input whose closing quote is
        // preceded by a lone backslash, so the scalar's real terminator is the
        // next `plan_path: "..."` line. Consuming that line would leave the
        // scalar open to EOF and drop every identity key below it (init writes
        // `input` before harness / harness_session_id / owner_cwd), turning a
        // matching manifest into "no evidence".
        let content = "---\n\
            fno_id: real-run\n\
            input: \"ship it \\\"\n\
            plan_path: \"internal/fno/plan.md\"\n\
            harness: claude\n\
            harness_session_id: c7dc6218-493a-4299-916a-330ec0b0b055\n\
            owner_cwd: \"/Users/x/wt\"\n\
            ---\n";
        let m = parse_manifest_identity(content);
        assert_eq!(m.harness, "claude");
        assert_eq!(m.harness_session_id, "c7dc6218-493a-4299-916a-330ec0b0b055");
        assert_eq!(m.owner_cwd, "/Users/x/wt");
        assert!(m.matches("c7dc6218-493a-4299-916a-330ec0b0b055"));
    }

    #[test]
    fn mint_uses_legacy_alias_when_canonical_session_is_null() {
        // init writes `harness_session_id: ${_HARNESS_SESSION:-null}`, so a real
        // manifest can carry the session under the legacy alias alone. Keying on
        // the canonical field alone minted an empty session id / short_id and the
        // name `target-`.
        let id = ManifestIdentity {
            harness: "claude".into(),
            claude_session_id: "c7dc6218-493a-4299-916a-330ec0b0b055".into(),
            owner_cwd: "/Users/x/wt".into(),
            ..Default::default()
        };
        let e = mint_synthesized_entry(&id, "now");
        assert_eq!(
            e.harness_session_id.as_deref(),
            Some("c7dc6218-493a-4299-916a-330ec0b0b055")
        );
        assert_eq!(e.short_id, "c0b0b055");
        assert_eq!(e.name, "target-c0b0b055");

        // Codex-alias-only manifest with no `harness` must not default to claude.
        let codex = ManifestIdentity {
            codex_thread_id: "thread-abcdef12".into(),
            ..Default::default()
        };
        let e = mint_synthesized_entry(&codex, "now");
        assert_eq!(e.harness.as_deref(), Some("codex"));
        assert_eq!(e.harness_session_id.as_deref(), Some("thread-abcdef12"));
        assert_eq!(e.claude_session_uuid, None);
    }

    #[test]
    fn parse_manifest_identity_single_line_input_does_not_open_scalar() {
        // `input: "x-0358"` closes on the same line; the next real key parses.
        let content = "input: \"x-0358\"\nharness: codex\n";
        let m = parse_manifest_identity(content);
        assert_eq!(m.harness, "codex");
    }

    #[test]
    fn derived_short_id_uses_final_eight() {
        assert_eq!(
            derived_short_id("c7dc6218-493a-4299-916a-330ec0b0b055"),
            "c0b0b055"
        );
        assert_eq!(derived_short_id("abc12345"), "abc12345");
        assert_eq!(derived_short_id("short"), "short");
    }

    #[test]
    fn mint_synthesized_entry_sets_identity_short_id_and_fno_id() {
        let id = ManifestIdentity {
            harness: "codex".into(),
            harness_session_id: "thread-1234567890".into(),
            owner_cwd: "/Users/x/wt".into(),
            fno_id: "20260804T202518Z-cl99002-4e0236".into(),
            ..Default::default()
        };
        let e = mint_synthesized_entry(&id, "2026-08-04T20:25:18Z");
        assert_eq!(e.harness.as_deref(), Some("codex"));
        assert_eq!(e.harness_session_id.as_deref(), Some("thread-1234567890"));
        assert_eq!(e.cwd, "/Users/x/wt");
        assert_eq!(e.project_root, "/Users/x/wt");
        // codex carries no claude uuid; claude_session_uuid stays None.
        assert_eq!(e.claude_session_uuid, None);
        assert_eq!(e.fno_id.as_deref(), Some("20260804T202518Z-cl99002-4e0236"));
        assert!(!e.short_id.is_empty());
        assert_eq!(e.name, format!("target-{}", e.short_id));
        assert_eq!(e.status, crate::AgentStatus::Idle);
        assert!(e.pid.is_none());
    }

    #[test]
    fn mint_synthesized_entry_claude_records_resume_uuid() {
        let id = ManifestIdentity {
            harness: "claude".into(),
            harness_session_id: "c7dc6218-493a-4299-916a-330ec0b0b055".into(),
            ..Default::default()
        };
        let e = mint_synthesized_entry(&id, "now");
        assert_eq!(
            e.claude_session_uuid.as_deref(),
            Some("c7dc6218-493a-4299-916a-330ec0b0b055")
        );
    }

    #[test]
    fn upsert_synthesized_row_is_idempotent_by_session_id() {
        let dir = cv_tmpdir();
        let reg = dir.path().join("registry.json");
        let id = ManifestIdentity {
            harness: "codex".into(),
            harness_session_id: "thread-1".into(),
            owner_cwd: "/x".into(),
            fno_id: "run-1".into(),
            ..Default::default()
        };
        let mut e = mint_synthesized_entry(&id, "t1");
        upsert_synthesized_row(&reg, e.clone()).unwrap();
        // re-adopt with an updated cwd upserts (keyed on harness_session_id),
        // never duplicates.
        e.cwd = "/y".into();
        upsert_synthesized_row(&reg, e).unwrap();
        let loaded = crate::state::load_registry(&reg).unwrap();
        let rows: Vec<_> = loaded
            .entries
            .iter()
            .filter(|r| r.harness_session_id.as_deref() == Some("thread-1"))
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cwd, "/y");
        assert_eq!(rows[0].fno_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn upsert_synthesized_row_preserves_live_runtime_state() {
        // Re-adopting a session that already has a LIVE row (reachable when the
        // operator adopts by a legacy alias, which resolve_entry_with_heal
        // misses) must not downgrade it to Idle or drop its pid.
        let dir = cv_tmpdir();
        let reg = dir.path().join("registry.json");
        let id = ManifestIdentity {
            harness: "codex".into(),
            harness_session_id: "thread-live".into(),
            owner_cwd: "/x".into(),
            ..Default::default()
        };
        let mut live = mint_synthesized_entry(&id, "t1");
        live.status = crate::AgentStatus::Busy;
        live.pid = Some(4242);
        live.log_path = Some("/tmp/live.log".into());
        upsert_synthesized_row(&reg, live).unwrap();

        upsert_synthesized_row(&reg, mint_synthesized_entry(&id, "t2")).unwrap();

        let loaded = crate::state::load_registry(&reg).unwrap();
        let row = loaded
            .entries
            .iter()
            .find(|r| r.harness_session_id.as_deref() == Some("thread-live"))
            .expect("row survives");
        assert_eq!(row.status, crate::AgentStatus::Busy);
        assert_eq!(row.pid, Some(4242));
        assert_eq!(row.log_path.as_deref(), Some("/tmp/live.log"));
        assert_eq!(row.created_at, "t1");
    }

    #[test]
    fn gc_keeps_synthesized_idle_row() {
        // An adopted orphan row (Idle, no pid, no exited_at) must survive the GC
        // sweep: non-terminal + no confirmed-dead pid -> gc_action Keep, so the
        // row stays addressable until the operator resumes it.
        let row = crate::gc::GcRow {
            status: crate::AgentStatus::Idle,
            is_live: false,
            pid_confirmed_dead: false,
            owns_worktree: true,
            exited_at: None,
            liveness_surface: true,
            transcript_fresh: Some(false),
            harness_session_gone: None,
            dormant_done: false,
            worktree_clean: None,
        };
        assert_eq!(
            crate::gc::gc_action(&row, 1000, 60),
            crate::gc::GcAction::Keep
        );
    }

    #[test]
    fn synthesize_and_adopt_registry_hit_is_idempotent() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = cv_tmpdir();
        std::env::set_var(crate::paths::HOME_ENV, dir.path());
        let home = AgentsHome::from_env();
        let id = ManifestIdentity {
            harness: "codex".into(),
            harness_session_id: "thread-seed-1234".into(),
            owner_cwd: "/x".into(),
            ..Default::default()
        };
        upsert_synthesized_row(&home.registry_json(), mint_synthesized_entry(&id, "t")).unwrap();
        let (row, fno_id, source) =
            synthesize_and_adopt("thread-seed-1234", &home).expect("seeded row resolves");
        assert_eq!(source, AdoptSource::Registry);
        assert_eq!(
            row.get("harness_session_id").and_then(Value::as_str),
            Some("thread-seed-1234")
        );
        assert_eq!(fno_id, None, "seeded row carried no fno_id");
        std::env::remove_var(crate::paths::HOME_ENV);
    }

    #[test]
    fn persist_manifest_identity_surfaces_registry_write_failure() {
        let dir = cv_tmpdir();
        let home = AgentsHome::at(dir.path());
        let mut existing = mint_synthesized_entry(
            &ManifestIdentity {
                harness: "codex".into(),
                harness_session_id: "existing-session".into(),
                ..Default::default()
            },
            "t0",
        );
        existing.name = "dffdeeca".into();
        existing.short_id = "transport".into();
        crate::state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(existing);
        })
        .unwrap();

        let result = persist_manifest_identity(
            &ManifestIdentity {
                harness: "codex".into(),
                harness_session_id: "01a0152f-45fd-78f0-b109-78f8dffdeeca".into(),
                ..Default::default()
            },
            &home,
        );

        let Err(AdoptError::Io(message)) = result else {
            panic!("registry collision must remain an adoption I/O error");
        };
        assert!(message.contains("collides with row"));
        assert!(message.contains("dffdeeca"));
    }

    #[test]
    fn synthesize_and_adopt_miss_writes_no_row() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = cv_tmpdir();
        std::env::set_var(crate::paths::HOME_ENV, dir.path());
        let home = AgentsHome::from_env();
        // A full session id absent from the registry, from every worktree manifest
        // (cwd is a bare tempdir), and from the harness stores. No row is written.
        let res = synthesize_and_adopt("deadbeef-1111-2222-3333-444455556666", &home);
        assert!(
            matches!(res, Err(AdoptError::NoEvidence) | Err(AdoptError::Io(_))),
            "miss must refuse, not mint; got {res:?}"
        );
        assert!(read_registry_entries(&home.registry_json())
            .unwrap()
            .is_empty());
        std::env::remove_var(crate::paths::HOME_ENV);
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
        let msg = claude_attach_pointer_with_truth(&ch_dead, &entry, "w", |_| Some("done".into()))
            .expect("dead row -> pointer");
        assert!(msg.contains("fno agents resume w"));
        assert!(msg.contains(&format!("--resume {uuid} --substrate bg")));

        // No uuid -> no pointer (never print an unusable command).
        let no_uuid = serde_json::json!({
            "name": "w", "provider": "claude", "short_id": "7c5dcf5d",
        });
        assert_eq!(
            claude_attach_pointer_with_truth(&ch_dead, &no_uuid, "w", |_| Some("done".into())),
            None
        );

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
            claude_attach_pointer_with_truth(
                &ClaudeHome::at(live_home.path()),
                &entry,
                "w",
                |_| None
            ),
            None
        );
        assert_eq!(
            claude_attach_pointer_with_truth(&ch_dead, &entry, "w", |_| None),
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
            "fno-cv-logs-{}-{}",
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
    fn mux_pane_run_failure_message_names_unanswered_not_absent() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        // exit 20 (EXIT_CONTROL_UNANSWERED): the verb reached the server, so
        // the message must say a pane MAY have started - never the blanket
        // "(no pane started)" the other codes get.
        let msg = mux_pane_run_failure_message(
            "worker-A",
            "main",
            ExitStatus::from_raw(MUX_CONTROL_UNANSWERED << 8),
        );
        assert!(msg.contains("MAY have started"), "{msg}");
        assert!(msg.contains("pane ls --session main"), "{msg}");
        assert!(!msg.contains("no pane started"), "{msg}");

        // Every other non-zero code keeps the original, stronger claim.
        let msg = mux_pane_run_failure_message("worker-A", "main", ExitStatus::from_raw(1 << 8));
        assert!(msg.contains("no pane started"), "{msg}");
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
            "fno-cv-reg-{}-{}",
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

        // Legacy Python format: top-level "agents" with provider as the
        // harness identity. Current rows carry harness separately.
        let valid = r#"{"name":"cx","provider":"codex","cwd":"/tmp/x","log_path":"/tmp/x/l","status":"live"}"#;
        let valid_current = r#"{"name":"cx","harness":"codex","cwd":"/tmp/x","log_path":"/tmp/x/l","status":"live"}"#;
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

        // A NEWER schema_version now reads forward rather than erroring. The old
        // refusal meant one source-ahead writer bricked every deployed reader on
        // the machine at once; a reader that is merely behind must degrade, not
        // take the fleet down. Matches Python load_registry.
        fs::write(
            &reg,
            format!(r#"{{"schema_version":99,"agents":[{valid_current}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);
        fs::write(
            &reg,
            format!(r#"{{"schema_version":15,"agents":[{valid_current}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);
        fs::write(
            &reg,
            format!(r#"{{"schema_version":14,"agents":[{valid}]}}"#),
        )
        .unwrap();
        assert_eq!(load_registry_entries(&reg).unwrap().len(), 1);

        // A missing or non-integer version is damage, not a newer writer.
        fs::write(&reg, r#"{"agents":[]}"#).unwrap();
        assert!(load_registry_entries(&reg).is_err());
        fs::write(&reg, r#"{"schema_version":"fourteen","agents":[]}"#).unwrap();
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
            "fno-cv-reg8dfc-{}-{}",
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
        // v10 two-way sync (codex P2): a harness-only claude row mirrors its
        // canonical id BACK into claude_session_uuid so the raw resume/attach
        // helpers (which read that legacy key) resolve it instead of refusing.
        assert_eq!(rows[0]["claude_session_uuid"], "aaaabbbbccccdddd");

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
            "fno-cv-event-{}-{}",
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
