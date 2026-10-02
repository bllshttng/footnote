//! The worked authority behind `fno backlog worked`: which nodes have
//! positively identified live workers. The python twin is `graph/worked.py`
//! over `statuses.live_worked_node_ids` (retiring change: the next/undispatched
//! doors); this leg mirrors it decision-for-decision.
//!
//! Divergences, named: the fleet read reuses the native claude roster spawn
//! (15s budget, the snapshot reader's own bound) and skips the headroom
//! latency warning (advisory-classified, never observable in this reply);
//! the transcript listing is built once per read rather than through the
//! python listing scope.

use crate::claude_roster::RawAgents;
use crate::claude_transcript_paths::{choose_from, store_listing, Hit};
use crate::state::RegistryEntry;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ADVISORY: &str = "roster advisory: ";
const UNMEASURABLE_ROW_PREFIX: &str = "unmeasurable-row: ";
const REGISTRY_ONLY_MARK: &str = "falling back to registry-only view";
const TRANSCRIPT_EVIDENCE_S: i64 = 20 * 60;
const STALLED_AFTER_S: i64 = 2 * 3600;
const TICK_TAIL_BYTES: u64 = 256 * 1024;
const MAX_PID: i64 = 0x7FFF_FFFF;

const LIVE_INPUT: &[(&str, &str)] = &[
    ("working", "working"),
    ("busy", "working"),
    ("blocked", "blocked"),
    ("needs input", "blocked"),
    ("idle", "idle"),
    ("ready", "idle"),
    ("done", "done"),
    ("failed", "done"),
    ("stopped", "done"),
    ("live", "working"),
    ("spawning", "working"),
    ("restarting", "working"),
];

const TERMINAL_STATES: &[&str] = &["stopped", "done", "completed", "exited", "killed"];
const FINISHED_STATES: &[&str] = &["done", "completed", "exited", "killed"];
const ACTIVE_STATES: &[&str] = &["working", "watching", "your-move"];

const USAGE: &str = "usage: fno-agents backlog worked [--json]";

// ---------------------------------------------------------------------------
// One fleet row
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FleetRow {
    row_id: String,
    name: String,
    state: String,
    node: Option<String>,
    cwd: String,
    agent: String,
    stopped_at: Option<String>,
    pid: Option<u32>,
    pid_start_time: Option<u64>,
    mux: Option<crate::state::MuxRef>,
}

fn status_word(status: crate::AgentStatus) -> &'static str {
    match status {
        crate::AgentStatus::Spawning => "spawning",
        crate::AgentStatus::Ready => "ready",
        crate::AgentStatus::Idle => "idle",
        crate::AgentStatus::Busy => "busy",
        crate::AgentStatus::Live => "live",
        crate::AgentStatus::Restarting => "restarting",
        crate::AgentStatus::Orphaned => "orphaned",
        crate::AgentStatus::Failed => "failed",
        crate::AgentStatus::Exited => "exited",
        crate::AgentStatus::PermanentDead => "permanent_dead",
    }
}

fn contains(list: &[&str], value: &str) -> bool {
    list.contains(&value)
}

// ---------------------------------------------------------------------------
// Row state fold (`_row_state`)
// ---------------------------------------------------------------------------

/// The canonical row state plus a warning when the spelling is new. Reads
/// ("state", "status") in that order, folds through the harness map, and
/// returns an unknown spelling as-is WITH a warning.
fn row_state(row: &Value) -> (String, Option<String>) {
    for key in ["state", "status"] {
        let Some(raw) = row.get(key).and_then(Value::as_str) else {
            continue;
        };
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() {
            continue;
        }
        for (input, mapped) in LIVE_INPUT {
            if *input == raw {
                return (mapped.to_string(), None);
            }
        }
        if contains(TERMINAL_STATES, &raw) {
            return (raw, None);
        }
        return (
            raw.clone(),
            Some(format!(
                "{ADVISORY}unmapped row state '{raw}', classified by name only"
            )),
        );
    }
    (
        String::new(),
        Some(format!(
            "{ADVISORY}row carried no state under either alias, unmeasurable"
        )),
    )
}

fn is_linked_worktree(cwd: &str) -> bool {
    !cwd.is_empty() && Path::new(cwd).join(".git").is_file()
}

fn node_id_from_worktree(cwd: &str) -> Option<String> {
    let text = std::fs::read_to_string(Path::new(cwd).join(".fno").join("target-state.md")).ok()?;
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("graph_node_id:") {
            let val = rest.trim().trim_matches('"').trim_matches('\'').trim();
            return normalize_node_id(val);
        }
    }
    None
}

fn normalize_node_id(value: &str) -> Option<String> {
    let normalized = value.trim().trim_matches('"').trim_matches('\'').trim();
    match normalized.to_ascii_lowercase().as_str() {
        "" | "null" | "none" | "nil" => None,
        _ => Some(normalized.to_string()),
    }
}

// ---------------------------------------------------------------------------
// The fleet fold (`fleet_rows`)
// ---------------------------------------------------------------------------

/// Every live registry status (`spawn_gate.LIVE_STATUSES`).
fn is_live_status(status: crate::AgentStatus) -> bool {
    matches!(
        status,
        crate::AgentStatus::Spawning
            | crate::AgentStatus::Ready
            | crate::AgentStatus::Idle
            | crate::AgentStatus::Busy
            | crate::AgentStatus::Live
            | crate::AgentStatus::Restarting
    )
}

fn fleet_rows() -> (Vec<FleetRow>, Vec<String>) {
    let raw = crate::claude_roster::read_all_agents_raw();
    let registry =
        crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
            .ok()
            .map(|r| r.entries);
    fleet_rows_from(raw, registry.as_deref())
}

/// The fleet fold, fno-first (the provenance ruling): the spine is fno's
/// registry - every claude row it holds is listed, the vendor row joining by
/// session id as a state column - and a vendor row with no spine row is
/// named vendor-only, never dropped and never silent. An unread registry
/// falls back to the vendor view; an unread VENDOR list only warns, and the
/// fno rows stand.
fn fleet_rows_from(
    raw: RawAgents,
    entries: Option<&[RegistryEntry]>,
) -> (Vec<FleetRow>, Vec<String>) {
    let mut warnings = raw.warnings;
    let mut ledger_nodes: Option<BTreeMap<String, String>> = None;
    let mut out: Vec<FleetRow> = Vec::new();
    let mut unmapped: BTreeSet<String> = BTreeSet::new();
    let mut skipped_no_sid = 0usize;
    let mut skipped_nonclaude_no_id = 0usize;
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // The spine: every claude registry row with a session id. It is listed
    // whether or not the vendor still lists it.
    let mut spine: BTreeMap<&str, &RegistryEntry> = BTreeMap::new();
    if let Some(entries) = entries {
        for entry in entries {
            if entry.harness.as_deref() != Some("claude") {
                continue;
            }
            match claude_spine_sid(entry) {
                Some(sid) => {
                    spine.insert(sid, entry);
                }
                None => warnings.push(format!(
                    "{ADVISORY}{UNMEASURABLE_ROW_PREFIX}harness=claude name={} (no session id)",
                    entry.name
                )),
            }
        }
    }
    // One pass over the vendor rows: sid -> row, so the spine join is a
    // map lookup, not a rescan per spine entry.
    let mut vendor_by_sid: BTreeMap<String, &Value> = BTreeMap::new();
    for r in &raw.rows {
        if let Some(sid) = raw_row_sid(r) {
            vendor_by_sid.entry(sid).or_insert(r);
        }
    }
    for (sid, e) in spine.iter() {
        let vendor = vendor_by_sid.get(*sid).copied();
        let (state, warn) = match vendor {
            Some(r) => row_state(r),
            None => row_state(&json!({"status": status_word(e.status)})),
        };
        if let Some(warn) = warn {
            unmapped.insert(warn);
        }

        let cwd = vendor
            .and_then(|r| r.get("cwd").and_then(Value::as_str))
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .or_else(|| (!e.cwd.is_empty()).then(|| e.cwd.clone()))
            .unwrap_or_default();
        let mut node = e.node.clone().filter(|n| !n.is_empty());
        if node.is_none() && is_linked_worktree(&cwd) {
            node = node_id_from_worktree(&cwd);
        }
        if node.is_none() {
            if ledger_nodes.is_none() {
                ledger_nodes = Some(ledger_nodes_map());
            }
            node = ledger_nodes.as_ref().and_then(|l| l.get(*sid).cloned());
        }
        out.push(FleetRow {
            row_id: (*sid).to_string(),
            name: if e.name.is_empty() {
                (*sid).to_string()
            } else {
                e.name.clone()
            },
            state,
            node,
            cwd,
            agent: "claude".to_string(),
            stopped_at: e
                .stop
                .as_ref()
                .and_then(|s| (!s.at.is_empty()).then(|| s.at.clone())),
            pid: None,
            pid_start_time: None,
            mux: None,
        });
    }

    // Vendor rows the spine did not claim: listed, never dropped, and named
    // vendor-only so the registry gap is loud. When the registry itself is
    // unreadable (entries None) the vendor view IS the read, so no warning.
    for r in &raw.rows {
        let Some(sid) = raw_row_sid(r) else {
            let cwd = r.get("cwd").and_then(Value::as_str).unwrap_or("");
            let name = r.get("name").and_then(Value::as_str).unwrap_or("unknown");
            let node = if is_linked_worktree(cwd) {
                node_id_from_worktree(cwd)
            } else {
                None
            };
            match node {
                Some(node) => warnings.push(format!(
                    "{ADVISORY}{UNMEASURABLE_ROW_PREFIX}harness=claude node={node} name={name}"
                )),
                None => skipped_no_sid += 1,
            }
            continue;
        };
        if spine.contains_key(sid.as_str()) {
            continue;
        }
        if entries.is_some() {
            warnings.push(format!(
                "{ADVISORY}vendor-only row: session {sid} is listed by claude agents but absent from the fno registry"
            ));
        }
        let (state, warn) = row_state(r);
        if let Some(warn) = warn {
            unmapped.insert(warn);
        }

        let name = r
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&sid)
            .to_string();
        let cwd = r
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .unwrap_or_default();
        let mut node = None;
        if is_linked_worktree(&cwd) {
            node = node_id_from_worktree(&cwd);
        }
        if node.is_none() {
            if ledger_nodes.is_none() {
                ledger_nodes = Some(ledger_nodes_map());
            }
            node = ledger_nodes.as_ref().and_then(|l| l.get(&sid).cloned());
        }
        out.push(FleetRow {
            row_id: sid,
            name,
            state,
            node,
            cwd,
            agent: "claude".to_string(),
            stopped_at: None,
            pid: None,
            pid_start_time: None,
            mux: None,
        });
    }
    for entry in entries.into_iter().flatten() {
        if entry.harness.as_deref() == Some("claude") {
            continue;
        }
        if !is_live_status(entry.status) {
            continue;
        }
        let row_id: String = entry
            .harness_session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| entry.session_id.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| (!entry.short_id.is_empty()).then_some(entry.short_id.as_str()))
            .unwrap_or("")
            .to_string();
        let name = if entry.name.is_empty() {
            "unknown".to_string()
        } else {
            entry.name.clone()
        };
        if row_id.is_empty() {
            match entry.node.as_deref().filter(|n| !n.is_empty()) {
                Some(node) => warnings.push(format!(
                    "{ADVISORY}{UNMEASURABLE_ROW_PREFIX}harness={} node={node} name={name}",
                    entry.harness.as_deref().unwrap_or("unknown"),
                )),
                None => skipped_nonclaude_no_id += 1,
            }
            continue;
        }
        if !seen.insert(row_id.clone()) {
            continue;
        }
        let (state, warn) = row_state(&json!({
            "status": status_word(entry.status),
        }));
        if let Some(warn) = warn {
            unmapped.insert(warn);
        }
        out.push(FleetRow {
            row_id: row_id.clone(),
            name: if name.is_empty() {
                row_id.clone()
            } else {
                name
            },
            state,
            node: entry.node.clone().filter(|n| !n.is_empty()),
            cwd: entry.cwd.clone(),
            agent: entry
                .harness
                .clone()
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "claude".to_string()),
            stopped_at: None,
            pid: entry.pid,
            pid_start_time: entry.pid_start_time,
            mux: entry.mux.clone(),
        });
    }

    if skipped_no_sid > 0 {
        warnings.push(format!(
            "{skipped_no_sid} row(s) carried no session id, unmeasurable, skipped"
        ));
    }
    if skipped_nonclaude_no_id > 0 {
        warnings.push(format!(
            "{skipped_nonclaude_no_id} non-claude row(s) carried no session id, unmeasurable, skipped"
        ));
    }
    warnings.extend(unmapped);
    (out, warnings)
}

/// The spine key: the row's harness session id, else its recorded session id.
fn claude_spine_sid(e: &RegistryEntry) -> Option<&str> {
    e.harness_session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| e.cc_session_id.as_deref().filter(|s| !s.is_empty()))
}

/// The vendor row's session id, either spelling, when nonempty.
fn raw_row_sid(r: &Value) -> Option<String> {
    r.get("sessionId")
        .or_else(|| r.get("session_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn ledger_nodes_map() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(crate::paths::ledger_path(
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    )) else {
        return out;
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    let Some(entries) = data.get("entries").and_then(Value::as_array) else {
        return out;
    };
    for e in entries {
        let Some(node) = e
            .get("graph_node_id")
            .and_then(Value::as_str)
            .and_then(normalize_node_id)
        else {
            continue;
        };
        // `sessions or session_id or []`: an EMPTY sessions list falls
        // through to the scalar spelling, exactly as python's `or` reads it.
        let raw = match e.get("sessions") {
            Some(Value::Array(rows)) if !rows.is_empty() => Value::Array(rows.clone()),
            _ => e
                .get("session_id")
                .cloned()
                .unwrap_or(Value::Array(Vec::new())),
        };
        for sid in raw.as_str().into_iter().chain(
            raw.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        ) {
            if !sid.is_empty() {
                out.insert(sid.to_string(), node.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The roster reading (`read_roster`, require_live_probe=False)
// ---------------------------------------------------------------------------

struct RosterReading {
    consulted: bool,
    reason: String,
    workers_by_node: BTreeMap<String, Vec<FleetRow>>,
    rows_by_session: BTreeMap<String, FleetRow>,
    unmeasurable_by_node: BTreeMap<String, Vec<String>>,
}

fn read_roster() -> RosterReading {
    let (rows, warnings) = fleet_rows();
    let mut blocking: Vec<String> = Vec::new();
    let mut degraded_reason = String::new();
    let mut unmeasurable: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for w in &warnings {
        let Some(idx) = w.find(UNMEASURABLE_ROW_PREFIX) else {
            let degraded_probe = w.contains(REGISTRY_ONLY_MARK);
            if !w.starts_with(ADVISORY) && !degraded_probe {
                blocking.push(w.clone());
            } else if degraded_probe && degraded_reason.is_empty() {
                degraded_reason = w.clone();
            }
            continue;
        };
        let mut node = None;
        let mut name = None;
        for tok in w[idx + UNMEASURABLE_ROW_PREFIX.len()..].split_whitespace() {
            if let Some((k, v)) = tok.split_once('=') {
                match k {
                    "node" => node = Some(v.to_string()),
                    "name" => name = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        if let Some(node) = node {
            unmeasurable.entry(node).or_default().push(
                name.filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "unknown".to_string()),
            );
        }
    }
    if !blocking.is_empty() {
        return RosterReading {
            consulted: false,
            reason: blocking.remove(0),
            workers_by_node: BTreeMap::new(),
            rows_by_session: BTreeMap::new(),
            unmeasurable_by_node: BTreeMap::new(),
        };
    }
    let mut workers_by_node: BTreeMap<String, Vec<FleetRow>> = BTreeMap::new();
    let mut rows_by_session: BTreeMap<String, FleetRow> = BTreeMap::new();
    for r in rows {
        if let Some(node) = &r.node {
            workers_by_node
                .entry(node.clone())
                .or_default()
                .push(r.clone());
        }
        rows_by_session.insert(r.row_id.clone(), r);
    }
    let _ = degraded_reason;
    RosterReading {
        consulted: true,
        reason: String::new(),
        workers_by_node,
        rows_by_session,
        unmeasurable_by_node: unmeasurable,
    }
}

// ---------------------------------------------------------------------------
// Transcript tails
// ---------------------------------------------------------------------------

struct TailFacts {
    last_epoch: Option<f64>,
    last_role: Option<String>,
    last_text: String,
}

fn now_epoch_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn claude_projects_root() -> PathBuf {
    crate::claude_roster::config_dir().join("projects")
}

/// Resolve one harness session's transcript file. claude walks the projects
/// store with the shared listing; codex finds its rollout; every other
/// harness keeps no per-session file.
fn resolve_transcript(
    agent: &str,
    session_id: &str,
    cwd: &str,
    listing: &[Hit],
) -> Option<PathBuf> {
    match agent {
        "claude" => {
            if session_id.is_empty() || cwd.is_empty() {
                return None;
            }
            let root = claude_projects_root();
            choose_from(listing, session_id)
                .or_else(|| choose_from(&store_listing(&root), session_id))
        }
        "codex" => crate::codex_store::codex_rollout_path(None, session_id),
        _ => None,
    }
}

fn record_epoch(record: &Value) -> Option<f64> {
    let ts = record.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9)
}

/// Tail-read one transcript and derive the classifier's input: the newest
/// record epoch, the LAST role-bearing record's role and flattened text.
/// `None` means the transcript could not be resolved, read, or decoded.
fn tail_facts(session_id: &str, cwd: &str, agent: &str, listing: &[Hit]) -> Option<TailFacts> {
    let path = resolve_transcript(agent, session_id, cwd, listing)?;
    let size = std::fs::metadata(&path).ok()?.len();
    // Read only the trailing window: transcripts grow without bound and the
    // classifier asks about the last turn, so the whole file is never wanted.
    let start = size.saturating_sub(TICK_TAIL_BYTES);
    let mut file = std::fs::File::open(&path).ok()?;
    use std::io::{Read as _, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TICK_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let start = if start > 0 {
        // A mid-file seek lands inside a line; drop the partial head.
        let mut s = 0usize;
        while s < bytes.len() && bytes[s] != b'\n' {
            s += 1;
        }
        s + 1
    } else {
        0
    };
    let text = String::from_utf8_lossy(&bytes[start..]);
    let mut last_epoch: Option<f64> = None;
    let mut last_role: Option<String> = None;
    let mut last_text = String::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let record = normalize_record(record);
        if let Some(epoch) = record_epoch(&record) {
            last_epoch = Some(epoch);
        }
        let role = record
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str);
        if let Some(role) = role {
            last_role = Some(role.to_string());
            last_text = record_text(&record);
        }
    }
    // A resolved-but-empty or stamp-free tail stays Some: the classifier's
    // `last_epoch is None` arm owns that fact (unknown, never no-transcript).
    Some(TailFacts {
        last_epoch,
        last_role,
        last_text,
    })
}

/// Codex response items fold onto the message shape the reader speaks.
fn normalize_record(record: Value) -> Value {
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return record;
    }
    let payload = record.get("payload");
    let is_message = payload.and_then(|p| p.get("type")).and_then(Value::as_str) == Some("message");
    if !is_message {
        return record;
    }
    let mut out = record.clone();
    if let Some(obj) = out.as_object_mut() {
        if let Some(payload) = payload {
            obj.insert("message".to_string(), payload.clone());
        }
    }
    out
}

/// Flattened text of one record: message bodies and top-level system text.
fn record_text(record: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let msg = record.get("message");
    let content = msg.and_then(|m| m.get("content"));
    match content {
        Some(Value::String(text)) => parts.push(text.clone()),
        Some(Value::Array(items)) => {
            for p in items {
                if p.get("type").and_then(Value::as_str) == Some("text")
                    || p.get("type").and_then(Value::as_str) == Some("input_text")
                    || p.get("type").and_then(Value::as_str) == Some("output_text")
                {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        if let Some(t) = record.get("text").and_then(Value::as_str) {
            parts.push(t.to_string());
        }
    }
    parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// The tail classifier (`classify_tail`)
// ---------------------------------------------------------------------------

fn watching_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<watching[>\s]").unwrap())
}

fn promise_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<promise[>\s]").unwrap())
}

fn help_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<help[>\s]").unwrap())
}

fn api_error_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^API Error\b").unwrap())
}

fn option_prompt_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[\[(](?:[Yy]/[Nn]|\d+(?:/\d+)+)[\])]\s*$").unwrap())
}

/// Pure classifier over the LAST transcript turn. Content signals apply only
/// when the last turn is the assistant's; a trailing user turn clears any
/// stale assistant signal and mtime decides. A turn stops being news past
/// `stalled_after_s`; `done` is an outcome and does not go stale.
fn classify_tail(last_role: Option<&str>, last_text: &str, age_s: Option<f64>) -> String {
    let text = last_text;
    let stale = age_s.is_some_and(|a| a > STALLED_AFTER_S as f64);
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
// Reachability
// ---------------------------------------------------------------------------

struct Reach {
    verdict: &'static str,
}

const REACHABLE: &str = "reachable";
const UNKNOWN: &str = "unknown";

fn classify_reachability(
    truth_state: Option<&str>,
    age_s: Option<f64>,
    falsifier: Option<String>,
) -> Reach {
    if falsifier.is_some() {
        return Reach {
            verdict: "unreachable",
        };
    }
    if truth_state.is_some_and(|s| contains(ACTIVE_STATES, s)) {
        if age_s.is_some_and(|a| a > TRANSCRIPT_EVIDENCE_S as f64) {
            return Reach { verdict: UNKNOWN };
        }
        return Reach { verdict: REACHABLE };
    }
    match truth_state {
        None | Some("unknown") => Reach { verdict: UNKNOWN },
        _ => Reach { verdict: UNKNOWN },
    }
}

fn pid_falsifier(pid: Option<u32>, recorded_start: Option<u64>) -> Option<String> {
    let pid = pid?;
    if pid <= 1 || pid as i64 > MAX_PID {
        return Some("process-gone".into());
    }
    // A zombie answers kill(pid,0) with success; the probe reads it gone.
    match crate::claims::probe_pid(pid as i32) {
        crate::claims::PidProbe::Absent => return Some("process-gone".into()),
        crate::claims::PidProbe::Refused => return None,
        crate::claims::PidProbe::Created(_) => {}
    }
    match recorded_start {
        None => None,
        Some(start) => match crate::daemon::process_start_time(pid) {
            None => None,
            Some(current) => (current != start).then(|| "process-gone".to_string()),
        },
    }
}

fn pane_falsifier(mux: Option<&crate::state::MuxRef>) -> Option<String> {
    let mux = mux?;
    match crate::spawn_gate_lanes::mux_pane_alive(&mux.session, mux.pane_id) {
        Ok(false) => Some("pane-gone".into()),
        Ok(true) | Err(()) => None,
    }
}

fn parse_stop_epoch(stamp: Option<&str>) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(&stamp?.replace('Z', "+00:00"))
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

/// One roster row through the shared reachability predicate: REACHABLE
/// engaged, UNREACHABLE falsified, UNKNOWN its own arm. The transcript
/// outranks the supervisor word for EVERY row; a terminal word with no
/// transcript stays positive evidence of the end.
fn worker_reachability(row: &FleetRow, listing: &[Hit]) -> Reach {
    let facts = tail_facts(&row.row_id, &row.cwd, &row.agent, listing);
    let proven = row.pid.is_some() && row.pid_start_time.is_some();
    let mut falsifier = if proven {
        pid_falsifier(row.pid, row.pid_start_time)
    } else {
        None
    };
    if falsifier.is_none() {
        falsifier = pane_falsifier(row.mux.as_ref());
    }
    let stop_epoch = parse_stop_epoch(row.stopped_at.as_deref());
    let Some(facts) = facts else {
        if let Some(falsifier) = falsifier {
            return classify_reachability(None, None, Some(falsifier));
        }
        if contains(ACTIVE_STATES, &row.state) {
            return classify_reachability(Some(&row.state), None, None);
        }
        let finished =
            contains(FINISHED_STATES, &row.state).then(|| format!("finished-state:{}", row.state));
        return classify_reachability(None, None, finished);
    };
    let Some(epoch) = facts.last_epoch else {
        // Transcript present but undatable: UNKNOWN about the instrument,
        // never engaged-by-default and never positively finished.
        return classify_reachability(None, None, falsifier);
    };
    let age = (now_epoch_s() - epoch).max(0.0);
    if falsifier.is_some() && age <= TRANSCRIPT_EVIDENCE_S as f64 {
        falsifier = None;
    }
    if let Some(stop) = stop_epoch {
        if epoch <= stop {
            falsifier = Some(format!(
                "stopped:{}",
                row.stopped_at.clone().unwrap_or_default()
            ));
        }
    }
    if falsifier.is_none()
        && age > TRANSCRIPT_EVIDENCE_S as f64
        && contains(FINISHED_STATES, &row.state)
    {
        falsifier = Some(format!("finished-state:{}", row.state));
    }
    let truth = classify_tail(facts.last_role.as_deref(), &facts.last_text, Some(age));
    classify_reachability(Some(&truth), Some(age), falsifier)
}

// ---------------------------------------------------------------------------
// The worked overlay (`live_worked_node_ids`, strict)
// ---------------------------------------------------------------------------

const UNMEASURABLE_LABEL_MARK: &str = "(unmeasurable:";

/// One graph session row: a valid, unfinished window of `phase`.
fn is_open_phase_row(row: &Value, phase: &str) -> bool {
    row.get("phase").and_then(Value::as_str) == Some(phase)
        && row
            .get("harness")
            .and_then(Value::as_str)
            .is_some_and(|h| !h.trim().is_empty())
        && row
            .get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        && row
            .get("started_at")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        && row.get("ended_at").map(Value::is_null).unwrap_or(true)
}

/// Session ids whose own phase row closed and none is open: finished with
/// THIS node ahead of the predicate. A ship row is a link event, never a
/// phase that closes.
fn closed_worker_session_ids(entry: &Value) -> BTreeSet<String> {
    let mut closed: BTreeSet<String> = BTreeSet::new();
    let mut open: BTreeSet<String> = BTreeSet::new();
    for row in entry
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(sid), Some(phase)) = (
            row.get("session_id").and_then(Value::as_str),
            row.get("phase").and_then(Value::as_str),
        ) else {
            continue;
        };
        if phase == "ship" {
            continue;
        }
        if is_open_phase_row(row, phase) {
            open.insert(sid.to_string());
        } else {
            closed.insert(sid.to_string());
        }
    }
    closed.difference(&open).cloned().collect()
}

/// The fold's terminal read: the bare rung, exactly `TERMINAL_RUNGS` - the
/// overlay never re-derives closure beyond the stored status.
fn terminal_entry(entry: &Value) -> bool {
    matches!(
        entry.get("status").and_then(Value::as_str),
        Some("done") | Some("superseded")
    )
}

fn admit(workers: &mut Vec<String>, name: &str, verdict: &str) {
    let label = if verdict == REACHABLE {
        name.to_string()
    } else {
        format!("{name} {UNMEASURABLE_LABEL_MARK} no positive liveness evidence)")
    };
    if !label.is_empty() && !workers.contains(&label) {
        workers.push(label);
    }
}

/// Open-phase nodes whose roster workers are live: the strict fold. An
/// unreadable roster refuses (the caller renders the named refusal); the
/// transcript listing is built once and shared across every resolution.
pub(crate) fn live_worked_node_ids(
    entries: &[Value],
) -> Result<Vec<(String, Vec<String>)>, String> {
    if !entries.iter().any(|e| !terminal_entry(e)) {
        return Ok(Vec::new());
    }
    let reading = read_roster();
    if !reading.consulted {
        return Err(reading.reason);
    }
    let listing = store_listing(&claude_projects_root());
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for entry in entries {
        if terminal_entry(entry) {
            continue;
        }
        let Some(node_id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mut workers: Vec<String> = Vec::new();
        let closed_ids = closed_worker_session_ids(entry);
        for row in entry
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(sid), Some(phase)) = (
                row.get("session_id").and_then(Value::as_str),
                row.get("phase").and_then(Value::as_str),
            ) else {
                continue;
            };
            if phase == "ship" || !is_open_phase_row(row, phase) {
                continue;
            }
            let Some(roster_row) = reading.rows_by_session.get(sid) else {
                continue;
            };
            if closed_ids.contains(&roster_row.row_id) {
                continue;
            }
            let verdict = worker_reachability(roster_row, &listing);
            if verdict.verdict == REACHABLE || verdict.verdict == UNKNOWN {
                admit(&mut workers, &roster_row.name, verdict.verdict);
            }
        }
        for extra in reading.workers_by_node.get(node_id).into_iter().flatten() {
            if closed_ids.contains(&extra.row_id) {
                continue;
            }
            let verdict = worker_reachability(extra, &listing);
            if verdict.verdict == REACHABLE || verdict.verdict == UNKNOWN {
                admit(&mut workers, &extra.name, verdict.verdict);
            }
        }

        for extra_name in reading
            .unmeasurable_by_node
            .get(node_id)
            .into_iter()
            .flatten()
        {
            let marker = format!("{extra_name} {UNMEASURABLE_LABEL_MARK} no harness session id)");
            if !workers.contains(&marker) {
                workers.push(marker);
            }
        }
        if !workers.is_empty() {
            out.push((node_id.to_string(), workers));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The verb
// ---------------------------------------------------------------------------

/// The strict authority both spellings serve: the graph rows and the
/// worked fold over them. Strict, like the python twin's
/// read_graph_strict: a corrupt graph is an unreadable authority, never
/// an empty fleet.
fn authority() -> Result<(Vec<Value>, Vec<(String, Vec<String>)>), String> {
    let graph = super::settings::graph_path();
    let entries = crate::graph_store::read_rows_strict(&graph)
        .map_err(|_| "the graph is unreadable".to_string())?;
    let worked = live_worked_node_ids(&entries)?;
    Ok((entries, worked))
}

/// The `--json` rows, in-process: the same payload `run --json` prints,
/// without a process. The king board reads this directly because the Python
/// worked leg is a refusing tombstone.
pub(crate) fn json_rows() -> Result<Vec<Value>, String> {
    let (entries, worked) = authority()?;
    let by_id: BTreeMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();
    Ok(worked
        .iter()
        .map(|(id, workers)| {
            let entry = by_id.get(id.as_str()).copied();
            let mut phases: Vec<&str> = Vec::new();
            if let Some(sessions) = entry
                .and_then(|e| e.get("sessions"))
                .and_then(Value::as_array)
            {
                for row in sessions {
                    let Some(phase) = row.get("phase").and_then(Value::as_str) else {
                        continue;
                    };
                    if is_open_phase_row(row, phase) && !phases.contains(&phase) {
                        phases.push(phase);
                    }
                }
            }
            json!({
                "id": id,
                "status": entry
                    .and_then(|e| e.get("status").and_then(Value::as_str))
                    .unwrap_or("unknown"),
                "workers": workers,
                "phases": phases,
            })
        })
        .collect())
}

pub fn run(args: &[String]) -> i32 {
    let mut json_output = false;
    for arg in args {
        match arg.as_str() {
            "--json" | "-J" => json_output = true,
            "-h" | "--help" => {
                println!("Show nodes with positively identified live workers.\n\n{USAGE}");
                return 0;
            }
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    if json_output {
        return match json_rows() {
            Ok(rows) => {
                println!(
                    "{}",
                    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
                );
                0
            }
            Err(reason) => {
                eprintln!("Error: worked authority unavailable: {reason}");
                1
            }
        };
    }
    let (entries, worked) = match authority() {
        Ok(pair) => pair,
        Err(reason) => {
            eprintln!("Error: worked authority unavailable: {reason}");
            return 1;
        }
    };
    let by_id: BTreeMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();

    for (id, workers) in &worked {
        println!(
            "{id}  {}  {}",
            worked_status(&by_id, id),
            workers.join(", ")
        );
    }
    0
}

fn worked_status<'a>(by_id: &BTreeMap<&str, &'a Value>, id: &str) -> &'a str {
    by_id
        .get(id)
        .and_then(|e| e.get("status").and_then(Value::as_str))
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fleet fold starts from fno's registry: a live claude spine row
    // lists with an EMPTY vendor view (AC11), and a vendor row with no
    // spine row is named vendor-only, never dropped silently (AC12).
    #[test]
    fn fleet_rows_start_from_the_registry_and_name_vendor_only_rows() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spaces_backup = std::env::var_os("FNO_SPACES_DIR");
        std::env::set_var(
            "FNO_SPACES_DIR",
            std::env::temp_dir().join("worked-fleet-hermetic"),
        );
        let mut live = crate::state::RegistryEntry::default();
        live.harness = Some("claude".into());
        live.name = "quill".into();
        live.harness_session_id = Some("9a1b2c3d-0000-0000-0000-000000000000".into());
        live.status = crate::AgentStatus::Live;
        let empty_vendor = RawAgents {
            rows: Vec::new(),
            warnings: Vec::new(),
        };
        let (rows, warnings) = fleet_rows_from(empty_vendor, Some(&[live.clone()]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row_id, "9a1b2c3d-0000-0000-0000-000000000000");
        assert_eq!(rows[0].name, "quill");
        assert_eq!(rows[0].state, "working");
        assert!(warnings.is_empty(), "{warnings:?}");

        // A vendor row with no spine row is listed with a vendor-only
        // warning; the spine row stays.
        let stray_vendor = RawAgents {
            rows: vec![serde_json::json!({
                "sessionId": "0badf00d-0000-0000-0000-000000000000",
                "name": "stray",
                "state": "working"
            })],
            warnings: Vec::new(),
        };
        let (rows, warnings) = fleet_rows_from(stray_vendor, Some(&[live]));
        assert_eq!(rows.len(), 2);
        let stray = rows
            .iter()
            .find(|r| r.row_id == "0badf00d-0000-0000-0000-000000000000")
            .expect("stray vendor row listed");
        assert_eq!(stray.name, "stray");
        assert_eq!(stray.state, "working");
        assert!(
            warnings
                .iter()
                .any(|w| { w.contains("vendor-only") && w.contains("0badf00d") }),
            "{warnings:?}"
        );

        // An unread registry falls back to the vendor view: the row lists
        // with no vendor-only warning, because the vendor IS the read.
        let again = RawAgents {
            rows: vec![serde_json::json!({
                "sessionId": "0badf00d-0000-0000-0000-000000000000",
                "name": "stray",
                "state": "working"
            })],
            warnings: Vec::new(),
        };
        let (rows, warnings) = fleet_rows_from(again, None);
        assert_eq!(rows.len(), 1);
        assert!(!warnings.iter().any(|w| w.contains("vendor-only")));
        match spaces_backup {
            Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
            None => std::env::remove_var("FNO_SPACES_DIR"),
        }
    }
}
