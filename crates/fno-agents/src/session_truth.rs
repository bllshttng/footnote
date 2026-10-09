//! Native session truth: one reader for a session's supervision state, read
//! from its transcript TAIL.
//!
//! The state words and the payload are the port of `resolve_session_truth`
//! (`cli/src/fno/agents/session_truth.py`), field for field: `handle, state,
//! reason, last_activity_age_s, last_event_at, last_activity_basis,
//! last_message, provider_refusal, session_id, observed_model,
//! harness_title, suggestions`. The daemon answers truth from this module in
//! process, from one cursor per transcript; no `fno agents truth` child ever
//! starts on that path.
//!
//! A cursor keeps a byte offset and a derived [`TailSummary`], never
//! transcript text, and drops when its session ends or goes idle. Right
//! after a daemon restart a session whose cursor is not rebuilt yet answers
//! `warming`: ask again.
//!
//! Liveness is transcript-keyed ONLY, exactly as the Python reader states:
//! argv, pid, the daemon record, and state.json have each been caught lying
//! about a live session. Every failure degrades to `unknown` with one of the
//! Python reasons (`not-found` | `no-records`); the reader never panics.

pub mod cursor;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::json;
use serde_json::Value;

use crate::claude_transcript_paths::choose_from;
use crate::state::RegistryEntry;
use cursor::TruthCursors;

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

/// One rendered transcript turn: a role and its human-readable text. Lives
/// only while a line folds; a summary keeps derived facts, never the text.
#[derive(Clone, Debug)]
pub(crate) struct TruthRecord {
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
}

/// The content signal of one assistant turn, in the classifier's order of
/// precedence. Computed once when the turn folds, so the summary never holds
/// the text it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    Watching,
    Promise,
    ApiError,
    Question,
    Plain,
}

impl Signal {
    pub(crate) fn of(text: &str) -> Self {
        if watching_re().is_match(text) {
            return Signal::Watching;
        }
        if promise_re().is_match(text) {
            return Signal::Promise;
        }
        if api_error_re().is_match(text.trim_start()) {
            return Signal::ApiError;
        }
        let stripped = text.trim_end();
        if stripped.ends_with('?')
            || help_re().is_match(text)
            || option_prompt_re().is_match(stripped)
        {
            return Signal::Question;
        }
        Signal::Plain
    }
}

/// Pure classifier over the LAST transcript turn, the one classifier in the
/// repo (`worked.rs` calls it too). Content signals apply only when the last
/// turn is the assistant's; a trailing user turn clears any stale assistant
/// signal and mtime decides. A turn stops being news past `stalled_after_s`;
/// `done` is an outcome and does not go stale.
pub(crate) fn classify_tail(
    last_role: Option<&str>,
    last_text: &str,
    age_s: Option<f64>,
    stalled_after_s: f64,
) -> String {
    classify_signal(last_role, Signal::of(last_text), age_s, stalled_after_s)
}

fn classify_signal(
    last_role: Option<&str>,
    signal: Signal,
    age_s: Option<f64>,
    stalled_after_s: f64,
) -> String {
    let stale = age_s.is_some_and(|a| a > stalled_after_s);
    let word = match (last_role == Some("assistant"), signal) {
        (true, Signal::Watching) if !stale => "watching",
        (true, Signal::Promise) => "done",
        (true, Signal::ApiError) => "stalled",
        (true, Signal::Question) if !stale => "your-move",
        _ if stale => "stalled",
        _ => "working",
    };
    word.to_string()
}

/// What one turn contributes to truth, derived when it folds.
#[derive(Clone, Debug)]
pub(crate) struct TurnFacts {
    role: String,
    signal: Signal,
    api_error: bool,
    refusal: Option<String>,
}

impl TurnFacts {
    fn of(role: String, text: &str) -> Self {
        Self {
            signal: Signal::of(text),
            api_error: api_error_re().is_match(text.trim_start()),
            refusal: provider_refusal_of(text),
            role,
        }
    }
}

/// The derived state of one transcript tail: what the payload needs and
/// nothing more. The only text it keeps is the 200-char `last_message` the
/// wire already carries. The `since_*` counters keep the Python reader's
/// 40-turn window without keeping the 40 turns.
#[derive(Clone, Debug, Default)]
pub(crate) struct TailSummary {
    records: usize,
    last: Option<TurnFacts>,
    last_message: Option<String>,
    actor: Option<TurnFacts>,
    since_actor: usize,
    stamp: Option<f64>,
    since_stamp: usize,
    model: Option<String>,
    model_samples: usize,
    // The model record lies past the bounded scan, so its absence is not
    // proof the session never answered.
    model_unscanned: bool,
    title: Option<String>,
}

impl TailSummary {
    pub(crate) fn has_record(&self) -> bool {
        self.records > 0
    }

    /// True once a non-peer turn sits inside the 40-turn window.
    pub(crate) fn has_actor(&self) -> bool {
        self.actor.is_some() && self.since_actor < TAIL_N
    }

    /// The summary a rebuild starting at a compact boundary would hold: no
    /// turns, the provenance (model, title) kept as its backfill finds it.
    pub(crate) fn after_boundary(&self) -> Self {
        Self {
            model: self.model.clone(),
            model_samples: self.model_samples,
            model_unscanned: self.model_unscanned,
            title: self.title.clone(),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn records(&self) -> usize {
        self.records
    }

    #[cfg(test)]
    pub(crate) fn last_role(&self) -> Option<&str> {
        self.last.as_ref().map(|t| t.role.as_str())
    }

    /// Fold one turn. A user turn that IS a delivered mail turn flips to
    /// `peer` (peek's `_resolve_peer_roles`), so a relay never reads as the
    /// operator's move.
    fn push(&mut self, role: String, text: &str, timestamp: Option<&str>) {
        let peer = role == "user"
            && crate::mail_header::classify(text) != crate::mail_header::Framing::Bare;
        let role = if peer { "peer".to_string() } else { role };
        let facts = TurnFacts::of(role, text);
        self.records = self.records.saturating_add(1);
        self.since_actor = self.since_actor.saturating_add(1);
        self.since_stamp = self.since_stamp.saturating_add(1);
        if facts.role != "peer" {
            self.actor = Some(facts.clone());
            self.since_actor = 0;
        }
        if let Some(epoch) = timestamp.and_then(parse_stamp) {
            self.stamp = Some(epoch);
            self.since_stamp = 0;
        }
        self.last_message = flatten_200(text);
        self.last = Some(facts);
    }

    fn note_model(&mut self, model: String) {
        self.model = Some(model);
        self.model_samples = self.model_samples.saturating_add(1);
        self.model_unscanned = false;
    }

    /// The newest non-peer turn inside the 40-turn window, else the last
    /// turn whatever its role.
    fn last_actor(&self) -> Option<&TurnFacts> {
        match &self.actor {
            Some(actor) if self.since_actor < TAIL_N => Some(actor),
            _ => self.last.as_ref(),
        }
    }

    /// The newest parseable stamp inside the 40-turn window.
    fn newest_stamp(&self) -> Option<f64> {
        self.stamp.filter(|_| self.since_stamp < TAIL_N)
    }
}

fn parse_stamp(ts: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00"))
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

// ---------------------------------------------------------------------------
// Folders: how each harness's lines become a summary
// ---------------------------------------------------------------------------

/// The provenance scans run past the rebuild window only to find a record
/// the window did not carry: codex stamps its model once per turn, which a
/// tool-heavy turn pushes megabytes back.
const CLAUDE_MODEL_SCAN_LIMIT: u64 = 4 << 20;
const CODEX_MODEL_SCAN_LIMIT: u64 = 32 << 20;
const TITLE_SCAN_LIMIT: u64 = 1 << 20;

fn fold_claude(s: &mut TailSummary, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if let Some(model) = model_of("claude", &v) {
        s.note_model(model);
    }
    if let Some(title) = title_of(&v) {
        s.title = Some(title);
    }
    if let Some(r) = parse_claude_record(&v) {
        s.push(r.role, &r.text, r.timestamp.as_deref());
    }
}

fn fold_codex(s: &mut TailSummary, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if let Some(model) = model_of("codex", &v) {
        s.note_model(model);
    }
    if let Some(r) = parse_codex_record(&v) {
        s.push(r.role, &r.text, r.timestamp.as_deref());
    }
}

/// claude's compact boundary: `{"type":"system","subtype":"compact_boundary"}`.
/// Only a line carrying the bare token is parsed to confirm it.
fn claude_boundary(line: &str) -> bool {
    line.contains("\"compact_boundary\"")
        && serde_json::from_str::<Value>(line).is_ok_and(|v| {
            v.get("type").and_then(Value::as_str) == Some("system")
                && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        })
}

/// codex's compact record: a top-level `"type":"compacted"` in the line's
/// head. Its line carries the whole replacement history (hundreds of KB), so
/// the head test decides without a parse.
fn codex_boundary(line: &str) -> bool {
    let head = &line.as_bytes()[..line.len().min(200)];
    head.windows(18).any(|w| w == b"\"type\":\"compacted\"")
}

fn backfill_claude(s: &mut TailSummary, path: &Path, before: u64) {
    if s.model.is_none() {
        let limit = CLAUDE_MODEL_SCAN_LIMIT;
        let found = cursor::scan_back(path, before, b"\"model\":\"", limit, &|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| model_of("claude", &v))
        });
        backfill_model(s, found, before, limit);
    }
    if s.title.is_none() {
        s.title = cursor::scan_back(path, before, b"\"agent-name\"", TITLE_SCAN_LIMIT, &|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| title_of(&v))
        });
    }
}

fn backfill_codex(s: &mut TailSummary, path: &Path, before: u64) {
    if s.model.is_none() {
        let limit = CODEX_MODEL_SCAN_LIMIT;
        let found = cursor::scan_back(path, before, b"turn_context", limit, &|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| model_of("codex", &v))
        });
        backfill_model(s, found, before, limit);
    }
}

fn backfill_model(s: &mut TailSummary, found: Option<String>, before: u64, limit: u64) {
    match found {
        Some(model) => s.note_model(model),
        None => s.model_unscanned = before > limit,
    }
}

/// The folder for a file-backed harness.
pub(crate) fn folder_for(agent: &str) -> cursor::Folder<'static> {
    match agent {
        "codex" => cursor::Folder {
            fold: &fold_codex,
            is_boundary: &codex_boundary,
            backfill: &backfill_codex,
        },
        _ => cursor::Folder {
            fold: &fold_claude,
            is_boundary: &claude_boundary,
            backfill: &backfill_claude,
        },
    }
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

// ---------------------------------------------------------------------------
// The opencode arm: the shared SQLite store, read-only
// ---------------------------------------------------------------------------

/// The opencode store path the age probe resolves against: `OPENCODE_DB`
/// when set, else `default_opencode_db_path` (`<xdg data home>/opencode/
/// opencode.db`). The records leg reads the same env, so records and age
/// never come from two stores. The truth module does NOT pass a per-call
/// override here, so the age leg of a fixture session (absent from the real
/// store) reads age-unknown by construction.
fn ambient_opencode_db() -> PathBuf {
    if let Some(db) = std::env::var_os("OPENCODE_DB").filter(|v| !v.is_empty()) {
        return PathBuf::from(db);
    }
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
        // A compaction part opens the post-compact window: nothing older
        // matters for truth.
        if blocks
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) == Some("compaction"))
        {
            break;
        }
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

/// The claude synthetic placeholder, the port of `observed.py`.
const CLAUDE_SYNTHETIC_MODEL: &str = "<synthetic>";

/// What the session ACTUALLY answered as, read from its summary. Five
/// outcomes, not collapsed: observed / no-transcript / not-file-backed /
/// no-model-yet / unreadable. Never raises.
fn observed_model(agent: &str, path: Option<&Path>, summary: Option<&TailSummary>) -> Value {
    if agent != "claude" && agent != "codex" {
        return json!({"kind": "not-file-backed"});
    }
    let Some(path) = path else {
        return json!({"kind": "no-transcript"});
    };
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json!({"kind": "no-transcript"})
        }
        Err(error) => return json!({"kind": "unreadable", "reason": error.to_string()}),
        Ok(meta) if meta.len() == 0 => return json!({"kind": "no-model-yet"}),
        Ok(_) => {}
    }
    let Some(summary) = summary else {
        return json!({"kind": "unreadable", "reason": "read failed"});
    };
    match &summary.model {
        Some(model) => {
            json!({"kind": "observed", "model": model, "samples": summary.model_samples})
        }
        // Absence past the bounded scan is not a zero: `no-model-yet` would
        // lower a live session's reachability on no evidence.
        None if summary.model_unscanned => {
            json!({"kind": "unreadable", "reason": "model record past the scan window"})
        }
        None => json!({"kind": "no-model-yet"}),
    }
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

/// The title the HARNESS carries for this session: claude's `agent-name`
/// record, when it names one.
fn title_of(rec: &Value) -> Option<String> {
    if rec.get("type").and_then(Value::as_str) != Some("agent-name") {
        return None;
    }
    let name = rec.get("agentName").and_then(Value::as_str)?;
    (!name.trim().is_empty()).then(|| name.to_string())
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
/// session id; then a session-id-shaped handle (a full id or an 8+ char id
/// prefix) through the claude projects store and the codex sessions tree.
/// Nothing else (Five questions 3). A handle naming no row and no transcript
/// answers `not-found` with no suggestions. Never fails hard.
fn resolve_handle<C: CursorAccess>(
    rows: Option<&[RegistryEntry]>,
    handle: &str,
    stores: &Stores,
    cursors: &mut C,
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
            return Some(session_from_row(matches[0]));
        }
        if matches.len() > 1 {
            return None;
        }
    }
    if !is_session_id_prefix(handle) {
        return None;
    }
    // Only the store leg is costly, so only it remembers a miss: a row that
    // registers inside the beat still resolves on the next ask.
    if cursors.with(|c| c.recent_miss(handle)) {
        return None;
    }
    for agent in ["claude", "codex"] {
        if let Some(path) = lookup_path(cursors, stores, agent, handle) {
            return Some(TruthSession {
                agent: agent.into(),
                session_id: handle.to_string(),
                transcript_path: Some(path),
                row: None,
            });
        }
    }
    cursors.with(|c| c.note_miss(handle));
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
fn claude_path_for(
    handle: &str,
    listing: &[crate::claude_transcript_paths::Hit],
) -> Option<PathBuf> {
    // A short prefix naming two sessions is ambiguous: answer nothing rather
    // than the first sorted session's truth.
    let mut names = listing
        .iter()
        .filter(|h| h.name.starts_with(handle))
        .map(|h| h.name.as_str());
    let first = names.next()?;
    if names.any(|n| n != first) {
        return None;
    }
    choose_from(listing, handle)
}

/// A session id or an id prefix: 8+ hex digits and dashes. The claude
/// store matches a prefix only when it names one session.
fn is_session_id_prefix(handle: &str) -> bool {
    handle.len() >= 8
        && handle.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && handle.chars().filter(char::is_ascii_hexdigit).count() >= 8
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
            opencode_db: ambient_opencode_db(),
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

/// [`registry_falsifier`] with a one-beat memo per row: the pane probe spawns
/// `fno mux` and the claude holder proof lists claude's session records, so a
/// sweep that asks every few seconds pays for each at most once a beat.
fn row_falsifier<C: CursorAccess>(row: &RegistryEntry, cursors: &mut C) -> Option<&'static str> {
    let key = format!(
        "{}|{:?}|{:?}|{:?}",
        row.name,
        row.pid,
        row.status,
        row.mux.as_ref().map(|m| (&m.session, m.pane_id))
    );
    if let Some(verdict) = cursors.with(|c| c.cached_verdict(&key)) {
        return verdict;
    }
    let verdict = registry_falsifier(row);
    cursors.with(|c| c.cache_verdict(&key, verdict));
    verdict
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

/// How a caller reaches the cursor set: a test owns one outright, the daemon
/// shares the process-global one behind its mutex. The lock is held only
/// around a cursor operation, never across a falsifier probe.
pub trait CursorAccess {
    fn with<R>(&mut self, f: impl FnOnce(&mut TruthCursors) -> R) -> R;
}

impl CursorAccess for &mut TruthCursors {
    fn with<R>(&mut self, f: impl FnOnce(&mut TruthCursors) -> R) -> R {
        f(self)
    }
}

impl CursorAccess for &std::sync::Mutex<TruthCursors> {
    fn with<R>(&mut self, f: impl FnOnce(&mut TruthCursors) -> R) -> R {
        f(&mut self.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

/// `Build` reads a missing cursor on demand. `WarmOnly` is the daemon right
/// after a restart: a session whose cursor the warm pass has not rebuilt yet
/// answers `warming`, never a cold guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Build,
    WarmOnly,
}

/// The answer payload for a handle the resolver could not answer, or whose
/// transcript read empty. A registry falsifier still applies, as in Python.
fn unknown_payload(
    handle: &str,
    reason: &str,
    sid: Option<&str>,
    observed: Option<Value>,
    falsifier: Option<&'static str>,
) -> Value {
    let (reachability, basis) =
        classify_reachability(Some("unknown"), None, falsifier, None, &Value::Null);
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
        "reachability": reachability,
        "basis": basis,
        "falsifier_error": Value::Null,
    })
}

/// The restart answer: the cursor is not rebuilt yet, so ask again. It is
/// neither dead nor busy: every reaper and liveness reader takes its
/// inconclusive branch, and the spawn gate refuses with a retry.
fn warming_payload(handle: &str, sid: &str) -> Value {
    json!({
        "handle": handle,
        "state": "warming",
        "reason": "warming",
        "last_activity_age_s": Value::Null,
        "last_event_at": Value::Null,
        "last_activity_basis": "warming",
        "last_message": Value::Null,
        "provider_refusal": Value::Null,
        "session_id": sid,
        "observed_model": {"kind": "unreadable", "reason": "truth warming; retry"},
        "harness_title": Value::Null,
        "suggestions": [],
        "reachability": "unknown",
        "basis": "warming",
        "falsifier_error": Value::Null,
    })
}

/// One truth answer, building a missing cursor on demand.
pub fn resolve_payload<C: CursorAccess>(
    rows: Option<&[RegistryEntry]>,
    handle: &str,
    now_s: f64,
    stores: &Stores,
    cursors: C,
) -> Value {
    resolve_payload_with(rows, handle, now_s, stores, cursors, ReadMode::Build)
}

/// One truth answer: resolve, read the summary through the cursors,
/// classify, derive reachability from the registry falsifier, and emit the
/// wire payload `parse_truth_payload` decodes. Never panics; every failure is
/// an `unknown` payload naming its Python reason. A handle that resolved to
/// nothing answers `not-found` from memory for one beat, so a caller that
/// asks every sweep pays one bounded attempt per beat, never a loop.
pub fn resolve_payload_with<C: CursorAccess>(
    rows: Option<&[RegistryEntry]>,
    handle: &str,
    now_s: f64,
    stores: &Stores,
    mut cursors: C,
    mode: ReadMode,
) -> Value {
    let Some(session) = resolve_handle(rows, handle, stores, &mut cursors) else {
        return unknown_payload(handle, "not-found", None, None, None);
    };
    let path = transcript_for(&session, stores, &mut cursors);
    let store_key = format!("opencode:{}", session.session_id);
    let summary = match (session.agent.as_str(), path.as_deref()) {
        ("claude" | "codex", Some(p)) => {
            if mode == ReadMode::WarmOnly && !cursors.with(|c| c.has(p)) {
                return warming_payload(handle, &session.session_id);
            }
            let folder = folder_for(&session.agent);
            cursors.with(|c| c.summary(p, &folder))
        }
        ("opencode", _) => {
            if mode == ReadMode::WarmOnly && !cursors.with(|c| c.has_store(&store_key)) {
                return warming_payload(handle, &session.session_id);
            }
            Some(opencode_summary(
                &mut cursors,
                stores,
                &session.session_id,
                &store_key,
            ))
        }
        _ => None,
    };
    let falsifier = session
        .row
        .as_ref()
        .and_then(|row| row_falsifier(row, &mut cursors));
    let observed = observed_model(&session.agent, path.as_deref(), summary.as_ref());
    let Some(summary) = summary.filter(TailSummary::has_record) else {
        return unknown_payload(
            handle,
            "no-records",
            Some(&session.session_id),
            Some(observed),
            falsifier,
        );
    };
    let stamp = match session.agent.as_str() {
        "claude" | "codex" => summary.newest_stamp(),
        _ => None,
    };
    let (epoch, basis): (Option<f64>, Option<String>) = match stamp {
        Some(stamp) => (Some(stamp), Some("last-entry".to_string())),
        None => age_fallback(&session, path.as_deref()),
    };
    let age: Option<f64> = epoch.map(|stamp| (now_s - stamp).max(0.0));
    let actor = summary.last_actor();
    let role = actor.map(|a| a.role.as_str());
    let state = classify_signal(
        role,
        actor.map_or(Signal::Plain, |a| a.signal),
        age,
        STALLED_AFTER_S,
    );
    let reason: Option<&'static str> =
        (state == "stalled" && role == Some("assistant") && actor.is_some_and(|a| a.api_error))
            .then_some("api-error-tail");
    let provider_refusal = if role == Some("assistant") && state == "done" {
        None
    } else {
        actor.and_then(|a| a.refusal.clone())
    };
    let basis_str = basis.as_deref();
    let age_i64 = age.map(|a| a as i64);
    let (reachability, reach_basis) =
        classify_reachability(Some(&state), age_i64, falsifier, basis_str, &observed);
    json!({
        "handle": handle,
        "state": state,
        "reason": reason,
        "last_activity_age_s": age_i64,
        "last_event_at": epoch.and_then(render_stamp),
        "last_activity_basis": basis_str,
        "last_message": summary.last_message,
        "provider_refusal": provider_refusal,
        "session_id": session.session_id,
        "observed_model": observed,
        "harness_title": summary.title,
        "suggestions": [],
        "reachability": reachability,
        "basis": reach_basis,
        "falsifier_error": Value::Null,
    })
}

/// Build (or reuse) the cursor for one registry row. The daemon's warm pass
/// calls this per row after a restart; `false` when the row names no
/// readable transcript.
pub(crate) fn warm_row<C: CursorAccess>(
    row: &RegistryEntry,
    stores: &Stores,
    mut cursors: C,
) -> bool {
    let session = session_from_row(row);
    match session.agent.as_str() {
        "claude" | "codex" => {
            let Some(path) = transcript_for(&session, stores, &mut cursors) else {
                return false;
            };
            let folder = folder_for(&session.agent);
            cursors.with(|c| c.summary(&path, &folder)).is_some()
        }
        "opencode" => {
            let key = format!("opencode:{}", session.session_id);
            opencode_summary(&mut cursors, stores, &session.session_id, &key);
            true
        }
        _ => false,
    }
}

/// The opencode summary, rebuilt only when the session's rows in the store
/// moved.
fn opencode_summary<C: CursorAccess>(
    cursors: &mut C,
    stores: &Stores,
    session_id: &str,
    key: &str,
) -> TailSummary {
    if !stores.opencode_db.exists() {
        return TailSummary::default();
    }
    let version = opencode_version(&stores.opencode_db, session_id);
    cursors.with(|c| {
        c.store_summary(key, version, || {
            let mut summary = TailSummary::default();
            for r in opencode_records_db(&stores.opencode_db, session_id, TAIL_N) {
                summary.push(r.role, &r.text, None);
            }
            summary
        })
    })
}

/// A cheap change marker for one opencode session: newest part write and
/// part count. `None` when the store cannot answer, which always rebuilds.
fn opencode_version(db: &Path, session_id: &str) -> Option<i64> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(2)).ok()?;
    let (newest, count): (Option<i64>, i64) = conn
        .query_row(
            "SELECT MAX(time_updated), COUNT(*) FROM part WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    Some(newest?.wrapping_mul(1_000_003).wrapping_add(count))
}

/// The transcript file for a resolved session: the row's own stamp when it
/// carries one, else the harness lookup (claude projects store; codex
/// sessions tree), cached so a sweep lists neither store per row. opencode
/// has none.
fn transcript_for<C: CursorAccess>(
    session: &TruthSession,
    stores: &Stores,
    cursors: &mut C,
) -> Option<PathBuf> {
    if let Some(path) = session.transcript_path.as_ref() {
        return Some(path.clone());
    }
    match session.agent.as_str() {
        "claude" | "codex" if !session.session_id.is_empty() => {
            lookup_path(cursors, stores, &session.agent, &session.session_id)
        }
        _ => None,
    }
}

fn lookup_path<C: CursorAccess>(
    cursors: &mut C,
    stores: &Stores,
    agent: &str,
    session_id: &str,
) -> Option<PathBuf> {
    let key = format!("{agent}:{session_id}");
    if let Some(path) = cursors.with(|c| c.cached_path(&key)) {
        return Some(path);
    }
    let found = cursors.with(|c| match agent {
        "claude" => claude_path_for(session_id, c.claude_hits(&stores.projects_root)),
        "codex" => c
            .codex_files(stores.codex_sessions_dir.as_deref())
            .iter()
            .find_map(|(name, path)| {
                crate::codex_store::codex_rollout_matches(name, session_id).then(|| path.clone())
            }),
        _ => None,
    })?;
    cursors.with(|c| c.cache_path(&key, &found));
    Some(found)
}

/// The mtime / opencode-db fallback leg of the age read, the port of
/// `_transcript_age_s`. Returns `(epoch, basis)`; the caller derives the
/// age from the same epoch, so the pair cannot disagree.
fn age_fallback(session: &TruthSession, path: Option<&Path>) -> (Option<f64>, Option<String>) {
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
                Some(epoch) => (Some(epoch), Some("mtime".to_string())),
                None => (None, None),
            }
        }
        _ => match opencode_activity_epoch(&ambient_opencode_db(), &session.session_id) {
            Some(epoch) => (Some(epoch), Some("opencode-db".to_string())),
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

/// Collapsed whitespace, capped at 200 chars, empty is absent: the
/// `last_message` wire field.
fn flatten_200(text: &str) -> Option<String> {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(200).collect();
    (!capped.is_empty()).then_some(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, agent: &str, path: &Path) -> RegistryEntry {
        let mut row = RegistryEntry::default();
        row.name = name.to_string();
        row.harness = Some(agent.to_string());
        row.harness_session_id = Some(format!("sid-{name}"));
        row.transcript_path = Some(path.to_string_lossy().to_string());
        row
    }

    fn stores(dir: &Path) -> Stores {
        Stores {
            projects_root: dir.join("no-projects"),
            codex_sessions_dir: Some(dir.join("no-codex")),
            opencode_db: dir.join("no-opencode.db"),
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("truth-reader-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const NOW: f64 = 1_791_460_800.0; // 2026-10-08T12:00:00Z

    /// Until the warm pass rebuilds a session, truth answers `warming`, and
    /// the decoded probe is neither live nor dead. After the rebuild the same
    /// handle answers its real state.
    #[test]
    fn an_unrebuilt_session_answers_warming_until_its_cursor_exists() {
        let dir = tmp("warming");
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"assistant","timestamp":"2026-10-08T11:59:00Z","message":{"role":"assistant","model":"claude-opus-5-5","content":"reading"}}"#.to_string() + "\n",
        )
        .unwrap();
        let rows = vec![row("w1", "claude", &path)];
        let stores = stores(&dir);
        let mut cursors = TruthCursors::new();
        let early = resolve_payload_with(
            Some(&rows),
            "w1",
            NOW,
            &stores,
            &mut cursors,
            ReadMode::WarmOnly,
        );
        assert_eq!(early["state"], "warming");
        assert_eq!(early["reachability"], "unknown");
        assert!(warm_row(&rows[0], &stores, &mut cursors));
        let warm = resolve_payload_with(
            Some(&rows),
            "w1",
            NOW,
            &stores,
            &mut cursors,
            ReadMode::WarmOnly,
        );
        assert_eq!(warm["state"], "working");
        assert_eq!(warm["reachability"], "reachable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unresolvable handle costs one bounded resolve per beat: the second
    /// ask inside the beat answers `not-found` from memory and touches no
    /// store.
    #[test]
    fn an_unresolvable_session_costs_one_attempt_per_beat() {
        let dir = tmp("miss");
        let stores = stores(&dir);
        let mut cursors = TruthCursors::new();
        let handle = "e247b6d7";
        let first = resolve_payload(None, handle, NOW, &stores, &mut cursors);
        assert_eq!(first["reason"], "not-found");
        assert!(cursors.recent_miss(handle), "the miss is remembered");
        // A registry row naming the handle still answers inside the beat.
        let path = dir.join("row.jsonl");
        std::fs::write(&path, "").unwrap();
        let rows = vec![row(handle, "claude", &path)];
        let by_row = resolve_payload(Some(&rows), handle, NOW, &stores, &mut cursors);
        assert_eq!(by_row["reason"], "no-records", "{by_row}");
        // A store that now holds the session is not consulted inside the beat.
        let project = stores.projects_root.join("-p");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{handle}-0000-0000-0000-000000000000.jsonl")),
            "",
        )
        .unwrap();
        let second = resolve_payload(None, handle, NOW, &stores, &mut cursors);
        assert_eq!(second["reason"], "not-found");
        cursors.expire_misses_for_test();
        let third = resolve_payload(None, handle, NOW, &stores, &mut cursors);
        assert_ne!(
            third["reason"], "not-found",
            "the next beat resolves again: {third}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn compact_case(agent: &str, before: &str, boundary: &str, after: &str) -> Value {
        let dir = tmp(&format!("compact-{agent}"));
        let path = dir.join("t.jsonl");
        // A long pre-compact history the rebuild must not read.
        let mut body = String::new();
        while body.len() < 40 * 1024 {
            body.push_str(before);
            body.push('\n');
        }
        body.push_str(boundary);
        body.push('\n');
        body.push_str(after);
        body.push('\n');
        std::fs::write(&path, &body).unwrap();
        let rows = vec![row("c1", agent, &path)];
        let stores = stores(&dir);
        let mut cursors = TruthCursors::new();
        let payload = resolve_payload(Some(&rows), "c1", NOW, &stores, &mut cursors);
        assert!(
            cursors.last_bytes_read() < 4 * 1024,
            "{agent}: the rebuild stopped at the boundary ({} bytes)",
            cursors.last_bytes_read()
        );
        let _ = std::fs::remove_dir_all(&dir);
        payload
    }

    /// claude: `{"type":"system","subtype":"compact_boundary"}`. Truth comes
    /// from the turns after it, never from the stale history before it.
    #[test]
    fn a_claude_rebuild_stops_at_the_compact_boundary() {
        let payload = compact_case(
            "claude",
            r#"{"type":"assistant","timestamp":"2026-10-08T09:00:00Z","message":{"role":"assistant","model":"claude-opus-5-5","content":"<promise>OLD</promise>"}}"#,
            r#"{"parentUuid":null,"isSidechain":false,"type":"system","subtype":"compact_boundary","content":"Conversation compacted","timestamp":"2026-10-08T11:58:00Z"}"#,
            r#"{"type":"assistant","timestamp":"2026-10-08T11:59:00Z","message":{"role":"assistant","model":"claude-opus-5-5","content":"Continuing the port."}}"#,
        );
        assert_eq!(payload["state"], "working", "{payload}");
        assert_eq!(payload["observed_model"]["model"], "claude-opus-5-5");
    }

    /// codex: a top-level `"type":"compacted"` record.
    #[test]
    fn a_codex_rebuild_stops_at_the_compacted_record() {
        let payload = compact_case(
            "codex",
            r#"{"timestamp":"2026-10-08T09:00:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"<promise>OLD</promise>"}]}}"#,
            r#"{"timestamp":"2026-10-08T11:58:00Z","ordinal":9,"type":"compacted","payload":{"message":"","replacement_history":[]}}"#,
            r#"{"timestamp":"2026-10-08T11:59:00Z","type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#,
        );
        // The turn after the boundary carries no message yet: no records.
        assert_eq!(payload["state"], "unknown", "{payload}");
        assert_eq!(payload["reason"], "no-records");
        assert_eq!(payload["observed_model"]["model"], "gpt-6.1-sol");
    }

    /// opencode: a `{"type":"compaction"}` part ends the read; older turns
    /// do not reach truth.
    #[test]
    fn an_opencode_read_stops_at_the_compaction_part() {
        let dir = tmp("compact-opencode");
        let db = dir.join("opencode.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO message VALUES ('m1','ses1',1,1,'{\"role\":\"assistant\"}');
             INSERT INTO part VALUES ('p1','m1','ses1',1,1,'{\"type\":\"text\",\"text\":\"<promise>OLD</promise>\"}');
             INSERT INTO message VALUES ('m2','ses1',2,2,'{\"role\":\"user\"}');
             INSERT INTO part VALUES ('p2','m2','ses1',2,2,'{\"type\":\"compaction\",\"auto\":true}');",
        )
        .unwrap();
        drop(conn);
        let mut row = RegistryEntry::default();
        row.name = "o1".into();
        row.harness = Some("opencode".into());
        row.harness_session_id = Some("ses1".into());
        let rows = vec![row];
        let mut st = stores(&dir);
        st.opencode_db = db;
        let mut cursors = TruthCursors::new();
        let payload = resolve_payload(Some(&rows), "o1", NOW, &st, &mut cursors);
        assert_eq!(
            payload["reason"], "no-records",
            "the pre-compact promise is not read: {payload}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
