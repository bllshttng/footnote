//! Permanent node-prose history journal (x-920a wave 1).
//!
//! One append-only JSONL journal derived from the canonical graph path: the
//! sibling directory `<graph-file-name>.history/notes feed keyed to the graph file, not to
//! any session or event stream. Lifetime registered in
//! `docs/state-root-inventory.md`.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Why a record entered the journal. The reason is data, not prose: readers
/// page and filter on it.
pub const REASON_STATE_REPLACED: &str = "state_replaced";
pub const REASON_STATE_CLEARED: &str = "state_cleared";
pub const REASON_NOTE_MIGRATED: &str = "note_migrated";
pub const REASON_TERMINAL_EVACUATED: &str = "terminal_evacuated";
pub const REASON_MACHINE_RECORD: &str = "machine_record";
pub const REASON_DETAILS_ARCHIVED: &str = "details_archived";

/// One journal record. `original` is the EXACT pre-image value (the prior
/// `current_state` object, or one legacy `progress_notes` row) - never a
/// re-serialization, so byte-level fidelity is the record's contract.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRecord {
    pub node_id: String,
    pub reason: String,
    /// Revision the pre-image carried; absent when the node had no state.
    pub prior_revision: Option<u64>,
    /// Source position of the original inside its array (migration keys on
    /// node + position, so identical bodies at different positions stay
    /// distinct records).
    pub position: Option<u64>,
    original: Value,
    pub source_session_id: Option<String>,
    pub source_harness: Option<String>,
    pub content_hash: String,
}

impl HistoryRecord {
    /// The exact original, read-only.
    pub fn original(&self) -> &Value {
        &self.original
    }

    /// The record as one journal line (no trailing newline).
    pub fn to_line(&self) -> Value {
        json!({
            "node_id": self.node_id,
            "reason": self.reason,
            "prior_revision": self.prior_revision,
            "position": self.position,
            "original": self.original,
            "source_session_id": self.source_session_id,
            "source_harness": self.source_harness,
            "content_hash": self.content_hash,
        })
    }
}

/// The one path function serving writers and readers: `<graph>.history/notes.jsonl`
/// next to the graph file. Derived from the graph path, never a home-directory
/// literal.
pub fn history_path(graph: &Path) -> PathBuf {
    let file_name = graph
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "graph.json".to_string());
    graph
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file_name}.history"))
        .join("notes.jsonl")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Canonical bytes of `original`: compact serde_json serialization. The hash
/// binds the record to the exact stored bytes.
fn canonical_bytes(original: &Value) -> Vec<u8> {
    serde_json::to_vec(original).unwrap_or_default()
}

fn record_from_line(line: &str) -> Option<HistoryRecord> {
    let v: Value = serde_json::from_str(line).ok()?;
    Some(HistoryRecord {
        node_id: v.get("node_id")?.as_str()?.to_string(),
        reason: v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        prior_revision: v.get("prior_revision").and_then(Value::as_u64),
        position: v.get("position").and_then(Value::as_u64),
        original: v.get("original").cloned().unwrap_or(Value::Null),
        source_session_id: v
            .get("source_session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        source_harness: v
            .get("source_harness")
            .and_then(Value::as_str)
            .map(str::to_string),
        content_hash: v
            .get("content_hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// A logical identity: same node, same reason, same prior revision, same
/// source position and same original bytes is the SAME record. Re-running a
/// migration, or retrying a state write whose publication failed after the
/// journal landed, must not multiply logical records; the identity scan is
/// the dedupe.
fn logical_identity(
    node_id: &str,
    reason: &str,
    prior_revision: Option<u64>,
    position: Option<u64>,
    hash: &str,
) -> String {
    format!(
        "{node_id}\u{1f}{reason}\u{1f}{}\u{1f}{}\u{1f}{hash}",
        prior_revision
            .map(|r| r.to_string())
            .unwrap_or_else(|| "-".into()),
        position
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into()),
    )
}

fn scan_identities(path: &Path) -> std::collections::HashSet<String> {
    let mut seen = std::collections::HashSet::new();
    let Ok(file) = std::fs::File::open(path) else {
        return seen;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Some(rec) = record_from_line(&line) else {
            continue;
        };
        seen.insert(logical_identity(
            &rec.node_id,
            &rec.reason,
            rec.prior_revision,
            rec.position,
            &rec.content_hash,
        ));
    }
    seen
}

/// Append one record: dedupe by logical identity, write, flush, fsync, then
/// read the line back from disk and verify it before returning. A journal
/// that cannot prove its own bytes is an error, and the state writer treats
/// it as a refusal.
pub fn append(
    graph: &Path,
    node_id: &str,
    reason: &str,
    prior_revision: Option<u64>,
    position: Option<u64>,
    original: &Value,
    source_session_id: Option<&str>,
    source_harness: Option<&str>,
) -> Result<HistoryRecord, String> {
    let content_hash = sha256_hex(&canonical_bytes(original));
    let identity = logical_identity(node_id, reason, prior_revision, position, &content_hash);
    let path = history_path(graph);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("history mkdir: {e}"))?;
    }
    if scan_identities(&path).contains(&identity) {
        // Already journaled: return the existing record unchanged.
        let file = std::fs::File::open(&path).map_err(|e| format!("history read: {e}"))?;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(rec) = record_from_line(&line) {
                if logical_identity(
                    &rec.node_id,
                    &rec.reason,
                    rec.prior_revision,
                    rec.position,
                    &rec.content_hash,
                ) == identity
                {
                    return Ok(rec);
                }
            }
        }
        return Err("history dedupe hit vanished between scan and read".to_string());
    }

    let record = HistoryRecord {
        node_id: node_id.to_string(),
        reason: reason.to_string(),
        prior_revision,
        position,
        original: original.clone(),
        source_session_id: source_session_id.map(str::to_string),
        source_harness: source_harness.map(str::to_string),
        content_hash,
    };
    let mut line =
        serde_json::to_string(&record.to_line()).map_err(|e| format!("history serialize: {e}"))?;
    line.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("history open: {e}"))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("history write: {e}"))?;
    file.flush().map_err(|e| format!("history flush: {e}"))?;
    file.sync_all().map_err(|e| format!("history sync: {e}"))?;

    // Read back and verify the exact bytes before the caller may publish.
    let file = std::fs::File::open(&path).map_err(|e| format!("history readback: {e}"))?;
    let wanted = line.trim_end();
    let matched = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .any(|l| l == wanted);
    if !matched {
        return Err(format!(
            "history readback missing the record just written to {}",
            path.display()
        ));
    }
    Ok(record)
}

/// Paged readback: `(records, total)`. `node_id` filters to one node; `None`
/// reads the whole journal. Explicit and paged by contract - history is
/// never silently loaded into a dispatch prompt.
pub fn read(
    graph: &Path,
    node_id: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Value>, usize), String> {
    let path = history_path(graph);
    let file = std::fs::File::open(&path).map_err(|e| format!("history read: {e}"))?;
    let mut all: Vec<Value> = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Some(rec) = record_from_line(&line) else {
            continue; // a torn trailing line is skipped, never fatal for readers
        };
        if node_id.map(|id| rec.node_id == id).unwrap_or(true) {
            all.push(rec.to_line());
        }
    }
    let total = all.len();
    let end = (offset + limit).min(total);
    let page = if offset >= total {
        Vec::new()
    } else {
        all[offset..end].to_vec()
    };
    Ok((page, total))
}

/// Number of records for one node (or all nodes when `node_id` is `None`).
pub fn count(graph: &Path, node_id: Option<&str>) -> usize {
    read(graph, node_id, 0, usize::MAX)
        .map(|(_, total)| total)
        .unwrap_or(0)
}
