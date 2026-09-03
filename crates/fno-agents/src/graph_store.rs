//! The graph store, ported from `cli/src/fno/graph/store.py` under the
//! four-step protocol in `docs/architecture/dual-implementation-inventory.md`.
//!
//! This is the ported half of the graph store: byte-compatible JSON I/O, the
//! defaults/migration pipeline every read applies, `recompute_statuses`, the
//! canonical key ordering, slug assignment, a bounded advisory lock, and the
//! atomic publish with its backup + SHA256 sidecar. The Python module that
//! mirrors it becomes the RPC client in
//! `crates/fno-agents/src/graph_keeper.rs`'s protocol; the store logic lives
//! HERE and only here.
//!
//! Two invariants are why this is a port and not a rewrite, and both are
//! expressed in types rather than comments:
//!
//! - A field update carries presence ([`FieldUpdate`]). Replacing a populated
//!   text field with an empty value is a distinct operation from leaving it
//!   unchanged, and the empty overwrite is not constructible from the update
//!   path: [`TextField::parse`] refuses empty and whitespace-only values, so
//!   the `fno backlog update <id> --details ""` wipe measured on 2026-09-02
//!   (3,036 characters lost to a failed shell substitution) is unrepresentable.
//! - The lock is bounded ([`BoundedLock::acquire`]). Acquisition takes a
//!   deadline and returns [`StoreError::LockTimeout`], mirroring the
//!   `LOCK_EX|LOCK_NB` polling idiom in `cli/src/fno/plan/locking.py`. There
//!   is no unbounded acquire to call.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Keep at most this many timestamped backups on disk (store.py
/// GRAPH_BACKUP_KEEP).
pub const GRAPH_BACKUP_KEEP: usize = 10;

/// Bounded lock default deadline. A wedged writer must surface as
/// [`StoreError::LockTimeout`] inside this budget instead of blocking a graph
/// caller forever (store.py's unbounded `fcntl.flock` was the original defect
/// this port retires).
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for the bounded lock, mirroring plan/locking.py's 20ms.
const LOCK_POLL: Duration = Duration::from_millis(20);

/// Canonical key order for serialized graph entries (store.py
/// CANONICAL_FIELD_ORDER). Keys NOT listed are appended in their original
/// order after the canonical block, so forward-compat additions and legacy
/// extras are reordered-around, never dropped.
pub const CANONICAL_FIELD_ORDER: &[&str] = &[
    "id",
    "status",
    "slug",
    "title",
    "priority",
    "rank",
    "type",
    "parent",
    "children",
    "project",
    "cwd",
    "domain",
    "blocked_by",
    "related",
    "dep",
    "stub_against",
    "contract_version",
    "locked_by",
    "locked_by_harness",
    "locked_by_harness_session",
    "session_id",
    "locked_at",
    "ownership_defect",
    "completed_at",
    "deferred_at",
    "deferred_reason",
    "deferred_kind",
    "touched_at",
    "has_brief",
    "roadmap_id",
    "vision_path",
    "details",
    "size",
    "batch",
    "cost_usd",
    "cost_sessions",
    "contained_in",
    "plan_path",
    "pr_number",
    "pr_url",
    "additional_prs",
    "merge_status",
    "artifact_url",
    "completion_note",
    "progress_notes",
    "created_at",
    "supersedes",
    "superseded_by",
    "supersession",
    "collisions_acknowledged",
    "source",
    "source_kind",
    "source_project",
    "source_session_id",
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "source_inbox_msg",
    "spawned_by_session",
    "spawned_by_harness",
    "spawned_by_cwd",
    "sessions",
    "encounters",
    "queued_at",
    "queued_reason",
];

/// Fields copied into each parent's `children` summary (store.py
/// CHILD_SUMMARY_FIELDS).
pub const CHILD_SUMMARY_FIELDS: &[&str] = &["id", "title", "project", "status"];

/// Fields whose change marks a node as human-curated "just now" (x-7dcb).
const CURATION_FIELDS: &[&str] = &["status", "priority", "rank", "parent", "blocked_by", "size"];

/// Legacy `priority` vocabulary -> current (constants.PRIORITY_MIGRATION).
const PRIORITY_MIGRATION: &[(&str, &str)] = &[("high", "p1"), ("medium", "p2"), ("low", "p3")];

/// Legacy `status` vocabulary -> current (statuses.STATUS_MIGRATION).
const STATUS_MIGRATION: &[(&str, &str)] = &[("claimed", "in_progress")];

/// Derived `status` vocabulary that outranks the blocked read-time overlay.
const OVERLAY_TERMINAL_STATUSES: &[&str] = &["done", "superseded", "deferred", "in_review"];

/// Terminal rungs (statuses.TERMINAL_RUNGS): past these a node is closed.
pub const TERMINAL_RUNGS: &[&str] = &["done", "superseded"];

/// Sentinel prefix the pre-feature workaround overloaded `completed_at` with
/// to encode deferral (statuses._LEGACY_DEFER_PREFIX).
const LEGACY_DEFER_PREFIX: &str = "deferred:";

/// Plan-frontmatter status -> rung (ladder._STATUS_TO_RUNG), after
/// canonical_status resolved the retired spellings.
const STATUS_TO_RUNG: &[(&str, &str)] = &[
    ("idea", "idea"),
    ("design", "design"),
    ("ready", "ready"),
    ("in_progress", "in_progress"),
    ("in_review", "in_review"),
    ("done", "done"),
    ("superseded", "superseded"),
];

/// Retired plan spellings (plan._status.STATUS_ALIASES), accepted on read.
const PLAN_STATUS_ALIASES: &[(&str, &str)] = &[
    ("shipped", "in_review"),
    ("archived", "superseded"),
    ("stub", "idea"),
];

/// Rung -> derived graph status (statuses._rung_to_graph_status). NONE and
/// IDEA both derive `idea`; the plan-side terminals map to `ready`: graph
/// truth for them is completed_at/pr_number/superseded_by, which the
/// precedence block consumes first.
fn rung_to_graph_status(rung: &str) -> &'static str {
    match rung {
        "none" | "idea" => "idea",
        "design" => "design",
        _ => "ready",
    }
}

/// Store-level failure taxonomy, mirroring the Python exceptions by name.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Corrupt(String),
    #[error("graph at {0} could not be read: {1}")]
    Unreadable(String, String),
    #[error("graph at {0} root object has no 'entries' key")]
    MalformedRoot(String),
    #[error("lock at {0} stayed busy past the {1:?} deadline")]
    LockTimeout(String, Duration),
    #[error("version conflict: the graph changed since the caller's snapshot")]
    Conflict,
    #[error("a field update carries no value: {0}")]
    EmptyFieldUpdate(String),
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The text fields the presence invariant guards. A populated value replaced
/// by an empty one is the measured wipe class; the update type makes it
/// unconstructible.
pub const PRESENCE_TEXT_FIELDS: &[&str] = &["details", "completion_note", "title"];

/// A non-empty text value. `TextField::parse` is the only constructor, so a
/// `FieldUpdate::Set` can never carry the empty overwrite: the caller that
/// tried is refused with [`StoreError::EmptyFieldUpdate`] naming the field.
#[derive(Debug, Clone, PartialEq)]
pub struct TextField(String);

impl TextField {
    /// Accept a text value for a store write, refusing empty/whitespace-only.
    pub fn parse(field: &str, raw: &str) -> Result<Self, StoreError> {
        if raw.trim().is_empty() {
            return Err(StoreError::EmptyFieldUpdate(format!(
                "refusing to write an empty value to '{field}': pass real content; \
                 clearing a populated field is a separate explicit operation"
            )));
        }
        Ok(TextField(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One field update, carrying its own presence (change 1's first invariant).
#[derive(Debug, Clone, PartialEq)]
pub enum FieldUpdate {
    /// Write this (non-empty) text value.
    Set(TextField),
    /// Leave the field exactly as it is. The default; a no-op is expressible,
    /// a silent wipe is not.
    Keep,
    /// Remove the key explicitly. Deliberate; never reachable by passing an
    /// empty value to [`FieldUpdate::parse`].
    Clear,
    /// Write a non-text JSON value verbatim (a rank number, a flag bool).
    /// Only reachable for fields OUTSIDE [`PRESENCE_TEXT_FIELDS`]: the
    /// presence invariant governs text, not structured values.
    Raw(Value),
}

impl FieldUpdate {
    /// Build the update a caller's raw value means. `None` (flag absent)
    /// keeps; a present-but-empty string is refused for the presence-guarded
    /// text fields, which is the whole point of the type.
    pub fn from_cli(field: &str, raw: Option<&str>) -> Result<Self, StoreError> {
        match raw {
            None => Ok(FieldUpdate::Keep),
            Some(v) => Ok(FieldUpdate::Set(TextField::parse(field, v)?)),
        }
    }

    /// Build the update a JSON request value means. Strings on a presence
    /// field go through [`TextField::parse`]; non-text values on other
    /// fields (the board rank's float, most notably) set verbatim.
    pub fn from_value(field: &str, value: &Value) -> Result<Self, StoreError> {
        match value {
            Value::Null => Ok(FieldUpdate::Keep),
            Value::Object(o) if o.get("clear") == Some(&Value::Bool(true)) => {
                Ok(FieldUpdate::Clear)
            }
            Value::String(s) => Ok(FieldUpdate::Set(TextField::parse(field, s)?)),
            other => {
                if PRESENCE_TEXT_FIELDS.contains(&field) {
                    let s = other.as_str().unwrap_or_default();
                    Ok(FieldUpdate::Set(TextField::parse(field, s)?))
                } else {
                    Ok(FieldUpdate::Raw(other.clone()))
                }
            }
        }
    }
}

/// Apply one field update to an entry. Unknown fields are set verbatim (the
/// graph is schema-extra-allow); the presence invariant is enforced by the
/// [`FieldUpdate`] constructors, not here.
pub fn apply_field_update(entry: &mut Map<String, Value>, field: &str, update: &FieldUpdate) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Set(text) => {
            entry.insert(field.to_string(), Value::String(text.as_str().to_string()));
        }
        FieldUpdate::Clear => {
            entry.shift_remove(field);
        }
        FieldUpdate::Raw(value) => {
            entry.insert(field.to_string(), value.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Python-compatible JSON serialization
// ---------------------------------------------------------------------------

/// Serialize `value` exactly as Python's `json.dumps(value, indent=2,
/// ensure_ascii=True)` would: two-space indent, `": "` separator, `\uXXXX`
/// escapes for every non-ASCII code point (surrogate pairs beyond the BMP),
/// and the four short control escapes where Python uses them.
///
/// Byte compatibility is a port requirement, not cosmetic: the differential
/// parity stage asserts the Rust store publishes the same bytes the Python
/// store did, and every `fno backlog` consumer reads this file.
pub fn to_python_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, 0, &mut out);
    out
}

fn write_value(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(s, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            let pad = "  ".repeat(indent + 1);
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad);
                write_value(item, indent + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let pad = "  ".repeat(indent + 1);
            for (i, (k, v)) in map.iter().enumerate() {
                out.push_str(&pad);
                write_json_string(k, out);
                out.push_str(": ");
                write_value(v, indent + 1, out);
                if i + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push('}');
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) <= 0x7e => out.push(c),
            // ensure_ascii: everything above DEL becomes \uXXXX, with a
            // surrogate pair past the BMP, matching Python's json module.
            c => {
                let cp = c as u32;
                if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x1_0000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
}

/// The serialized graph file body: `{"entries": [...]}` pretty-printed with a
/// trailing newline, byte-identical to Python's
/// `json.dumps({"entries": entries}, indent=2) + "\n"`.
pub fn serialize_graph_file(entries: &[Value]) -> String {
    let mut root = Map::new();
    root.insert("entries".to_string(), Value::Array(entries.to_vec()));
    let mut out = to_python_json(&Value::Object(root));
    out.push('\n');
    out
}

/// The serialized READ result: the defaulted entries as a JSON array,
/// byte-identical to Python's `json.dumps(read_graph(path), indent=2)`.
pub fn serialize_entries(entries: &[Value]) -> String {
    to_python_json(&Value::Array(entries.to_vec()))
}

// ---------------------------------------------------------------------------
// Raw read: the four-shape taxonomy from _read_json / read_graph_strict
// ---------------------------------------------------------------------------

/// The strict read outcome. `Empty` covers an absent file (an absent graph is
/// empty, not unreadable - matching read_graph and read_graph_strict).
pub enum RawRead {
    /// File absent: zero entries, no error.
    Empty,
    /// Parsed `entries` list.
    Entries(Vec<Value>),
    /// Root not a JSON object, bad JSON, or `entries` not a list.
    Corrupt(String),
    /// Root is an object but carries no `entries` key.
    MalformedRoot,
}

/// Defuse the non-finite float literals Python's `json.dumps` writes bare
/// (`Infinity`, `-Infinity`, `NaN`) into JSON-null, string-aware. The old
/// Python store read such a file happily and its rank math treated a
/// non-finite rank as unranked; the ported store cannot carry a non-finite
/// f64 at all, so the honest equivalent is to read them as null (unranked)
/// and let the next write publish a finite file. Tokens OUTSIDE strings
/// only: a string value spelling "Infinity" is data, not a float.
fn defuse_nonfinite(text: &str) -> String {
    const TOKENS: [&str; 3] = ["Infinity", "-Infinity", "NaN"];
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut rest = text;
    while !rest.is_empty() {
        let c = rest.chars().next().unwrap();
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            rest = &rest[c.len_utf8()..];
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let hit = TOKENS.iter().find(|t| rest.starts_with(**t));
        if let Some(tok) = hit {
            let after = &rest[tok.len()..];
            let next = after.chars().next();
            let delimited = next.is_none()
                || matches!(
                    next,
                    Some(',')
                        | Some('}')
                        | Some(']')
                        | Some(' ')
                        | Some('\n')
                        | Some('\r')
                        | Some('\t')
                );
            if delimited {
                out.push_str("null");
                rest = after;
                continue;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Raw read of the entries file. Raises nothing; callers map [`RawRead`] onto
/// their own strictness (read_graph swallows Corrupt to empty; the strict
/// read surfaces it).
pub fn read_raw(path: &Path) -> Result<RawRead, StoreError> {
    if !path.exists() {
        return Ok(RawRead::Empty);
    }
    let raw = std::fs::read(path)
        .map_err(|e| StoreError::Unreadable(path.display().to_string(), format!("{e}")))?;
    let text = String::from_utf8(raw).map_err(|e| {
        StoreError::Unreadable(path.display().to_string(), format!("not UTF-8: {e}"))
    })?;
    if text.trim().is_empty() {
        return Err(StoreError::Unreadable(
            path.display().to_string(),
            "empty (zero bytes)".to_string(),
        ));
    }
    let text = if text.contains("Infinity") || text.contains("NaN") {
        defuse_nonfinite(&text)
    } else {
        text
    };
    // The three parse shapes answer as RawRead::Corrupt (not Err) so the
    // caller decides strictness: the soft read backs the bytes up before
    // surfacing them; the strict read just diagnoses.
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return Ok(RawRead::Corrupt(format!(
                "{} is not valid JSON",
                path.display()
            )))
        }
    };
    let Some(obj) = data.as_object() else {
        return Ok(RawRead::Corrupt(format!(
            "{} root is not a JSON object",
            path.display()
        )));
    };
    let Some(entries) = obj.get("entries") else {
        return Ok(RawRead::MalformedRoot);
    };
    let Some(list) = entries.as_array() else {
        return Ok(RawRead::Corrupt(format!(
            "{} 'entries' is not a list",
            path.display()
        )));
    };
    Ok(RawRead::Entries(list.clone()))
}

// ---------------------------------------------------------------------------
// Time helpers matching the Python stamps byte-for-byte
// ---------------------------------------------------------------------------

/// Python `datetime.now(timezone.utc).isoformat()`: seconds, plus
/// microseconds only when nonzero, `+00:00` suffix.
pub fn now_isoformat() -> String {
    let now = chrono::Utc::now();
    let micros = now.timestamp_subsec_micros();
    if micros == 0 {
        now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        now.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// Python `strftime("%Y%m%dT%H%M%S%f")` in UTC, the backup filename stamp.
fn backup_stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%f").to_string()
}

// ---------------------------------------------------------------------------
// Value-shaped entry helpers (entries stay schema-extra-allow dicts)
// ---------------------------------------------------------------------------

pub fn s_str<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

pub fn entry_id(entry: &Value) -> Option<&str> {
    s_str(entry, "id")
}

pub fn is_dict(v: &Value) -> bool {
    v.is_object()
}

// ---------------------------------------------------------------------------
// Defaults pipeline (_apply_graph_defaults and friends)
// ---------------------------------------------------------------------------

/// Populate each entry's `children` with summaries of its direct children,
/// rebuilt from scratch so it can never drift. The summary's `status` derives
/// through the same readiness overlay every live read applies. Mirrors
/// store._compute_children.
pub fn compute_children(entries: &mut [Value]) {
    let ids: Vec<String> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e).map(str::to_string))
        .collect();
    let id_to_entry = index_by_id(entries);
    let mut kids: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for e in entries.iter() {
        if !is_dict(e) {
            // A junk row is skipped, never dropped here: the read path's
            // evidence caller needs malformed rows to survive the pass.
            continue;
        }
        let (Some(cid), Some(parent)) = (entry_id(e).map(str::to_string), s_str(e, "parent"))
        else {
            continue;
        };
        // A self-parented node must not become its own child: that would
        // accumulate on every write with no self-healing path.
        if !ids.iter().any(|i| i == parent) || cid == parent {
            continue;
        }
        let mut summary = Map::new();
        for f in CHILD_SUMMARY_FIELDS {
            summary.insert((*f).to_string(), e.get(*f).cloned().unwrap_or(Value::Null));
        }
        let (status, _reason) = readiness_status(e, &id_to_entry);
        summary.insert(
            "status".to_string(),
            status.map(Value::String).unwrap_or(Value::Null),
        );
        kids.entry(parent.to_string())
            .or_default()
            .push(Value::Object(summary));
    }
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let eid = entry_id(e).map(str::to_string);
        let summaries = eid.as_deref().and_then(|id| kids.get_mut(id));
        match summaries {
            Some(summaries) => {
                summaries.sort_by(|a, b| {
                    a.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .cmp(b.get("id").and_then(Value::as_str).unwrap_or(""))
                });
                e.as_object_mut().unwrap().insert(
                    "children".to_string(),
                    Value::Array(std::mem::take(summaries)),
                );
            }
            None => {
                e.as_object_mut()
                    .unwrap()
                    .insert("children".to_string(), Value::Array(vec![]));
            }
        }
    }
}

/// Reconcile the lock-owner field to locked_by, mirroring session_id
/// (store._normalize_lock_fields). Idempotent; mutates in place.
pub fn normalize_lock_fields(entries: &mut [Value]) {
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        if !obj.contains_key("locked_by") {
            // Legacy row: on a LIVE node session_id IS the lock owner; on a
            // done node it is work/cost provenance, never a lock.
            let adopted = if obj
                .get("completed_at")
                .map(|v| !v.is_null())
                .unwrap_or(false)
            {
                Value::Null
            } else {
                obj.get("session_id").cloned().unwrap_or(Value::Null)
            };
            obj.insert("locked_by".to_string(), adopted);
        }
        let resolved = obj.get("locked_by").map(|v| !v.is_null()).unwrap_or(false);
        if resolved {
            let owner = obj.get("locked_by").cloned().unwrap_or(Value::Null);
            obj.insert("session_id".to_string(), owner);
        } else if !obj
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            // Released and not done: drop the stale mirror.
            obj.insert("session_id".to_string(), Value::Null);
        }
        if !resolved {
            // A cleared owner must not retain a holder identity.
            obj.insert("locked_by_harness".to_string(), Value::Null);
            obj.insert("locked_by_harness_session".to_string(), Value::Null);
        }
    }
}

/// A blocker that is deferred holds its dependents, but the KIND of hold is
/// a human decision, not an in-flight one: `fno backlog defer` parked it.
fn is_deferred_blocker(blocker: &Value) -> bool {
    blocker
        .get("deferred_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
        || blocker.get("status").and_then(Value::as_str) == Some("deferred")
}

/// Follow a superseded blocker to the node that actually owns the work
/// (the `superseded_by` chain, bounded so a cycle reads as an unknown dep
/// instead of looping). `Ok` carries the effective entry and id; `Err`
/// carries the last id visited when the chain hits a missing row or
/// overruns the hop bound.
fn effective_blocker(
    blocker: &Value,
    blocker_id: &str,
    by_id: &std::collections::HashMap<String, Value>,
) -> Result<(Value, String), String> {
    const MAX_CHAIN_HOPS: usize = 8;
    let mut current = blocker.clone();
    let mut current_id = blocker_id.to_string();
    for _ in 0..MAX_CHAIN_HOPS {
        let Some(next_id) = current.get("superseded_by").and_then(Value::as_str) else {
            return Ok((current, current_id));
        };
        let Some(next) = by_id.get(next_id).cloned() else {
            return Err(next_id.to_string());
        };
        current_id = next_id.to_string();
        current = next;
    }
    Err(current_id) // overrun: a chain this long is a cycle in disguise
}

/// Read-time dependency readiness for one entry: never a boolean
/// (statuses.compute_readiness). A blocker superseded by another node is
/// chased to its live successor, so a done successor releases the dependent
/// and an open one is named in the reason; a deferred blocker holds with a
/// kind of its own (statuses stays blocked for both).
pub fn compute_readiness(
    entry: &Value,
    by_id: &std::collections::HashMap<String, Value>,
) -> (String, Option<String>) {
    let Some(blockers) = entry.get("blocked_by").and_then(Value::as_array) else {
        return ("ready".to_string(), None);
    };
    for blocker_id in blockers {
        let Some(bid) = blocker_id.as_str() else {
            continue;
        };
        let Some(blocker) = by_id.get(bid) else {
            return ("unknown-dep".to_string(), Some(bid.to_string()));
        };
        if blocker
            .get("completed_at")
            .map(|v| v.is_null())
            .unwrap_or(true)
        {
            let (effective, effective_id) = match effective_blocker(blocker, bid, by_id) {
                Ok(pair) => pair,
                // Chain overrun or missing link (a cycle in disguise): the
                // last id visited is the honest name for what holds this edge.
                Err(last_id) => return ("unknown-dep".to_string(), Some(last_id)),
            };
            if effective
                .get("completed_at")
                .map(|v| v.is_null())
                .unwrap_or(true)
            {
                if is_deferred_blocker(&effective) {
                    return ("blocked-by-deferred".to_string(), Some(effective_id));
                }
                return ("blocked-by".to_string(), Some(effective_id));
            }
        }
    }
    ("ready".to_string(), None)
}

/// Describe a proposed supersession that lacks merged-PR proof
/// (statuses.pending_supersession_reason).
pub fn pending_supersession_reason(entry: &Value) -> Option<String> {
    let superseded_by = entry.get("superseded_by").map(|v| !v.is_null())?;
    if !superseded_by {
        return None;
    }
    let record = entry.get("supersession")?;
    if !record.is_object() {
        return None;
    }
    if record
        .get("verified_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        return None;
    }
    let successor = record
        .get("successor")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            entry
                .get("superseded_by")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "missing successor".to_string());
    let cause = record
        .get("cause")
        .and_then(Value::as_str)
        .unwrap_or("missing cause");
    let surfaces = record
        .get("surfaces")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| match s {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "missing surfaces".to_string());
    let surface_text = if surfaces.is_empty() {
        "missing surfaces".to_string()
    } else {
        surfaces
    };
    Some(format!(
        "pending supersession: successor={successor}; cause={cause}; surfaces={surface_text}"
    ))
}

/// Open in the `_reconcile.node_is_open` sense: neither done nor
/// superseded-closed. Keyed off the underlying fields so it holds on rows
/// that have not been through a status recompute.
fn is_open_entry(entry: &Value) -> bool {
    if entry
        .get("completed_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        return false;
    }
    let Some(successor) = entry.get("superseded_by").and_then(Value::as_str) else {
        return true;
    };
    if successor.is_empty() {
        return true;
    }
    match entry.get("supersession") {
        Some(r) if r.is_object() => r.get("verified_at").map(|v| v.is_null()).unwrap_or(true),
        _ => false,
    }
}

/// The blocked_by edge settlement: the write-side twin of the chase in
/// `compute_readiness`. For every open node, prune an edge whose blocker is
/// done (directly or through its supersession chain), rewire one superseded
/// to its live successor, and hold the deferred or missing with a receipt -
/// a deferred blocker is a human decision and a missing one is data loss,
/// neither is a sweep's to erase. A live blocker gets no receipt: a correct
/// edge is not a finding. Returns the rewritten entries, one receipt per
/// settled edge, and the per-node new `blocked_by` list for every node the
/// sweep changed; the caller persists under the graph lock (the Python full
/// sweep applies the change map and reports the receipts).
pub fn settle_blocked_by_edges(
    mut entries: Vec<Value>,
) -> (
    Vec<Value>,
    Vec<Value>,
    std::collections::BTreeMap<String, Vec<Value>>,
) {
    let by_id = index_by_id(&entries);
    let mut receipts: Vec<Value> = Vec::new();
    let mut changes: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for e in entries.iter_mut() {
        if !is_dict(e) || !is_open_entry(e) {
            continue;
        }
        let Some(blockers) = e.get("blocked_by").and_then(Value::as_array) else {
            continue;
        };
        if blockers.is_empty() {
            continue;
        }
        let node_id = entry_id(e).unwrap_or("").to_string();
        let mut settled: Vec<Value> = Vec::with_capacity(blockers.len());
        let mut changed = false;
        for blocker_id in blockers {
            let Some(bid) = blocker_id.as_str() else {
                settled.push(blocker_id.clone());
                continue;
            };
            let Some(target) = by_id.get(bid) else {
                receipts.push(json_receipt(
                    "blocked_by_held",
                    &node_id,
                    bid,
                    "blocker missing from graph",
                ));
                settled.push(blocker_id.clone());
                continue;
            };
            if !target
                .get("completed_at")
                .map(|v| v.is_null())
                .unwrap_or(true)
            {
                receipts.push(json_receipt(
                    "blocked_by_pruned",
                    &node_id,
                    bid,
                    "blocker done",
                ));
                changed = true;
                continue;
            }
            if target
                .get("superseded_by")
                .and_then(Value::as_str)
                .is_none()
            {
                if is_deferred_blocker(target) {
                    receipts.push(json_receipt(
                        "blocked_by_held",
                        &node_id,
                        bid,
                        "blocker deferred",
                    ));
                    settled.push(blocker_id.clone());
                } else {
                    settled.push(blocker_id.clone());
                }
                continue;
            }
            let (effective, effective_id) = match effective_blocker(target, bid, &by_id) {
                Ok(pair) => pair,
                Err(last_id) => {
                    receipts.push(json_receipt(
                        "blocked_by_held",
                        &node_id,
                        bid,
                        &format!("supersession chain stops at {last_id}"),
                    ));
                    settled.push(blocker_id.clone());
                    continue;
                }
            };
            if effective
                .get("completed_at")
                .map(|v| v.is_null())
                .unwrap_or(true)
            {
                let already_named = settled
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|s| s == effective_id);
                if !already_named {
                    settled.push(Value::String(effective_id.clone()));
                }
                receipts.push(json_receipt(
                    "blocked_by_rewired",
                    &node_id,
                    bid,
                    "blocker superseded; edge now names the live successor",
                ));
                changed = true;
            } else {
                receipts.push(json_receipt(
                    "blocked_by_pruned",
                    &node_id,
                    bid,
                    &format!("superseded by {effective_id}, which is done"),
                ));
                changed = true;
            }
        }
        if changed {
            changes.insert(node_id.clone(), settled.clone());
            e.as_object_mut()
                .unwrap()
                .insert("blocked_by".to_string(), Value::Array(settled));
        }
    }
    (entries, receipts, changes)
}

fn json_receipt(kind: &str, node: &str, blocker: &str, reason: &str) -> Value {
    serde_json::json!({"kind": kind, "node": node, "blocker": blocker, "reason": reason})
}

/// The one overlay wrapper every status consumer shares
/// (statuses.readiness_status): terminal statuses pass through, everything
/// else overlays compute_readiness.
pub fn readiness_status(
    entry: &Value,
    by_id: &std::collections::HashMap<String, Value>,
) -> (Option<String>, Option<String>) {
    let status = entry.get("status").and_then(Value::as_str);
    if let Some(s) = status {
        if OVERLAY_TERMINAL_STATUSES.contains(&s) {
            return (Some(s.to_string()), None);
        }
    }
    if let Some(reason) = pending_supersession_reason(entry) {
        return (Some("blocked".to_string()), Some(reason));
    }
    let (kind, blocker_id) = compute_readiness(entry, by_id);
    if kind == "ready" {
        return (status.map(str::to_string), None);
    }
    (
        Some("blocked".to_string()),
        Some(match blocker_id {
            Some(id) => format!("{kind}:{id}"),
            None => kind,
        }),
    )
}

fn index_by_id(entries: &[Value]) -> std::collections::HashMap<String, Value> {
    entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e).map(|i| (i.to_string(), e.clone())))
        .collect()
}

/// Overlay read-time dependency readiness onto `status`/`blocked_reason`
/// (store._apply_readiness_overlay).
pub fn apply_readiness_overlay(entries: &mut [Value]) {
    let by_id = index_by_id(entries);
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let (status, reason) = readiness_status(e, &by_id);
        let obj = e.as_object_mut().unwrap();
        obj.insert(
            "status".to_string(),
            status.map(Value::String).unwrap_or(Value::Null),
        );
        obj.insert(
            "blocked_reason".to_string(),
            reason.map(Value::String).unwrap_or(Value::Null),
        );
    }
}

/// The lock TTL, read from TASK_LOCK_TTL_HOURS at first use (statuses:
/// LOCK_TTL_HOURS env read).
fn lock_ttl_hours() -> f64 {
    static TTL: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        std::env::var("TASK_LOCK_TTL_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2.0)
    })
}

/// Classify the graph lock timestamp without deciding owner death
/// (statuses.lock_timestamp_quality).
pub fn lock_timestamp_quality(entry: &Value) -> &'static str {
    let holder = entry
        .get("locked_by")
        .map(|v| !v.is_null())
        .unwrap_or(false)
        || entry
            .get("session_id")
            .map(|v| !v.is_null())
            .unwrap_or(false);
    if !holder {
        return "fresh";
    }
    let lock_time_str = entry
        .get("locked_at")
        .and_then(Value::as_str)
        .or_else(|| entry.get("claimed_at").and_then(Value::as_str));
    let Some(ts) = lock_time_str else {
        return "unreadable";
    };
    // Python: datetime.fromisoformat(ts.replace("Z", "+00:00")).
    let parsed = chrono::DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00"));
    let Ok(parsed) = parsed else {
        return "unreadable";
    };
    let elapsed = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
    if (elapsed as f64) / 3600.0 > lock_ttl_hours() {
        "old"
    } else {
        "fresh"
    }
}

/// Apply lazy migration defaults to graph entries (store._apply_graph_defaults)
/// - the one migration seam every reader routes through. Mutates in place.
pub fn apply_defaults(entries: &mut Vec<Value>, keep_malformed: bool) {
    let migration = |raw: &str| -> Option<&'static str> {
        PRIORITY_MIGRATION
            .iter()
            .find(|(from, _)| *from == raw)
            .map(|(_, to)| *to)
    };
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        if let Some(old) = obj.get("_status").cloned() {
            obj.entry("status".to_string()).or_insert(old);
            obj.shift_remove("_status");
        }
        if let Some(old) = obj.get("priority").and_then(Value::as_str) {
            if let Some(new) = migration(old) {
                obj.insert("priority".to_string(), Value::String(new.to_string()));
            }
        }
        if let Some(old) = obj.get("status").and_then(Value::as_str) {
            if let Some((_, to)) = STATUS_MIGRATION.iter().find(|(from, _)| *from == old) {
                obj.insert("status".to_string(), Value::String(to.to_string()));
            }
        }
    }
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        // Insertion order IS the byte contract: Python's dict remembers the
        // order each setdefault first ran, so a legacy row's fresh keys land
        // in exactly this sequence - locked_at is inserted by its own
        // conditional block BETWEEN locked_by_harness_session and
        // completed_at, not after the tail.
        for (k, v) in [
            ("parent", Value::Null),
            ("tags", Value::Array(vec![])),
            ("type", Value::String("feature".into())),
            ("project", Value::Null),
            ("cwd", Value::Null),
            ("priority", Value::String("p2".into())),
            ("rank", Value::Null),
            ("domain", Value::String("code".into())),
            ("blocked_by", Value::Array(vec![])),
            ("session_id", Value::Null),
            ("locked_by_harness", Value::Null),
            ("locked_by_harness_session", Value::Null),
        ] {
            obj.entry(k.to_string()).or_insert_with(|| v.clone());
        }
        if !obj.contains_key("locked_at") {
            let legacy = obj.get("claimed_at").and_then(Value::as_str);
            let stamped = match legacy {
                Some(s) if !s.trim().is_empty() => {
                    chrono::DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00"))
                        .map(|_| Value::String(s.to_string()))
                        .unwrap_or(Value::Null)
                }
                _ => Value::Null,
            };
            obj.insert("locked_at".to_string(), stamped);
        }
        for (k, v) in [
            ("completed_at", Value::Null),
            ("status", Value::String("ready".into())),
            ("slug", Value::Null),
            ("children", Value::Array(vec![])),
            ("has_brief", Value::Bool(false)),
            ("roadmap_id", Value::Null),
            ("vision_path", Value::Null),
            ("details", Value::Null),
            ("cost_usd", Value::Null),
            ("cost_sessions", Value::Array(vec![])),
            ("size", Value::Null),
            ("batch", Value::Null),
            ("plan_path", Value::Null),
            ("pr_number", Value::Null),
            ("pr_url", Value::Null),
            ("additional_prs", Value::Array(vec![])),
            ("merge_status", Value::Null),
            ("artifact_url", Value::Null),
            ("completion_note", Value::Null),
            ("progress_notes", Value::Array(vec![])),
            ("collisions_acknowledged", Value::Array(vec![])),
            ("related", Value::Array(vec![])),
            ("supersedes", Value::Array(vec![])),
            ("superseded_by", Value::Null),
            ("supersession", Value::Null),
            ("source_kind", Value::String("organic".into())),
            ("source_project", Value::Null),
            ("source_session_id", Value::Null),
            ("source_harness", Value::Null),
            ("source_cwd", Value::Null),
            ("source_node_id", Value::Null),
            ("source_plan_path", Value::Null),
            ("source_inbox_msg", Value::Null),
            ("spawned_by_session", Value::Null),
            ("spawned_by_harness", Value::Null),
            ("spawned_by_cwd", Value::Null),
            ("sessions", Value::Array(vec![])),
            ("decisions", Value::Array(vec![])),
            ("queued_at", Value::Null),
            ("queued_reason", Value::Null),
        ] {
            obj.entry(k.to_string()).or_insert_with(|| v.clone());
        }
    }
    normalize_lock_fields(entries);
    apply_readiness_overlay(entries);
    compute_children(entries);
    if !keep_malformed {
        entries.retain(|e| is_dict(e));
    }
}

// ---------------------------------------------------------------------------
// plan_rung: supplied by the client, never read here
// ---------------------------------------------------------------------------

/// The rung a node's linked plan sits on, as the CLIENT supplied it
/// (ladder.plan_rung answers on the Python side of the seam: repo law keeps
/// plan-document reading in the Python rung table, so the store consumes
/// rungs as data). `rungs` maps node id to the ladder rung string; a node
/// absent from the map reads as rung "none".
pub fn supplied_plan_rung(entry: &Value, rungs: &BTreeMap<String, String>) -> &'static str {
    let raw = match (is_dict(entry), entry_id(entry)) {
        (true, Some(id)) => rungs.get(id).map(String::as_str),
        _ => None,
    }
    .unwrap_or("none");
    let s = raw.trim().trim_matches(['\'', '"']).to_lowercase();
    if s == "none" {
        // Not a table member: "nothing on disk" answers NONE directly.
        return "none";
    }
    let canonical = PLAN_STATUS_ALIASES
        .iter()
        .find(|(from, _)| *from == &s)
        .map(|(_, to)| *to)
        .unwrap_or(s.as_str());
    STATUS_TO_RUNG
        .iter()
        .find(|(from, _)| *from == canonical)
        .map(|(_, rung)| *rung)
        .unwrap_or("unreadable")
}

// ---------------------------------------------------------------------------
// recompute_statuses
// ---------------------------------------------------------------------------

/// Is this entry closed for good (statuses.is_terminal_entry semantics
/// without the legacy-sentinel nuance the store's callers need)?
pub fn is_terminal_entry(entry: &Value) -> bool {
    if !is_dict(entry) {
        return false;
    }
    if entry
        .get("status")
        .and_then(Value::as_str)
        .map(|s| TERMINAL_RUNGS.contains(&s))
        .unwrap_or(false)
        || entry
            .get("superseded_by")
            .map(|v| !v.is_null())
            .unwrap_or(false)
    {
        return true;
    }
    entry
        .get("completed_at")
        .and_then(Value::as_str)
        .map(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
        .unwrap_or(false)
}

fn is_open_phase_row(row: &Value, phase: &str) -> bool {
    row.get("phase").and_then(Value::as_str) == Some(phase)
        && row
            .get("harness")
            .and_then(Value::as_str)
            .map(|h| !h.trim().is_empty())
            .unwrap_or(false)
        && row
            .get("session_id")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        && row
            .get("started_at")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        && !row
            .as_object()
            .map(|o| o.contains_key("ended_at"))
            .unwrap_or(false)
}

fn is_open_do_row(row: &Value) -> bool {
    is_open_phase_row(row, "do")
}

/// Recompute status for all entries based on graph state
/// (statuses.recompute_statuses). The write path's derivation: blocked_by is
/// deliberately NOT derived here - dependency satisfaction is answered fresh
/// on every read instead.
pub fn recompute_statuses(entries: &mut [Value]) {
    recompute_statuses_with_plan_rungs(entries, None)
}

/// `plan_rungs` is None when the caller supplies no plan data: plan-derived
/// statuses then keep their stored values instead of re-deriving (a mux-side
/// op write never flips a node's status behind the plan's back). The Python
/// client always supplies the map, computed by `ladder.plan_rung`.
pub fn recompute_statuses_with_plan_rungs(
    entries: &mut [Value],
    plan_rungs: Option<&BTreeMap<String, String>>,
) {
    normalize_lock_fields(entries);

    let valid_ids: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e).map(str::to_string))
        .collect();

    // children_by_parent, excluding self-parents.
    let mut children_by_parent: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, e) in entries.iter().enumerate() {
        if !is_dict(e) {
            continue;
        }
        let (Some(parent), Some(cid)) = (
            s_str(e, "parent").map(str::to_string),
            entry_id(e).map(str::to_string),
        ) else {
            continue;
        };
        if valid_ids.contains(&parent) && cid != parent {
            children_by_parent.entry(parent).or_default().push(i);
        }
    }

    // One-shot priority/status vocabulary backfill.
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        if let Some(old) = obj
            .get("priority")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if let Some((_, to)) = PRIORITY_MIGRATION.iter().find(|(from, _)| from == &old) {
                obj.insert("priority".to_string(), Value::String(to.to_string()));
            }
        }
        if let Some(old) = obj
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if let Some((_, to)) = STATUS_MIGRATION.iter().find(|(from, _)| from == &old) {
                obj.insert("status".to_string(), Value::String(to.to_string()));
            }
        }
    }

    // One-shot defer-vocabulary backfill: `completed_at: "deferred:<ts>"`.
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        let legacy = obj
            .get("completed_at")
            .and_then(Value::as_str)
            .filter(|c| c.starts_with(LEGACY_DEFER_PREFIX))
            .map(str::to_string);
        if let Some(completed) = legacy {
            obj.insert(
                "deferred_at".to_string(),
                Value::String(completed[LEGACY_DEFER_PREFIX.len()..].to_string()),
            );
            obj.insert("completed_at".to_string(), Value::Null);
            obj.entry("deferred_reason".to_string())
                .or_insert(Value::String(String::new()));
        }
    }

    for e in entries.iter_mut() {
        if entry_id(e).is_none() {
            continue;
        }
        // Never persist a stale readiness detail; clear the ownership defect
        // marker so an owner replacement cannot retain an obsolete diagnosis.
        // Decisions read first, mutations after: the derivation functions
        // borrow immutably, the writes need the mutable borrow.
        let completed = e.get("completed_at").map(|v| !v.is_null()).unwrap_or(false);
        let pending_reason = pending_supersession_reason(e);
        let superseded = e
            .get("superseded_by")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let deferred = e.get("deferred_at").map(|v| !v.is_null()).unwrap_or(false);
        let locked = e.get("locked_by").map(|v| !v.is_null()).unwrap_or(false);
        let lock_quality = if locked {
            Some(lock_timestamp_quality(e))
        } else {
            None
        };
        let has_pr = e.get("pr_number").map(|v| !v.is_null()).unwrap_or(false);
        let open_do = e
            .get("sessions")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().any(is_open_do_row))
            .unwrap_or(false);
        let rung = if !locked
            && !open_do
            && !completed
            && pending_reason.is_none()
            && !superseded
            && !deferred
            && !has_pr
        {
            match plan_rungs {
                Some(map) => Some(supplied_plan_rung(e, map)),
                // No plan data supplied: the ladder write below is skipped and
                // the entry keeps its stored status.
                None => None,
            }
        } else {
            None
        };

        let obj = e.as_object_mut().unwrap();
        obj.insert("blocked_reason".to_string(), Value::Null);
        obj.shift_remove("ownership_defect");

        if completed {
            obj.insert("status".to_string(), Value::String("done".into()));
            continue;
        }
        if let Some(reason) = pending_reason {
            obj.insert("status".to_string(), Value::String("blocked".into()));
            obj.insert("blocked_reason".to_string(), Value::String(reason));
            continue;
        }
        if superseded {
            obj.insert("status".to_string(), Value::String("superseded".into()));
            continue;
        }
        if deferred {
            obj.insert("status".to_string(), Value::String("deferred".into()));
            continue;
        }
        if let Some(quality) = lock_quality {
            // A graph timestamp is diagnostic only: age or malformed data
            // records uncertainty, never clears an owner that may be working.
            if matches!(quality, "old" | "unreadable") {
                let kind = if quality == "old" {
                    "stale-active-owner-unverified"
                } else {
                    "lock-timestamp-unreadable"
                };
                let mut defect = Map::new();
                defect.insert("kind".to_string(), Value::String(kind.into()));
                defect.insert(
                    "node_id".to_string(),
                    obj.get("id").cloned().unwrap_or(Value::Null),
                );
                defect.insert(
                    "holder".to_string(),
                    obj.get("locked_by").cloned().unwrap_or(Value::Null),
                );
                defect.insert("liveness".to_string(), Value::String("unverified".into()));
                obj.insert("ownership_defect".to_string(), Value::Object(defect));
            }
        }
        if has_pr {
            obj.insert("status".to_string(), Value::String("in_review".into()));
            continue;
        }
        if locked || open_do {
            obj.insert("status".to_string(), Value::String("in_progress".into()));
        } else if let Some(derived) = rung {
            obj.insert(
                "status".to_string(),
                Value::String(rung_to_graph_status(derived).to_string()),
            );
        }
    }

    // Container rollup, deepest first, keyed by id for index lookups.
    let id_index: std::collections::HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| is_dict(e))
        .filter_map(|(i, e)| entry_id(e).map(|id| (id.to_string(), i)))
        .collect();

    fn depth_of(
        id: &str,
        children_by_parent: &std::collections::BTreeMap<String, Vec<usize>>,
        id_index: &std::collections::HashMap<String, usize>,
        entries: &[Value],
        visiting: &mut std::collections::HashSet<String>,
    ) -> usize {
        if !visiting.insert(id.to_string()) {
            return 0;
        }
        let Some(idx) = id_index.get(id) else {
            return 0;
        };
        let parent = entries[*idx].get("parent").and_then(Value::as_str);
        let Some(parent) = parent else {
            return 0;
        };
        if !children_by_parent.contains_key(parent) {
            return 0;
        }
        1 + depth_of(parent, children_by_parent, id_index, entries, visiting)
    }

    let mut parents: Vec<(String, usize)> = children_by_parent
        .keys()
        .map(|pid| {
            let mut visiting = std::collections::HashSet::new();
            let d = depth_of(pid, &children_by_parent, &id_index, entries, &mut visiting);
            (pid.clone(), d)
        })
        .collect();
    parents.sort_by(|a, b| b.1.cmp(&a.1));

    for (pid, _) in parents {
        let Some(&pidx) = id_index.get(&pid) else {
            continue;
        };
        let parent_status = entries[pidx]
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string);
        if entries[pidx]
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            entries[pidx]
                .as_object_mut()
                .unwrap()
                .insert("status".to_string(), Value::String("done".into()));
            continue;
        }
        if let Some(reason) = pending_supersession_reason(&entries[pidx]) {
            entries[pidx]
                .as_object_mut()
                .unwrap()
                .insert("status".to_string(), Value::String("blocked".into()));
            entries[pidx]
                .as_object_mut()
                .unwrap()
                .insert("blocked_reason".to_string(), Value::String(reason));
            continue;
        }
        if entries[pidx]
            .get("superseded_by")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            entries[pidx]
                .as_object_mut()
                .unwrap()
                .insert("status".to_string(), Value::String("superseded".into()));
            continue;
        }
        if entries[pidx]
            .get("deferred_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            entries[pidx]
                .as_object_mut()
                .unwrap()
                .insert("status".to_string(), Value::String("deferred".into()));
            continue;
        }
        let child_statuses: Vec<Option<String>> = children_by_parent[&pid]
            .iter()
            .filter_map(|ci| entries.get(*ci))
            .map(|c| c.get("status").and_then(Value::as_str).map(str::to_string))
            .collect();
        // A container carrying live work of its own is NOT done just because
        // its children are.
        let own_work_live = matches!(
            parent_status.as_deref(),
            Some("in_review") | Some("in_progress")
        );
        if !child_statuses.is_empty() && child_statuses.iter().all(|s| s.as_deref() == Some("done"))
        {
            if !own_work_live {
                entries[pidx]
                    .as_object_mut()
                    .unwrap()
                    .insert("status".to_string(), Value::String("done".into()));
            }
        } else if child_statuses
            .iter()
            .any(|s| matches!(s.as_deref(), Some("in_review") | Some("in_progress")))
        {
            entries[pidx]
                .as_object_mut()
                .unwrap()
                .insert("status".to_string(), Value::String("in_progress".into()));
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical ordering + slugs
// ---------------------------------------------------------------------------

/// Reorder each entry's keys status-forward and refresh the children index
/// (store.canonicalize_entries).
pub fn canonicalize_entries(entries: &mut Vec<Value>) {
    compute_children(entries);
    normalize_lock_fields(entries);
    let mut out = Vec::with_capacity(entries.len());
    for e in entries.drain(..) {
        if !is_dict(&e) {
            out.push(e);
            continue;
        }
        let mut obj = e.as_object().unwrap().clone();
        // One-write migration: remove only the legacy top-level lock key.
        obj.shift_remove("claimed_at");
        let mut ordered = Map::new();
        for k in CANONICAL_FIELD_ORDER {
            if let Some(v) = obj.shift_remove(*k) {
                ordered.insert((*k).to_string(), v);
            }
        }
        // Unknown keys append in their original relative order.
        for (k, v) in obj {
            ordered.insert(k, v);
        }
        out.push(Value::Object(ordered));
    }
    *entries = out;
}

/// Slugify a title into a base handle (slug.derive_base_slug).
pub fn derive_base_slug(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut raw = String::new();
    let mut prev_dash = false;
    for ch in lower.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            raw.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            raw.push('-');
            prev_dash = true;
        }
    }
    let raw = raw.trim_matches('-').to_string();
    if raw.is_empty() {
        return String::new();
    }
    let words: Vec<&str> = raw.split('-').filter(|w| !w.is_empty()).collect();
    const STOPWORDS: &[&str] = &[
        "a", "an", "the", "of", "for", "to", "and", "or", "in", "on", "with",
    ];
    let kept: Vec<&str> = {
        let filtered: Vec<&str> = words
            .iter()
            .copied()
            .filter(|w| !STOPWORDS.contains(w))
            .collect();
        if filtered.is_empty() {
            words
        } else {
            filtered
        }
    };
    const WORD_BUDGET: usize = 6;
    const LEN_CAP: usize = 48;
    let mut out: Vec<&str> = Vec::new();
    let mut length = 0usize;
    for w in kept.iter().take(WORD_BUDGET) {
        let add = w.len() + if out.is_empty() { 0 } else { 1 };
        if length + add > LEN_CAP {
            break;
        }
        out.push(w);
        length += add;
    }
    if out.is_empty() {
        return kept[0].chars().take(LEN_CAP).collect();
    }
    out.join("-")
}

fn hex_fallback_slug(node_id: &str) -> String {
    let suffix = node_id.split_once('-').map(|(_, s)| s).unwrap_or(node_id);
    format!(
        "node-{}",
        if suffix.is_empty() { "unknown" } else { suffix }
    )
}

/// Assign a slug to every entry lacking one (slug.ensure_slugs). Idempotent.
pub fn ensure_slugs(entries: &mut [Value]) -> usize {
    let mut taken: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| s_str(e, "slug").map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect();
    let mut assigned = 0;
    for e in entries.iter_mut() {
        if !is_dict(e) {
            continue;
        }
        if s_str(e, "slug").map(|s| !s.is_empty()).unwrap_or(false) {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        let title = obj.get("title").and_then(Value::as_str).unwrap_or("");
        let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
        let base = derive_base_slug(title);
        let candidate = if !base.is_empty() && !taken.contains(&base) {
            base
        } else if !base.is_empty() {
            let mut n = 2;
            while taken.contains(&format!("{base}-{n}")) {
                n += 1;
            }
            format!("{base}-{n}")
        } else {
            hex_fallback_slug(id)
        };
        taken.insert(candidate.clone());
        obj.insert("slug".to_string(), Value::String(candidate));
        assigned += 1;
    }
    assigned
}

// ---------------------------------------------------------------------------
// Curation key + touched_at stamping (x-7dcb)
// ---------------------------------------------------------------------------

fn curation_key(entry: &Value) -> Value {
    let mut parts = Vec::with_capacity(CURATION_FIELDS.len());
    for f in CURATION_FIELDS {
        match entry.get(*f) {
            Some(Value::Array(items)) => parts.push(Value::Array(items.clone())),
            Some(v) => parts.push(v.clone()),
            None => parts.push(Value::Null),
        }
    }
    Value::Array(parts)
}

// ---------------------------------------------------------------------------
// Bounded lock
// ---------------------------------------------------------------------------

/// The sibling lockfile for a graph file (store._graph_lock_path), resolved so
/// two spellings of the same graph share one inode.
pub fn graph_lock_path(path: &Path) -> PathBuf {
    let base = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(format!("{}.lock", base.display()))
}

/// A held bounded lock. Release on drop.
#[derive(Debug)]
pub struct BoundedLock {
    file: File,
    path: PathBuf,
}

impl BoundedLock {
    /// Acquire the exclusive advisory lock on `<graph>.lock` with a deadline
    /// (plan/locking.py's LOCK_EX|LOCK_NB polling idiom; std's stabilized
    /// File::try_lock is the same flock(2) Python's fcntl takes, proven by
    /// tests/flock_interop.rs).
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, StoreError> {
        let lock_path = graph_lock_path(path);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(BoundedLock {
                        file,
                        path: lock_path,
                    })
                }
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(StoreError::LockTimeout(
                            lock_path.display().to_string(),
                            timeout,
                        ));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
            }
        }
    }

    pub fn lock_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BoundedLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

// ---------------------------------------------------------------------------
// Atomic publish: backup, write, sidecar
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copy the current file to a timestamped backup, prune to
/// GRAPH_BACKUP_KEEP, and return the backup path (store._create_backup).
/// None when the file does not yet exist or the copy failed (warned, never
/// fatal: the mutation proceeds).
pub fn create_backup(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    let backup = path.with_file_name(format!(
        "{}.bak.{}",
        path.file_name()?.to_string_lossy(),
        backup_stamp()
    ));
    if std::fs::copy(path, &backup).is_err() {
        return None;
    }
    let mut existing: Vec<PathBuf> = std::fs::read_dir(path.parent()?)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with(&format!(
                        "{}.bak.",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ))
                })
                .unwrap_or(false)
        })
        .collect();
    existing.sort();
    if existing.len() > GRAPH_BACKUP_KEEP {
        for old in &existing[..existing.len() - GRAPH_BACKUP_KEEP] {
            let _ = std::fs::remove_file(old);
        }
    }
    Some(backup)
}

/// Atomic whole-file write: temp sibling + rename (store._write_json).
pub fn write_atomic(path: &Path, body: &str) -> Result<(), StoreError> {
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write the SHA256 sidecar of `path` atomically
/// (store._write_sha256_sidecar).
pub fn write_sha256_sidecar(path: &Path) -> Result<(), StoreError> {
    let bytes = std::fs::read(path)?;
    let sidecar = PathBuf::from(format!("{}.sha256", path.display()));
    let tmp = path.with_file_name(format!(
        "{}.sha256.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut f = File::create(&tmp)?;
        writeln!(f, "{}", sha256_hex(&bytes))?;
    }
    std::fs::rename(&tmp, sidecar)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The locked mutate cycle (store.locked_mutate_graph, store-side half)
// ---------------------------------------------------------------------------

/// What the store-side mutate cycle reports to its caller, so the client can
/// run its post-lock duties (claim releases, renders, nudge) on the same
/// facts the file-leg implementation produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MutateOutcome {
    /// The final, canonicalized entries (the published bytes' content).
    pub entries: Vec<Value>,
    /// Entries dropped because they were not JSON objects, with the backup
    /// that preserves them named.
    pub dropped: usize,
    pub backup: Option<String>,
    /// `(node_id, rung)` pairs whose status newly entered a terminal rung
    /// during this mutation; the caller releases their claims after the lock
    /// drops.
    pub closure_releases: Vec<(String, String)>,
    /// True when this graph file is the configured canonical graph
    /// (~/.fno/graph.json), which gates claim release and board renders.
    pub is_canonical: bool,
}

/// Inputs to the store-side mutate cycle that the CLIENT computes
/// (Python-owned facts: the mutator's output and the company-work
/// validation, which is pydantic and stays on the Python side).
pub struct MutateInput {
    /// The mutator's output: entries already defaulted (the begin snapshot)
    /// and mutated client-side. Presence-refusing writes were enforced by
    /// the FieldUpdate constructors before this was built.
    pub entries: Vec<Value>,
    /// Configured canonical graph path, when the caller knows it; the
    /// closure-release and board-render gates key on it.
    pub canonical_path: Option<PathBuf>,
    /// The snapshot version (the file-content digest at begin time). The
    /// cycle refuses to publish over a changed file, so a caller whose read
    /// ran outside the lock retries on [`StoreError::Conflict`] instead of
    /// silently clobbering an interleaved writer. `None` only for callers
    /// that already serialized the whole read-apply-publish cycle under one
    /// gate (the keeper's own ops).
    pub base_version: Option<String>,
    /// Node id -> the rung of the node's linked plan, as the client computed
    /// it with the Python rung table (`ladder.plan_rung`). Repo law keeps
    /// plan-document reading on the Python side, so the store derives
    /// plan-based statuses ONLY from this map; `None` keeps stored statuses
    /// (a caller that is not re-deriving from plans).
    pub plan_rungs: Option<BTreeMap<String, String>>,
}

/// The content digest a begin/commit pair compares (the wire "version").
pub fn file_content_version(path: &Path) -> String {
    use std::io::Read;
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return "absent".to_string(),
    };
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let mut h = Sha256::new();
    h.update(&buf);
    format!("sha256:{:x}", h.finalize())
}

/// The store-side half of the locked read-modify-write cycle. Holds the
/// bounded lock; re-derives the pre-image; runs slugs, recompute, the
/// touched_at stamp, the closure-detection hook, canonicalization, and the
/// atomic publish with backup + sidecar. The MUTATOR is the caller's: it ran
/// client-side against the begin snapshot, and contention is resolved by the
/// caller retrying on [`StoreError::LockTimeout`] or a version conflict.
pub fn locked_mutate(
    path: &Path,
    input: MutateInput,
    timeout: Duration,
) -> Result<MutateOutcome, StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = BoundedLock::acquire(path, timeout)?;
    if let Some(expected) = &input.base_version {
        let current = file_content_version(path);
        if current != *expected {
            return Err(StoreError::Conflict);
        }
    }
    let raw = match read_raw(path)? {
        RawRead::Entries(v) => v,
        RawRead::Empty => vec![],
        RawRead::MalformedRoot => {
            return Err(StoreError::MalformedRoot(path.display().to_string()))
        }
        RawRead::Corrupt(reason) => return Err(StoreError::Corrupt(reason)),
    };

    // Pre-image defaults for the curation snapshot, re-derived through the
    // same pipeline (store.py's _status_normalized + _pre_curation).
    let mut pre = raw.clone();
    apply_defaults(&mut pre, false);
    let mut pre_normalized = pre.clone();
    recompute_statuses_with_plan_rungs(&mut pre_normalized, input.plan_rungs.as_ref());
    let status_normalized: std::collections::HashMap<String, String> = pre_normalized
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| {
            entry_id(e).map(|id| {
                (
                    id.to_string(),
                    e.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            })
        })
        .collect();
    let mut pre_curation: std::collections::HashMap<String, Value> = Default::default();
    for e in pre.iter() {
        let (Some(id), true) = (entry_id(e).map(str::to_string), is_dict(e)) else {
            continue;
        };
        let mut snapshot = e.clone();
        if let Some(obj) = snapshot.as_object_mut() {
            obj.insert(
                "status".to_string(),
                status_normalized
                    .get(&id)
                    .map(|s| Value::String(s.clone()))
                    .unwrap_or_else(|| e.get("status").cloned().unwrap_or(Value::Null)),
            );
        }
        pre_curation.insert(id, curation_key(&snapshot));
    }

    let dropped = raw
        .len()
        .saturating_sub(raw.iter().filter(|e| is_dict(e)).count());
    let mut entries = input.entries;

    // The presence invariant holds at the STORE boundary, not only at the
    // typed update path: the Python mutator runs client-side against plain
    // dicts, so `FieldUpdate` alone cannot see everything a commit carries.
    // An entry that arrives with an empty/whitespace-only presence field is
    // refused outright -- the measured `--details ""` wipe (3,036 characters,
    // 2026-09-02) is unrepresentable even from a hand-built payload. Clearing
    // a populated field stays expressible the explicit way: remove the key
    // ([`FieldUpdate::Clear`]), never write an empty string.
    for e in entries.iter() {
        let Some(obj) = e.as_object() else {
            continue;
        };
        let id = entry_id(e).unwrap_or("<no id>");
        for field in PRESENCE_TEXT_FIELDS {
            if let Some(Value::String(s)) = obj.get(*field) {
                if s.trim().is_empty() {
                    return Err(StoreError::EmptyFieldUpdate(format!(
                        "refusing to persist an empty '{field}' on entry '{id}': \
                         pass real content, or remove the key to clear it"
                    )));
                }
            }
        }
    }

    // Slug assignment on EVERY persisted mutation (ab-f82e8083).
    ensure_slugs(&mut entries);
    recompute_statuses_with_plan_rungs(&mut entries, input.plan_rungs.as_ref());

    // touched_at stamp: a curation-field change vs the pre-image.
    let now_iso = now_isoformat();
    for e in entries.iter_mut() {
        let (Some(id), true) = (entry_id(e).map(str::to_string), is_dict(e)) else {
            continue;
        };
        let Some(before) = pre_curation.get(&id) else {
            continue; // absent from the pre-image: new node, created_at carries it
        };
        if curation_key(e) != *before {
            e.as_object_mut()
                .unwrap()
                .insert("touched_at".to_string(), Value::String(now_iso.clone()));
        }
    }

    // Node closure releases the node claim (x-94f8): a transition into a
    // terminal rung during THIS mutation is the one moment every closure path
    // shares. Ids are COLLECTED here; the release runs in the caller after
    // the lock drops. Only the CONFIGURED graph owns the global node-id space.
    let is_canonical = match &input.canonical_path {
        Some(cfg) => same_file(path, cfg),
        None => false,
    };
    let mut closure_releases = Vec::new();
    for e in entries.iter_mut() {
        let (Some(id), true) = (entry_id(e).map(str::to_string), is_dict(e)) else {
            continue;
        };
        let rung = e
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
        let pre_rung = status_normalized.get(&id).map(String::as_str);
        if !TERMINAL_RUNGS.contains(&rung.as_str())
            || pre_rung
                .map(|r| TERMINAL_RUNGS.contains(&r))
                .unwrap_or(false)
        {
            continue;
        }
        let obj = e.as_object_mut().unwrap();
        obj.insert("locked_by".to_string(), Value::Null);
        obj.insert("locked_at".to_string(), Value::Null);
        if is_canonical {
            closure_releases.push((id, rung));
        }
    }

    canonicalize_entries(&mut entries);

    let backup = create_backup(path);
    let body = serialize_graph_file(&entries);
    write_atomic(path, &body)?;
    write_sha256_sidecar(path)?;

    Ok(MutateOutcome {
        entries,
        dropped,
        backup: backup.map(|p| p.display().to_string()),
        closure_releases,
        is_canonical,
    })
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// Read path with defaults (read_graph / read_graph_strict, store-side)
// ---------------------------------------------------------------------------

/// Serialize the defaulted entries a read returns, byte-identical to the
/// Python leg's `json.dumps` of its own result (the differential parity
/// contract). Applies defaults; junk rows are kept only when `keep_malformed`
/// (load_graph's discovery caller needs them; ordinary reads filter).
///
/// The soft read keeps `_read_json`'s corrupt side effect: the unreadable
/// bytes are copied to a `.json.bak` sibling before the error surfaces, so
/// the recovery the store's messages promise actually exists on disk.
pub fn read_defaulted(path: &Path, keep_malformed: bool) -> Result<Vec<Value>, StoreError> {
    read_defaulted_opts(path, keep_malformed, true)
}

/// The strict variant takes `backup_on_corrupt = false`: read_graph_strict's
/// contract is that diagnosis is read-only and never writes a .bak.
pub fn read_defaulted_opts(
    path: &Path,
    keep_malformed: bool,
    backup_on_corrupt: bool,
) -> Result<Vec<Value>, StoreError> {
    match read_raw(path) {
        Ok(RawRead::Empty) => Ok(vec![]),
        Ok(RawRead::MalformedRoot) => {
            if backup_on_corrupt {
                // Soft read: a root with no entries key reads EMPTY, never an
                // error -- the malformed-root signal is reachable only through
                // the strict path, exactly as the Python soft reader answered.
                Ok(vec![])
            } else {
                Err(StoreError::MalformedRoot(path.display().to_string()))
            }
        }
        Ok(RawRead::Corrupt(reason)) => {
            if backup_on_corrupt {
                let backup = path.with_file_name(format!(
                    "{}.bak",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                ));
                // path.with_suffix(".json.bak") in Python; the file-name form
                // keeps "graph.json" -> "graph.json.bak" for the same effect.
                let _ = std::fs::copy(path, &backup);
            }
            Err(StoreError::Corrupt(reason))
        }
        Ok(RawRead::Entries(mut v)) => {
            apply_defaults(&mut v, keep_malformed);
            Ok(v)
        }
        Err(e) => Err(e),
    }
}

/// The working graph plus archived nodes, the working graph winning on id
/// (store.entries_with_archive). The archive's read failures degrade to the
/// working graph: the archive is advisory.
pub fn entries_with_archive(entries: &[Value], archive_path: &Path) -> Vec<Value> {
    if !archive_path.exists() {
        return entries.to_vec();
    }
    let archived = match read_defaulted(archive_path, false) {
        Ok(v) => v,
        Err(_) => return entries.to_vec(),
    };
    let live: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| is_dict(e))
        .filter_map(|e| entry_id(e).map(str::to_string))
        .collect();
    let mut out = entries.to_vec();
    for a in archived {
        if is_dict(&a) {
            let id = entry_id(&a).map(str::to_string);
            if let Some(id) = id {
                if !live.contains(&id) {
                    out.push(a);
                }
            }
        }
    }
    out
}

/// Normalize a plan_path for comparison (store.normalize_plan_path):
/// os.path.normpath's LEXICAL fold (dot segments collapsed, no symlink
/// resolution) plus trailing-separator strip, so abs/rel spellings of one
/// plan compare equal at every guard site.
pub fn normalize_plan_path(path: Option<&str>) -> Option<String> {
    let path = path?;
    if path.is_empty() {
        return None;
    }
    let absolute = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if !stack.is_empty() && *stack.last().unwrap() != ".." {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
                // An absolute path popping past root stays at root, like
                // normpath.
            }
            seg => stack.push(seg),
        }
    }
    let joined = stack.join("/");
    let mut s = if absolute {
        format!("/{joined}")
    } else {
        joined
    };
    if s.is_empty() {
        s = ".".to_string();
    }
    Some(s)
}

/// The id of another node already bound to the same plan_path
/// (store.plan_path_owner_conflict).
pub fn plan_path_owner_conflict(
    entries: &[Value],
    node_id: Option<&str>,
    plan_path: Option<&str>,
) -> Option<String> {
    let new_norm = normalize_plan_path(plan_path)?;
    for e in entries {
        if !is_dict(e) {
            continue;
        }
        let Some(eid) = entry_id(e) else {
            continue;
        };
        if Some(eid) == node_id {
            continue;
        }
        if normalize_plan_path(s_str(e, "plan_path")).as_deref() == Some(new_norm.as_str()) {
            return Some(eid.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_json_matches_reference_shapes() {
        // Byte-compat with json.dumps(indent=2, ensure_ascii=True), verified
        // against fixtures the differential parity corpus also drives through
        // the Python leg.
        let v = json!({"entries": [{"id": "ab-1", "n": 2, "ok": true, "none": null,
            "nested": {"a": [1, 2]}, "uni": "h\u{e9}llo", "emoji": "\u{1f600}"}]});
        let out = to_python_json(&v);
        assert!(out.contains("\"uni\": \"h\\u00e9llo\""), "{out}");
        assert!(out.contains("emoji\": \"\\ud83d\\ude00\""), "{out}");
        assert!(out.starts_with("{\n  \"entries\": [\n    {\n      \"id\": \"ab-1\","));
    }

    #[test]
    fn empty_containers_stay_inline_like_python() {
        let v = json!({"a": [], "b": {}});
        assert_eq!(to_python_json(&v), "{\n  \"a\": [],\n  \"b\": {}\n}");
    }

    #[test]
    fn presence_type_refuses_the_measured_wipe() {
        // The 2026-09-02 defect: `--details ""` wiped 3,036 characters. The
        // type cannot express it.
        assert!(TextField::parse("details", "").is_err());
        assert!(TextField::parse("details", "   ").is_err());
        assert!(FieldUpdate::from_cli("details", Some("")).is_err());
        assert!(matches!(
            FieldUpdate::from_cli("details", Some("real content")),
            Ok(FieldUpdate::Set(_))
        ));
        assert!(matches!(
            FieldUpdate::from_cli("details", None),
            Ok(FieldUpdate::Keep)
        ));
    }

    #[test]
    fn commit_refuses_an_empty_presence_field_even_from_a_raw_mutator() {
        // The Python mutator runs client-side against plain dicts, so the
        // store boundary enforces what the typed update path cannot see: a
        // hand-built payload carrying details:"" never persists.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let err = locked_mutate(
            &graph,
            MutateInput {
                entries: vec![json!({"id": "ab-1", "title": "t", "details": ""})],
                canonical_path: None,
                base_version: None,
                plan_rungs: None,
            },
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::EmptyFieldUpdate(_)), "{err}");
        // Whitespace-only is the same wipe shape.
        let err = locked_mutate(
            &graph,
            MutateInput {
                entries: vec![json!({"id": "ab-1", "completion_note": "   "})],
                canonical_path: None,
                base_version: None,
                plan_rungs: None,
            },
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::EmptyFieldUpdate(_)), "{err}");
        // Removing the key stays legal (the explicit clear).
        locked_mutate(
            &graph,
            MutateInput {
                entries: vec![json!({"id": "ab-1", "title": "t"})],
                canonical_path: None,
                base_version: None,
                plan_rungs: None,
            },
            Duration::from_secs(2),
        )
        .unwrap();
    }

    #[test]
    fn lock_timeout_surfaces_inside_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let _holder = BoundedLock::acquire(&graph, Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let err = BoundedLock::acquire(&graph, Duration::from_millis(150)).unwrap_err();
        assert!(matches!(err, StoreError::LockTimeout(_, _)));
        assert!(started.elapsed() < Duration::from_secs(2), "must not block");
    }

    #[test]
    fn defaults_apply_and_reorder_nothing() {
        let mut entries = vec![json!({
            "id": "ab-1", "_status": "claimed", "priority": "high"
        })];
        apply_defaults(&mut entries, false);
        let e = &entries[0];
        assert_eq!(s_str(e, "status"), Some("in_progress"));
        assert_eq!(s_str(e, "priority"), Some("p1"));
        assert_eq!(s_str(e, "parent"), None);
        assert!(e.get("children").unwrap().is_array());
    }

    #[test]
    fn recompute_derives_the_ladder_and_the_rollup() {
        let mut entries = vec![
            json!({"id": "parent", "parent": null}),
            json!({"id": "kid", "parent": "parent", "completed_at": "2026-01-01T00:00:00+00:00"}),
        ];
        recompute_statuses(&mut entries);
        // Parent with all-done children and no live work of its own -> done.
        assert_eq!(s_str(&entries[0], "status"), Some("done"));
    }

    #[test]
    fn plan_rungs_arrive_from_the_client_and_derive() {
        // Repo law keeps plan-document reading on the Python side, so the
        // store consumes the rungs the client computed with the rung table.
        let mut entries = vec![json!({"id": "n", "plan_path": "plan.md"})];
        let rungs = BTreeMap::from([("n".to_string(), "design".to_string())]);
        recompute_statuses_with_plan_rungs(&mut entries, Some(&rungs));
        assert_eq!(s_str(&entries[0], "status"), Some("design"));
        // A node the map does not name reads as rung "none".
        let mut entries2 = vec![json!({"id": "m", "plan_path": "plan.md"})];
        recompute_statuses_with_plan_rungs(&mut entries2, Some(&rungs));
        assert_eq!(s_str(&entries2[0], "status"), Some("idea"));
        // NO map at all: stored statuses stay (a caller not re-deriving).
        let mut entries3 = vec![json!({"id": "n", "plan_path": "plan.md", "status": "ready"})];
        recompute_statuses_with_plan_rungs(&mut entries3, None);
        assert_eq!(s_str(&entries3[0], "status"), Some("ready"));
    }

    #[test]
    fn canonicalize_reorders_status_forward_and_keeps_extras() {
        let mut entries = vec![json!({"zebra": 1, "id": "ab-1", "title": "t", "priority": "p1"})];
        canonicalize_entries(&mut entries);
        let obj = entries[0].as_object().unwrap();
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(keys[0], "id");
        assert!(keys.contains(&&"zebra".to_string()), "unknown keys kept");
        assert!(
            keys.iter().position(|k| **k == *"zebra").unwrap()
                > keys.iter().position(|k| **k == *"title").unwrap()
        );
    }

    #[test]
    fn slug_assignment_is_unique_and_idempotent() {
        let mut entries = vec![
            json!({"id": "ab-1", "title": "Port the graph store"}),
            json!({"id": "ab-2", "title": "Port the graph store"}),
            json!({"id": "ab-3", "title": "Port the graph store", "slug": "kept"}),
        ];
        ensure_slugs(&mut entries);
        let first = s_str(&entries[0], "slug").unwrap().to_string();
        let second = s_str(&entries[1], "slug").unwrap().to_string();
        assert_ne!(first, second);
        assert!(second.starts_with(&first));
        assert_eq!(s_str(&entries[2], "slug"), Some("kept"));
        ensure_slugs(&mut entries);
        assert_eq!(s_str(&entries[0], "slug"), Some(first.as_str()));
    }

    #[test]
    fn mutate_pipeline_publishes_bytes_the_python_shape_produces() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let entries = vec![json!({"id": "ab-1", "title": "t"})];
        let out = locked_mutate(
            &graph,
            MutateInput {
                entries,
                canonical_path: None,
                base_version: None,
                plan_rungs: None,
            },
            Duration::from_secs(2),
        )
        .unwrap();
        let body = std::fs::read_to_string(&graph).unwrap();
        assert!(body.starts_with("{\n  \"entries\": [\n    {"));
        assert!(body.ends_with("\n"));
        assert!(
            graph.with_extension("json.sha256").exists()
                || PathBuf::from(format!("{}.sha256", graph.display())).exists()
        );
        assert!(out.dropped == 0);
    }

    #[test]
    fn junction_preserve_order_keeps_canonical_insertion_order() {
        // Byte-parity depends on insertion order surviving the parse/serialize
        // round trip.
        let v: Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        assert_eq!(to_python_json(&v), "{\n  \"b\": 1,\n  \"a\": 2\n}");
    }

    #[test]
    fn python_nonfinite_floats_defuse_to_null_and_strings_survive() {
        // json.dumps(float("inf")) writes the bare token; the old Python
        // store read it and its rank math treated non-finite as unranked.
        // The port reads it as null (unranked) instead of refusing the file.
        let raw = "{\"entries\": [{\"rank\": Infinity, \"neg\": -Infinity, \"nan\": NaN, \
                   \"keep\": \"Infinity and NaN stay\", \"esc\": \"escaped \\\"Infinity\\\"\"}]}";
        let defused = defuse_nonfinite(raw);
        let v: Value = serde_json::from_str(&defused).expect("defused output parses");
        let e = &v["entries"][0];
        assert!(e["rank"].is_null());
        assert!(e["neg"].is_null());
        assert!(e["nan"].is_null());
        assert_eq!(e["keep"], "Infinity and NaN stay");
        assert_eq!(e["esc"], "escaped \"Infinity\"");
        // A full file with a poisoned rank reads Entries, never Corrupt.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, raw).unwrap();
        assert!(matches!(read_raw(&graph), Ok(RawRead::Entries(_))));
    }

    fn readiness_fixture(entries: Vec<Value>) -> std::collections::HashMap<String, Value> {
        index_by_id(&entries)
    }

    #[test]
    fn readiness_releases_a_dependent_whose_blocker_is_superseded_by_a_done_node() {
        // x-eb0e shape: the named blocker will never carry completed_at; only
        // its live successor decides the edge.
        let entries = vec![
            json!({"id": "ab-1", "blocked_by": ["ab-2"]}),
            json!({"id": "ab-2", "superseded_by": "ab-3"}),
            json!({"id": "ab-3", "completed_at": "2026-09-01T00:00:00Z"}),
        ];
        let by_id = readiness_fixture(entries);
        let a = json!({"id": "ab-1", "blocked_by": ["ab-2"]});
        assert_eq!(compute_readiness(&a, &by_id), ("ready".to_string(), None));
    }

    #[test]
    fn readiness_names_the_live_successor_of_a_superseded_blocker() {
        let entries = vec![
            json!({"id": "ab-1", "blocked_by": ["ab-2"]}),
            json!({"id": "ab-2", "superseded_by": "ab-3"}),
            json!({"id": "ab-3"}),
        ];
        let by_id = readiness_fixture(entries);
        let a = json!({"id": "ab-1", "blocked_by": ["ab-2"]});
        assert_eq!(
            compute_readiness(&a, &by_id),
            ("blocked-by".to_string(), Some("ab-3".to_string()))
        );
    }

    #[test]
    fn readiness_marks_a_deferred_blocker_and_never_loops_a_cycle() {
        let by_id = readiness_fixture(vec![
            json!({"id": "ab-1", "blocked_by": ["ab-2"]}),
            json!({"id": "ab-2", "deferred_at": "2026-08-01T00:00:00Z"}),
            // A ring of nine rows, each superseding into the next: the chase
            // hits the hop bound and answers unknown-dep, never loops.
            json!({"id": "ab-4", "blocked_by": ["ab-c1"]}),
            json!({"id": "ab-c1", "superseded_by": "ab-c2"}),
            json!({"id": "ab-c2", "superseded_by": "ab-c3"}),
            json!({"id": "ab-c3", "superseded_by": "ab-c4"}),
            json!({"id": "ab-c4", "superseded_by": "ab-c5"}),
            json!({"id": "ab-c5", "superseded_by": "ab-c6"}),
            json!({"id": "ab-c6", "superseded_by": "ab-c7"}),
            json!({"id": "ab-c7", "superseded_by": "ab-c8"}),
            json!({"id": "ab-c8", "superseded_by": "ab-c9"}),
            json!({"id": "ab-c9", "superseded_by": "ab-c1"}),
        ]);
        let a = json!({"id": "ab-1", "blocked_by": ["ab-2"]});
        assert_eq!(
            compute_readiness(&a, &by_id),
            ("blocked-by-deferred".to_string(), Some("ab-2".to_string()))
        );
        let cycle = json!({"id": "ab-4", "blocked_by": ["ab-c1"]});
        assert_eq!(
            compute_readiness(&cycle, &by_id),
            ("unknown-dep".to_string(), Some("ab-c9".to_string()))
        );
    }

    #[test]
    fn settle_prunes_rewires_and_holds_blocked_by_edges() {
        let entries = vec![
            json!({"id": "ab-1", "blocked_by": ["ab-done"]}),
            json!({"id": "ab-done", "completed_at": "2026-09-01T00:00:00Z"}),
            json!({"id": "ab-2", "blocked_by": ["ab-old"]}),
            json!({"id": "ab-old", "superseded_by": "ab-new"}),
            json!({"id": "ab-new"}),
            json!({"id": "ab-3", "blocked_by": ["ab-old2"]}),
            json!({"id": "ab-old2", "superseded_by": "ab-done2"}),
            json!({"id": "ab-done2", "completed_at": "2026-09-01T00:00:00Z"}),
            json!({"id": "ab-4", "blocked_by": ["ab-def"]}),
            json!({"id": "ab-def", "deferred_at": "2026-08-01T00:00:00Z"}),
            // A record-less superseded row is itself closed: never settled.
            json!({"id": "ab-9", "superseded_by": "ab-new", "blocked_by": ["ab-done"]}),
        ];
        let (entries, receipts, _) = settle_blocked_by_edges(entries);
        let by_id = index_by_id(&entries);
        let edge = |id: &str| {
            by_id
                .get(id)
                .map(|e| e.get("blocked_by").cloned())
                .unwrap_or(None)
        };
        assert_eq!(edge("ab-1"), Some(json!([])));
        assert_eq!(edge("ab-2"), Some(json!(["ab-new"])));
        assert_eq!(edge("ab-3"), Some(json!([])));
        assert_eq!(edge("ab-4"), Some(json!(["ab-def"])));
        // ab-9 is closed (superseded, no record): its edge is untouched.
        assert_eq!(edge("ab-9"), Some(json!(["ab-done"])));
        let kinds: Vec<&str> = receipts
            .iter()
            .filter_map(|r| r.get("kind").and_then(Value::as_str))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "blocked_by_pruned",
                "blocked_by_rewired",
                "blocked_by_pruned",
                "blocked_by_held",
            ]
        );
        let pruned2 = receipts
            .iter()
            .find(|r| r.get("blocker").and_then(Value::as_str) == Some("ab-old2"))
            .unwrap();
        assert!(pruned2["reason"]
            .as_str()
            .unwrap()
            .contains("superseded by ab-done2"));
    }

    #[test]
    fn settle_is_idempotent_over_a_settled_graph() {
        let entries = vec![
            json!({"id": "ab-1", "blocked_by": ["ab-2"]}),
            json!({"id": "ab-2", "superseded_by": "ab-3"}),
            json!({"id": "ab-3"}),
        ];
        let (entries, receipts, _) = settle_blocked_by_edges(entries);
        assert_eq!(receipts.len(), 1);
        let (entries2, receipts2, _) = settle_blocked_by_edges(entries);
        assert!(receipts2.is_empty(), "{receipts2:?}");
        let by_id = index_by_id(&entries2);
        assert_eq!(
            by_id.get("ab-1").unwrap().get("blocked_by").cloned(),
            Some(json!(["ab-3"]))
        );
    }
}
