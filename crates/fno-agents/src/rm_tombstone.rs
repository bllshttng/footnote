//! The rm tombstone: one JSON file beside the registry naming the sessions
//! `fno agents rm` removed, so the harness-store healer refuses to adopt a
//! removed session back under a fresh short-id name (x-976b: rm dropped the
//! row, a resolve healed the same codex session, and the adopted duplicate
//! blocked resume).
//!
//! One producer (the Rust daemon's rm), one consumer (the Python healer in
//! `cli/src/fno/agents/store_fallback.py`, reached through the daemon's
//! heal-token shellout). The grace window is duplicated across the seam by
//! design: the file is the contract, not a shared constant. Expired entries
//! are pruned on write; a failed tombstone write is an event, never a
//! refused removal.

use crate::daemon::now_epoch_secs;
use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::path::PathBuf;

/// How long a removal keeps blocking store re-adoption. Long enough to cover
/// the re-adoption window the incident showed (62s) with room for a working
/// day; short enough that a deliberate later adoption is one wait away.
pub(crate) const GRACE_SECS: i64 = 86_400;

const FILENAME: &str = "rm_tombstones.json";

pub(crate) fn path(home: &AgentsHome) -> PathBuf {
    home.root().join(FILENAME)
}

/// Stamp one removed session. Best-effort: the caller decides whether a
/// failure is an event or a refusal; rm never refuses a completed removal
/// because the tombstone write failed.
pub(crate) fn record(home: &AgentsHome, harness: &str, session_id: &str) -> Result<(), String> {
    record_at(home, harness, session_id, now_epoch_secs())
}

pub(crate) fn record_at(
    home: &AgentsHome,
    harness: &str,
    session_id: &str,
    now: i64,
) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Ok(());
    }
    let file = path(home);
    let mut entries: Vec<Value> = match std::fs::read_to_string(&file) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| Vec::new()),
        Err(_) => Vec::new(),
    };
    entries.retain(|row| {
        row.get("removed_at")
            .and_then(Value::as_i64)
            .is_some_and(|at| at + GRACE_SECS > now)
    });
    entries.push(json!({
        "harness": harness,
        "session_id": session_id,
        "removed_at": now,
    }));
    // Atomic rename so the Python reader never sees a torn write.
    let tmp = file.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string(&entries).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &file).map_err(|e| e.to_string())
}

/// True when rm removed this (harness, session) inside the grace window.
/// Symmetry reader for tests and any future Rust-side door; the production
/// consumer is the Python healer.
pub(crate) fn recent(home: &AgentsHome, harness: &str, session_id: &str) -> bool {
    let raw = match std::fs::read_to_string(path(home)) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    let now = now_epoch_secs();
    let entries: Vec<Value> = match serde_json::from_str(&raw) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    entries.iter().any(|row| {
        row.get("harness").and_then(Value::as_str) == Some(harness)
            && row.get("session_id").and_then(Value::as_str) == Some(session_id)
            && row
                .get("removed_at")
                .and_then(Value::as_i64)
                .is_some_and(|at| now.saturating_sub(at) <= GRACE_SECS)
    })
}
