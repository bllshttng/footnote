//! Native session truth: one reader for a session's supervision state, read
//! from its transcript TAIL.
//!
//! The state words and the payload are the port of `resolve_session_truth`
//! (`cli/src/fno/agents/session_truth.py`), field for field: `handle, state,
//! reason, last_activity_age_s, last_event_at, last_activity_basis,
//! last_message, provider_refusal, session_id, observed_model,
//! harness_title, suggestions`. The daemon's truth probe answers from this
//! module in process; no `fno agents truth` child ever starts on that path.
//!
//! Liveness is transcript-keyed ONLY, exactly as the Python reader states:
//! argv, pid, the daemon record, and state.json have each been caught lying
//! about a live session. Every failure degrades to `unknown` with one of the
//! Python reasons (`not-found` | `no-records`); the reader never panics and
//! never raises. `resolver-error` stays a decoder-accepted word for wire
//! compat, but the native resolver is infallible by construction, so it does
//! not emit it.

pub mod cursor;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::json;
use serde_json::Value;

use crate::claude_transcript_paths::{choose_from, store_listing};
use crate::state::RegistryEntry;
use cursor::TruthCursors;

pub(crate) use cursor::global_cursors;

/// Exact tag openers only: `<promise>` / `<promise ...>`, never `<promised>`.
/// Mirrors the loop runtime's protocol so truth and the runtime agree.
pub(crate) fn promise_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<promise[>\s]").unwrap())
}

pub(crate) fn watching_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<watching[>\s]").unwrap())
}

pub(crate) fn help_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<help[>\s]").unwrap())
}

pub(crate) fn api_error_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^API Error\b").unwrap())
}

pub(crate) fn option_prompt_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[\[(](?:[Yy]/[Nn]|\d+(?:/\d+)+)[\])]\s*$").unwrap())
}

/// "Silent for hours": below this the worker is between turns; above it, the
/// transcript has been quiet past the reap-safety bound and the row reads
/// stalled. Surfaced through the rendered age so a mis-tuned window misleads
/// less.
pub(crate) const STALLED_AFTER_S: f64 = 2.0 * 3600.0;

/// Tail depth: enough to find the last assistant turn past trailing
/// tool/user rows, bounded so a multi-MB transcript stays cheap.
pub(crate) const TAIL_N: usize = 40;

/// One rendered transcript turn: a role and its human-readable text.
#[derive(Clone, Debug)]
pub(crate) struct TruthRecord {
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
}

/// Pure classifier over the LAST transcript turn, moved verbatim from
/// `worked.rs` (which now calls this one) so one classifier exists in the
/// repo. Content signals apply only when the last turn is the assistant's; a
/// trailing user turn clears any stale assistant signal and mtime decides. A
/// turn stops being news past `stalled_after_s`; `done` is an outcome and
/// does not go stale.
pub(crate) fn classify_tail(
    last_role: Option<&str>,
    last_text: &str,
    age_s: Option<f64>,
    stalled_after_s: f64,
) -> String {
    let text = last_text;
    let stale = age_s.is_some_and(|a| a > stalled_after_s);
    if last_role == Some("assistant") {
        if watching_re().is_match(text) {
            return if stale {
                "stalled".into()
            } else {
                "watching".into()
            };
        }
        if promise_re().is_match(text) {
            return "done".into();
        }
        if api_error_re().is_match(text.trim_start()) {
            return "stalled".into();
        }
        let stripped = text.trim_end();
        if stripped.ends_with('?')
            || help_re().is_match(text)
            || option_prompt_re().is_match(stripped)
        {
            return if stale {
                "stalled".into()
            } else {
                "your-move".into()
            };
        }
    }
    if stale {
        return "stalled".into();
    }
    "working".into()
}

// ---------------------------------------------------------------------------
// Record extraction: the port of peek.py's claude/codex parse
// ---------------------------------------------------------------------------

/// Flatten a message `content` to legible text: a bare string, claude blocks
/// (`text` / `tool_use`), codex blocks (`input_text` / `output_text`).
/// `thinking` and `tool_result` bodies drop as observe-noise; `tool_use`
/// renders a compact marker so the peer flips and refusal reads stay whole.
fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.trim().to_string());
                } else if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    parts.push(format!("[tool_use: {name}]"));
                }
            }
            parts
                .iter()
                .filter(|p| !p.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => String::new(),
    }
}

/// A claude transcript `user`/`assistant` line, else `None`.
fn parse_claude_record(v: &Value) -> Option<TruthRecord> {
    let kind = v.get("type").and_then(Value::as_str)?;
    if kind != "user" && kind != "assistant" {
        return None;
    }
    let msg = v.get("message")?.as_object()?;
    let text = extract_text(msg.get("content"));
    if text.is_empty() {
        return None;
    }
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or(kind)
        .to_string();
    let timestamp = v.get("timestamp").and_then(Value::as_str).map(String::from);
    Some(TruthRecord {
        role,
        text,
        timestamp,
    })
}

/// A codex rollout `response_item` message line, else `None`.
fn parse_codex_record(v: &Value) -> Option<TruthRecord> {
    let kind = v.get("type").and_then(Value::as_str)?;
    if kind != "response_item" {
        return None;
    }
    let payload = v.get("payload")?.as_object()?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let text = extract_text(payload.get("content"));
    if text.is_empty() {
        return None;
    }
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let timestamp = v.get("timestamp").and_then(Value::as_str).map(String::from);
    Some(TruthRecord {
        role,
        text,
        timestamp,
    })
}

/// Flip `user` roles whose text IS a delivered mail turn to `peer`, the port
/// of peek's `_resolve_peer_roles`: the classifier already lives in this
/// crate (`mail_header::classify`), so the flip is one call, no subprocess.
fn resolve_peer_roles(records: &mut Vec<TruthRecord>) {
    for record in records.iter_mut() {
        if record.role != "user" {
            continue;
        }
        if crate::mail_header::classify(&record.text) != crate::mail_header::Framing::Bare {
            record.role = "peer".to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// The opencode arm: the shared SQLite store, read-only
// ---------------------------------------------------------------------------

/// The opencode store path the age probe resolves against, mirroring
/// `default_opencode_db_path` (`<xdg data home>/opencode/opencode.db`); the
/// truth module does NOT pass a per-call override there, so the age leg of a
/// fixture session (absent from the real store) reads age-unknown by
/// construction.
fn ambient_opencode_db() -> PathBuf {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".local").join("share"),
            None => PathBuf::from("."),
        });
    data_home.join("opencode").join("opencode.db")
}

/// Newest `time_updated` for a session's messages, in epoch SECONDS, or
/// `None`. `time_updated` is epoch millis. A locked or schema-drifted store
/// answers `None` (age-unknown), never an error.
fn opencode_activity_epoch(db: &Path, session_id: &str) -> Option<f64> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(2)).ok()?;
    conn.query_row(
        "SELECT MAX(time_updated) FROM message WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, Option<f64>>(0),
    )
    .ok()?
    .map(|ms| ms / 1000.0)
}

/// The last `n` renderable opencode turns from the store, chronologically
/// (newest last). Port of `_opencode_records_db` + `_opencode_join_parts`:
/// walk newest-first, stop at `n` renderable turns, `text` parts join as
/// text, `tool` parts render `[tool_use: name]`, no renderable text drops
/// the turn.
fn opencode_records_db(db: &Path, session_id: &str, n: usize) -> Vec<TruthRecord> {
    let conn =
        match rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, data FROM message WHERE session_id = ?1 \
             ORDER BY time_created DESC, id DESC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| ())
    else {
        return Vec::new();
    };
    let mut records: Vec<TruthRecord> = Vec::new();
    for row in rows.flatten() {
        let (mid, raw) = row;
        let msg: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let blocks = per_msg_parts(&conn, &mid);
        let text = join_opencode_parts(&blocks);
        if text.is_empty() {
            continue;
        }
        let role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        records.push(TruthRecord {
            role,
            text,
            timestamp: None,
        });
        if records.len() >= n {
            break;
        }
    }
    records.reverse();
    records
}

/// One message's parts, creation-ordered (`time_created, id`).
fn per_msg_parts(conn: &rusqlite::Connection, message_id: &str) -> Vec<Value> {
    let sql = "SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created, id";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt
        .query_map([message_id], |row| row.get::<_, String>(0))
        .map_err(|_| ())
    else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect()
}

/// The part-rendering policy shared by the opencode arms, mirroring peek's
/// `_opencode_join_parts`: `text` joins as text, `tool` renders a compact
/// marker, everything else is observe-noise.
fn join_opencode_parts(blocks: &[Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for p in blocks {
        if p.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                parts.push(t.to_string());
            }
        } else if p.get("type").and_then(Value::as_str) == Some("tool") {
            let name = p.get("tool").and_then(Value::as_str).unwrap_or("?");
            parts.push(format!("[tool_use: {name}]"));
        }
    }
    parts
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// observed_model + harness_title: the provenance reads
// ---------------------------------------------------------------------------

/// Model tail window (256 KiB) and the claude synthetic placeholder, the
/// port of `observed.py`.
const MODEL_TAIL_BYTES: u64 = 256 * 1024;
const CLAUDE_SYNTHETIC_MODEL: &str = "<synthetic>";

/// What the session ACTUALLY answered as, read from its own transcript.
/// Five outcomes, not collapsed: observed / no-transcript / not-file-backed
/// / no-model-yet / unreadable. Never raises.
fn observed_model(agent: &str, transcript_path: Option<&Path>) -> Value {
    if agent != "claude" && agent != "codex" {
        return json!({"kind": "not-file-backed"});
    }
    let Some(path) = transcript_path else {
        return json!({"kind": "no-transcript"});
    };
    let size = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(error) => {
            return if error.kind() == std::io::ErrorKind::NotFound {
                json!({"kind": "no-transcript"})
            } else {
                json!({"kind": "unreadable", "reason": error.to_string()})
            }
        }
    };
    if size == 0 {
        return json!({"kind": "no-model-yet"});
    }
    let windowed = size > MODEL_TAIL_BYTES;
    let start = size.saturating_sub(MODEL_TAIL_BYTES);
    let bytes = match read_range(path, start, size) {
        Some(bytes) => bytes,
        None => return json!({"kind": "unreadable", "reason": "read failed"}),
    };
    let text = String::from_utf8_lossy(&bytes);
    if text.is_empty() {
        return json!({"kind": "no-model-yet"});
    }
    if !text.ends_with('\n') {
        return json!({"kind": "unreadable", "reason": "torn final line (mid-write)"});
    }
    let (last, samples) = models_in(text.split('\n').skip(windowed as usize), |v| {
        model_of(agent, v)
    });
    if last.is_none() && windowed {
        // The tail was inconclusive; escalate to a full scan before claiming
        // absence, the escalation `observed.py` documents (codex stamps the
        // model once per TURN, which a tool-heavy turn pushes out of window).
        let all = match std::fs::read(path) {
            Ok(all) => all,
            Err(error) => return json!({"kind": "unreadable", "reason": error.to_string()}),
        };
        let (l2, s2) = models_in(String::from_utf8_lossy(&all).lines(), |v| {
            model_of(agent, v)
        });
        return match l2 {
            Some(model) => json!({"kind": "observed", "model": model, "samples": s2}),
            None => json!({"kind": "no-model-yet"}),
        };
    }
    match last {
        Some(model) => json!({"kind": "observed", "model": model, "samples": samples}),
        None => json!({"kind": "no-model-yet"}),
    }
}

/// `(most recent model, how many records carried one)` over `lines`.
fn models_in<'a, I: Iterator<Item = &'a str>>(
    lines: I,
    reader: impl Fn(&Value) -> Option<String>,
) -> (Option<String>, usize) {
    let mut last: Option<String> = None;
    let mut samples = 0usize;
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(model) = reader(&rec) {
            last = Some(model);
            samples += 1;
        }
    }
    (last, samples)
}

fn model_of(agent: &str, rec: &Value) -> Option<String> {
    match agent {
        "claude" => {
            if rec.get("type").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            let msg = rec.get("message")?;
            let model = msg.get("model")?.as_str()?;
            (model != CLAUDE_SYNTHETIC_MODEL).then(|| model.to_string())
        }
        _ => {
            if rec.get("type").and_then(Value::as_str) != Some("turn_context") {
                return None;
            }
            rec.get("payload")?
                .get("model")
                .and_then(Value::as_str)
                .map(String::from)
        }
    }
}

/// The title the HARNESS carries for this session (claude only: the last
/// `agent-name` record), newest-first, stopping at the newest one.
fn observed_title(agent: &str, transcript_path: Option<&Path>) -> Option<String> {
    if agent != "claude" {
        return None;
    }
    let path = transcript_path?;
    let bytes = std::fs::read(path).ok()?;
    for line in String::from_utf8_lossy(&bytes).lines().rev() {
        if !line.contains("agent-name") {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if rec.get("type").and_then(Value::as_str) == Some("agent-name") {
            if let Some(name) = rec.get("agentName").and_then(Value::as_str) {
                if !name.trim().is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Read `[start, size)` of a file in one call.
fn read_range(path: &Path, start: u64, size: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut out = Vec::new();
    file.take(size - start).read_to_end(&mut out).ok()?;
    Some(out)
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

/// One resolved session: the transcript triple the reader needs, plus the
/// registry row when the handle named one (it carries the falsifier).
pub(crate) struct TruthSession {
    pub agent: String,
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub row: Option<RegistryEntry>,
}

/// Resolve a truth handle: a registry row by name, alias, short id, or
/// session id; then a session-id-shaped handle through the claude projects
/// store and the codex sessions tree. Nothing else (Five questions 3). A
/// handle naming no row and no session-id transcript answers `not-found`
/// with no suggestions. Never fails hard.
fn resolve_handle(
    rows: Option<&[RegistryEntry]>,
    handle: &str,
    stores: &Stores,
) -> Option<TruthSession> {
    if let Some(rows) = rows {
        let mut matches: Vec<&RegistryEntry> = Vec::new();
        for row in rows {
            let session_keys = [row.harness_session_id.as_deref(), row.session_id.as_deref()];
            let id_match = !row
                .extra
                .get("identity_provisional")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && session_keys
                    .iter()
                    .flatten()
                    .any(|key| !key.is_empty() && *key == handle);
            let name_match = row.name == handle
                || row.aliases.iter().any(|a| a == handle)
                || (!row.short_id.is_empty() && row.short_id == handle);
            if id_match || name_match {
                matches.push(row);
            }
        }
        // One row wins; several rows naming one handle is ambiguity, and the
        // native resolver carries no suggestions to offer, so: not-found.
        if matches.len() == 1 {
            let row = matches.remove(0);
            return Some(session_from_row(row));
        }
        if matches.len() > 1 {
            return None;
        }
    }
    // No registry row named the handle. A claude-UUID-shaped handle resolves
    // through the projects store; a codex id through the sessions tree.
    if is_claude_uuid(handle) {
        if let Some(path) = claude_path_for(handle, stores) {
            return Some(TruthSession {
                agent: "claude".into(),
                session_id: handle.to_string(),
                transcript_path: Some(path),
                row: None,
            });
        }
    }
    if let Some(path) = crate::codex_store::codex_rollout_path(None, handle) {
        return Some(TruthSession {
            agent: "codex".into(),
            session_id: handle.to_string(),
            transcript_path: Some(path),
            row: None,
        });
    }
    None
}

/// The registry-session rung: agent, session id, cwd from the row; the
/// transcript file resolves at read time.
fn session_from_row(row: &RegistryEntry) -> TruthSession {
    TruthSession {
        agent: row.harness.clone().unwrap_or_else(|| "claude".into()),
        session_id: row
            .harness_session_id
            .clone()
            .or_else(|| row.session_id.clone())
            .unwrap_or_default(),
        transcript_path: row.transcript_path.as_deref().map(PathBuf::from),
        row: Some(row.clone()),
    }
}

/// The claude projects store leg of the session-id rung.
fn claude_path_for(handle: &str, stores: &Stores) -> Option<PathBuf> {
    let listing = store_listing(&stores.projects_root);
    choose_from(&listing, handle)
}

/// The claude session-id shape (`_CLAUDE_UUID_RE`): 8-4-4-4-12 hex.
fn is_claude_uuid(handle: &str) -> bool {
    let parts: Vec<&str> = handle.split('-').collect();
    let widths = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts.iter().zip(widths.iter()).all(|(part, width)| {
            part.len() == *width && part.chars().all(|c| c.is_ascii_hexdigit())
        })
}

/// The store paths a truth read resolves against: injected by tests, ambient
/// everywhere else. opencode's store arrives through `OPENCODE_DB` (the same
/// seam `opencode_transcript.rs` honors); claude's through
/// `CLAUDE_CONFIG_DIR` at read time via `claude_roster::config_dir`.
pub struct Stores {
    pub projects_root: PathBuf,
    pub codex_sessions_dir: Option<PathBuf>,
    pub opencode_db: PathBuf,
}

impl Stores {
    /// The ambient stores the daemon and CLI processes read.
    pub fn ambient() -> Self {
        Self {
            projects_root: crate::claude_roster::config_dir().join("projects"),
            codex_sessions_dir: None,
            opencode_db: match std::env::var_os("OPENCODE_DB") {
                Some(db) if !db.is_empty() => PathBuf::from(db),
                _ => ambient_opencode_db(),
            },
        }
    }
}

/// The reachability verdict: a falsifier answers unreachable; active states
/// answer from the transcript's freshness and model evidence; done/stalled
/// answer unknown/silent; unknown answers unknown/no-evidence. The port of
/// `classify_reachability` (reachability.py), kept monotone: it only ever
/// lowers, never invents a verdict.
fn classify_reachability(
    truth_state: Option<&str>,
    age_s: Option<i64>,
    falsifier: Option<&'static str>,
    last_activity_basis: Option<&str>,
    observed: &Value,
) -> (&'static str, &'static str) {
    if let Some(word) = falsifier {
        // The falsifier word IS the basis: process-gone / pane-gone /
        // exit-recorded, the evidence the verdict was reached from.
        return ("unreachable", word);
    }
    if truth_state == Some("working")
        || truth_state == Some("watching")
        || truth_state == Some("your-move")
    {
        if last_activity_basis == Some("mtime") {
            return ("unknown", "mtime-only");
        }
        if inference_samples(observed) == Some(0) {
            return ("unknown", "no-inference");
        }
        if age_s.is_some_and(|a| a > TRANSCRIPT_EVIDENCE_S) {
            return ("unknown", "stale-transcript");
        }
        return ("reachable", "transcript");
    }
    if truth_state.is_none() || truth_state == Some("unknown") {
        return ("unknown", "no-evidence");
    }
    ("unknown", "silent")
}

/// The reachability window (20 minutes), `reachability.py`'s
/// TRANSCRIPT_EVIDENCE_S: an active tail older than this is stale evidence.
const TRANSCRIPT_EVIDENCE_S: i64 = 20 * 60;

/// Model-bearing records the transcript tail carried, or None for absence.
/// `no-model-yet` is a real zero; every other variant is an absence and
/// lowers nothing. The port of `inference_samples`.
fn inference_samples(observed: &Value) -> Option<usize> {
    let kind = observed.get("kind")?.as_str()?;
    if kind == "no-model-yet" {
        return Some(0);
    }
    if kind != "observed" {
        return None;
    }
    observed
        .get("samples")
        .and_then(Value::as_u64)
        .map(|s| s as usize)
}

/// The falsifier a registry row carries, the port of `registry_falsifier`:
/// a mux-pane row is falsified by its PANE, never by its recorded pid; a
/// non-pane row by its pid or its exit tombstone; a claude row's negative
/// yields to claude's own session records (a proven live holder cancels).
fn registry_falsifier(row: &RegistryEntry) -> Option<&'static str> {
    if let Some(mux) = row.mux.as_ref() {
        if mux_ref_names_a_pane(mux) {
            return pane_falsifier(mux);
        }
    }
    let falsifier = pid_falsifier(row.pid, row.pid_start_time).or_else(|| exit_falsifier(row));
    if falsifier.is_some() && row.harness.as_deref() == Some("claude") {
        if let Some(sid) = row.harness_session_id.as_deref() {
            if claude_holder_proven(sid) {
                return None;
            }
        }
    }
    falsifier
}

fn mux_ref_names_a_pane(mux: &crate::state::MuxRef) -> bool {
    !mux.session.trim().is_empty() && mux.pane_id >= 1
}

fn pane_falsifier(mux: &crate::state::MuxRef) -> Option<&'static str> {
    match crate::spawn_gate_lanes::mux_pane_alive(&mux.session, mux.pane_id) {
        Ok(false) => Some("pane-gone"),
        _ => None,
    }
}

fn pid_falsifier(pid: Option<u32>, pid_start_time: Option<u64>) -> Option<&'static str> {
    let pid = pid?;
    if pid <= 1 || pid as i64 > MAX_PID {
        return Some("process-gone");
    }
    match crate::claims::probe_pid(pid as i32) {
        crate::claims::PidProbe::Absent => Some("process-gone"),
        crate::claims::PidProbe::Refused => None,
        crate::claims::PidProbe::Created(_) => match crate::daemon::process_start_time(pid) {
            None => None,
            Some(current) => {
                let matches_start = pid_start_time.is_none_or(|start| current == start);
                if matches_start {
                    None
                } else {
                    Some("process-gone")
                }
            }
        },
    }
}

const MAX_PID: i64 = 0x7FFF_FFFF;

/// `exit-recorded` when reconcile already proved this row's child gone. The
/// ONE stored status that is a probe RESULT rather than a guess.
fn exit_falsifier(row: &RegistryEntry) -> Option<&'static str> {
    if row.status == crate::AgentStatus::Exited {
        Some("exit-recorded")
    } else {
        None
    }
}

/// True only when claude's own per-process records prove a live process
/// holds the session, the same proof `_claude_holder_proven` shells to. An
/// error, a timeout, or a not-held answer is never a cancellation.
fn claude_holder_proven(session_id: &str) -> bool {
    let dirs = crate::claude_ask::ClaudeHome::from_env().sessions_dirs();
    matches!(
        crate::claude_sessions::session_record_holder(&dirs, session_id, &|pid| {
            crate::claims::process_create_time_ms(pid as i32)
        }),
        crate::pane_stop::SessionHolder::Held { proven: true, .. }
    )
}

/// The provider refusal a worker's own last turn carries, the port of the
/// live-transcript subset of `classify_worker_refusal`: only the leading
/// 120 chars classify, and with no HTTP status the only swap-triggering
/// class reachable is the quota one. The error-class strings stay verbatim.
fn provider_refusal_of(last_text: &str) -> Option<String> {
    const QUOTA_MARKERS: [&str; 3] = ["rate limit", "quota exceeded", "usage limit"];
    let collapsed: String = last_text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lead: String = collapsed.chars().take(120).collect();
    let lead_lower = lead.to_lowercase();
    QUOTA_MARKERS
        .iter()
        .find(|marker| lead_lower.contains(*marker))
        .map(|_| "provider_4xx_quota".to_string())
}

// ---------------------------------------------------------------------------
// The one reader: resolve + read + classify + the wire payload
// ---------------------------------------------------------------------------

/// The answer payload for a handle the resolver could not answer, or whose
/// transcript read empty. `observed` renders absence as the harness's
/// not-file-backed fact, never as a fabricated variant.
fn unknown_payload(
    handle: &str,
    reason: &str,
    sid: Option<&str>,
    observed: Option<Value>,
) -> Value {
    json!({
        "handle": handle,
        "state": "unknown",
        "reason": reason,
        "last_activity_age_s": Value::Null,
        "last_event_at": Value::Null,
        "last_activity_basis": Value::Null,
        "last_message": Value::Null,
        "provider_refusal": Value::Null,
        "session_id": sid,
        "observed_model": observed.unwrap_or_else(|| json!({"kind": "no-transcript"})),
        "harness_title": Value::Null,
        "suggestions": [],
        "reachability": "unknown",
        "basis": "no-evidence",
        "falsifier_error": Value::Null,
    })
}

/// One truth answer: resolve, read the tail through the cursors, classify,
/// derive reachability from the registry falsifier, and emit the wire
/// payload `parse_truth_payload` decodes. Never panics; every failure is an
/// `unknown` payload naming its Python reason.
pub fn resolve_payload(
    rows: Option<&[RegistryEntry]>,
    handle: &str,
    now_s: f64,
    stores: &Stores,
    cursors: &mut TruthCursors,
) -> Value {
    let Some(session) = resolve_handle(rows, handle, stores) else {
        return unknown_payload(handle, "not-found", None, None);
    };
    let path = transcript_for(&session, stores);
    let observed = observed_model(&session.agent, path.as_deref());
    let records = read_records(cursors, stores, &session, path.as_deref());
    if records.is_empty() {
        return unknown_payload(
            handle,
            "no-records",
            Some(&session.session_id),
            Some(observed),
        );
    }
    let (epoch, basis): (Option<f64>, Option<String>) = match newest_stamp(&records, &session.agent)
    {
        Some(stamp) => (Some(stamp), Some("last-entry".to_string())),
        None => age_fallback(&session, now_s, path.as_deref()),
    };
    let age: Option<f64> = epoch.map(|stamp| (now_s - stamp).max(0.0));
    let last = records.last().cloned().unwrap_or_else(|| TruthRecord {
        role: String::new(),
        text: String::new(),
        timestamp: None,
    });
    let last_actor = records
        .iter()
        .rev()
        .find(|r| r.role != "peer")
        .cloned()
        .unwrap_or_else(|| last.clone());
    let state = classify_tail(
        Some(last_actor.role.as_str()),
        &last_actor.text,
        age,
        STALLED_AFTER_S,
    );
    let reason: Option<&'static str> = if state == "stalled"
        && last_actor.role == "assistant"
        && api_error_re().is_match(last_actor.text.trim_start())
    {
        Some("api-error-tail")
    } else {
        None
    };
    let provider_refusal = if last_actor.role == "assistant" && state == "done" {
        None
    } else {
        provider_refusal_of(&last_actor.text)
    };
    let last_event_at = epoch.and_then(render_stamp);
    let last_message = flatten_200(&last.text);
    let basis_str = basis.as_deref();
    let age_i64 = age.map(|a| a as i64);
    let falsifier = session.row.as_ref().and_then(registry_falsifier);
    let (reachability, reach_basis) =
        classify_reachability(Some(&state), age_i64, falsifier, basis_str, &observed);
    json!({
        "handle": handle,
        "state": state,
        "reason": reason,
        "last_activity_age_s": age_i64,
        "last_event_at": last_event_at,
        "last_activity_basis": basis_str,
        "last_message": last_message,
        "provider_refusal": provider_refusal,
        "session_id": session.session_id,
        "harness_title": observed_title(&session.agent, path.as_deref()),
        "suggestions": [],
        "reachability": reachability,
        "basis": reach_basis,
        "falsifier_error": Value::Null,
    })
}

/// The transcript file for a resolved session: the row's own stamp when it
/// carries one, else the harness lookup (claude projects store; codex
/// sessions tree). opencode has none.
fn transcript_for(session: &TruthSession, stores: &Stores) -> Option<PathBuf> {
    if let Some(path) = session.transcript_path.as_ref() {
        return Some(path.clone());
    }
    match session.agent.as_str() {
        "claude" if !session.session_id.is_empty() => claude_path_for(&session.session_id, stores),
        "codex" => crate::codex_store::codex_rollout_path(
            stores.codex_sessions_dir.as_deref(),
            &session.session_id,
        ),
        _ => None,
    }
}

/// The per-harness tail read: claude/codex through the cursors, opencode
/// from its store, every other harness empty. An unregistered harness reads
/// `unknown`/`no-records`, matching peek's ObserveUnsupported handling.
fn read_records(
    cursors: &mut TruthCursors,
    stores: &Stores,
    session: &TruthSession,
    path: Option<&Path>,
) -> Vec<TruthRecord> {
    let mut records: Vec<TruthRecord> = match (session.agent.as_str(), path) {
        ("claude", Some(path)) => {
            cursors
                .read_tail(path, TAIL_N, &parse_json_line(parse_claude_record))
                .unwrap_or_default()
                .0
        }
        ("codex", Some(path)) => {
            cursors
                .read_tail(path, TAIL_N, &parse_json_line(parse_codex_record))
                .unwrap_or_default()
                .0
        }
        ("opencode", _) => {
            if stores.opencode_db.exists() {
                opencode_records_db(&stores.opencode_db, &session.session_id, TAIL_N)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    resolve_peer_roles(&mut records);
    records
}

/// One JSONL line parsed to a `Value`, then to a record.
fn parse_json_line(
    inner: fn(&Value) -> Option<TruthRecord>,
) -> impl Fn(&str) -> Option<TruthRecord> {
    move |line: &str| {
        let v: Value = serde_json::from_str(line).ok()?;
        inner(&v)
    }
}

/// The newest parseable record stamp (epoch seconds), newest-first. claude
/// and codex only; opencode records carry no stamp.
fn newest_stamp(records: &[TruthRecord], agent: &str) -> Option<f64> {
    if agent != "claude" && agent != "codex" {
        return None;
    }
    for record in records.iter().rev() {
        let Some(ts) = record.timestamp.as_deref() else {
            continue;
        };
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00")) else {
            continue;
        };
        return Some(dt.timestamp() as f64);
    }
    None
}

/// The mtime / opencode-db fallback leg of the age read, the port of
/// `_transcript_age_s`: one read yields BOTH the stamp and the age, so the
/// pair cannot disagree about one transcript. Returns `(age, basis)`.
fn age_fallback(
    session: &TruthSession,
    now_s: f64,
    path: Option<&Path>,
) -> (Option<f64>, Option<String>) {
    match session.agent.as_str() {
        "claude" | "codex" => {
            let Some(path) = path else {
                return (None, None);
            };
            let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
            let epoch = mtime
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64());
            match epoch {
                Some(epoch) => (Some((now_s - epoch).max(0.0)), Some("mtime".to_string())),
                None => (None, None),
            }
        }
        _ => match opencode_activity_epoch(&ambient_opencode_db(), &session.session_id) {
            Some(epoch) => (
                Some((now_s - epoch).max(0.0)),
                Some("opencode-db".to_string()),
            ),
            None => (None, None),
        },
    }
}

/// Epoch seconds to the wire stamp (`YYYY-MM-DDTHH:MM:SSZ`), or `None` for
/// an out-of-range epoch: a stamp that cannot render degrades to absence,
/// the never-raises contract.
fn render_stamp(epoch: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(epoch as i64, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Collapsed whitespace, capped at 200 chars, empty is null: the
/// `last_message` wire field.
fn flatten_200(text: &str) -> Value {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(200).collect();
    if capped.is_empty() {
        Value::Null
    } else {
        Value::String(capped)
    }
}
