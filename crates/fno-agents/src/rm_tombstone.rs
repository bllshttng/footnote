//! The rm tombstone: one JSON file beside the registry naming the sessions
//! `fno agents rm` removed, so the harness-store healer refuses to adopt a
//! removed session back under a fresh short-id name (rm dropped the row, a
//! resolve healed the same codex session, and the adopted duplicate blocked
//! resume).
//!
//! One producer (the Rust daemon's rm), one consumer (the Python healer in
//! `cli/src/fno/agents/store_fallback.py`, reached through the daemon's
//! heal-token shellout). The grace window is duplicated across the seam by
//! design: the file is the contract, not a shared constant. Expired entries
//! are pruned on write; a failed write surfaces on the rm receipt, never a
//! refused removal. The row's transport short id, name and cwd ride
//! along on each record, so the stop verb resolves a just-removed
//! session straight from the tombstone without the adopting heal whose
//! grace window refuses exactly this session.

use crate::daemon::now_epoch_secs;
use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How long a removal keeps blocking store re-adoption. Long enough to cover
/// the re-adoption window the incident showed (62s) with room for a working
/// day; short enough that a deliberate later adoption is one wait away.
pub(crate) const GRACE_SECS: i64 = 86_400;

const FILENAME: &str = "rm_tombstones.json";

pub(crate) fn path(home: &AgentsHome) -> PathBuf {
    home.root().join(FILENAME)
}

/// One flock-protected JSON-array sidecar write: read (or seed an empty
/// array), hand the rows to `f`, then atomic-rename so a concurrent reader
/// never sees a torn write. The shared write protocol behind this file's
/// tombstone and the wake-name tombstone beside it.
pub(crate) fn update_json_array(
    file: &Path,
    f: impl FnOnce(Vec<Value>) -> Vec<Value>,
) -> Result<(), String> {
    let _lock = crate::state::acquire_exclusive(&crate::state::lock_path(file))
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<Value> = match std::fs::read_to_string(file) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| Vec::new()),
        Err(_) => Vec::new(),
    };
    entries = f(entries);
    // Atomic rename so the Python reader never sees a torn write; the pid
    // suffix keeps a crashed writer's leftover from colliding with this one.
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_string(&entries).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, file).map_err(|e| e.to_string())
}

/// Stamp one removed session. Best-effort: the caller decides whether a
/// failure is an event or a refusal; rm never refuses a completed removal
/// because the tombstone write failed.
pub(crate) fn record(
    home: &AgentsHome,
    harness: &str,
    session_id: &str,
    short: &str,
    name: &str,
    cwd: &str,
    host_mode: &str,
) -> Result<(), String> {
    record_at(
        home,
        harness,
        session_id,
        short,
        name,
        cwd,
        host_mode,
        now_epoch_secs(),
    )
}

pub(crate) fn record_at(
    home: &AgentsHome,
    harness: &str,
    session_id: &str,
    short: &str,
    name: &str,
    cwd: &str,
    host_mode: &str,
    now: i64,
) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Ok(());
    }
    update_json_array(&path(home), |mut entries| {
        entries.retain(|row| {
            row.get("removed_at")
                .and_then(Value::as_i64)
                .is_some_and(|at| now.saturating_sub(at) <= GRACE_SECS)
        });
        entries.push(json!({
            "harness": harness,
            "session_id": session_id,
            "short": short,
            "name": name,
            "cwd": cwd,
            "host_mode": host_mode,
            "removed_at": now,
        }));
        entries
    })
}

/// A removed session a stop resolved from the tombstone: every field the
/// lifecycle arms need to reach the live thread (the codex re-attach rides
/// the session id + cwd; the claude stop rides the short id).
pub(crate) struct RemovedSession {
    pub harness: String,
    pub session_id: String,
    pub short: String,
    pub name: String,
    pub cwd: String,
    pub host_mode: String,
}

/// The in-window removal `token` names, matched against the row's own
/// transport short id and the full session id. `registry_path` is the
/// registry file the tombstone sits beside (the daemon resolves the pair
/// together, the same way the Python healer does).
pub(crate) fn lookup_removed_beside(registry_path: &Path, token: &str) -> Option<RemovedSession> {
    let file = registry_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILENAME);
    let raw = std::fs::read_to_string(&file).ok()?;
    let entries: Vec<Value> = serde_json::from_str(&raw).ok()?;
    let now = now_epoch_secs();
    entries.into_iter().rev().find_map(|row| {
        let in_window = row
            .get("removed_at")
            .and_then(Value::as_i64)
            .is_some_and(|at| now.saturating_sub(at) <= GRACE_SECS);
        if !in_window {
            return None;
        }
        let session_id = row
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let short = row.get("short").and_then(Value::as_str).unwrap_or_default();
        if session_id != token && (short.is_empty() || short != token) {
            return None;
        }
        Some(RemovedSession {
            harness: row
                .get("harness")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            session_id: session_id.to_string(),
            short: short.to_string(),
            name: row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            cwd: row
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            host_mode: row
                .get("host_mode")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staged tombstone beside a fake registry, with its dir: the dir must
    /// outlive the lookup, so it is returned.
    fn staged(rows: Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join(FILENAME), serde_json::to_string(&rows).unwrap()).unwrap();
        (dir, agents.join("registry.json"))
    }

    #[test]
    fn lookup_matches_short_and_full_id_inside_the_window() {
        let (_dir, reg) = staged(json!([
            {"harness": "codex", "session_id": "0198abcd-0000-0000-0000-000000000001",
             "short": "f4e3d2c1", "name": "t-worker", "cwd": "/repo/one",
             "removed_at": now_epoch_secs()}
        ]));
        let found = lookup_removed_beside(&reg, "f4e3d2c1").expect("short match");
        assert_eq!(
            found.session_id, "0198abcd-0000-0000-0000-000000000001",
            "the short match answers the full session id"
        );
        let found = lookup_removed_beside(&reg, "0198abcd-0000-0000-0000-000000000001")
            .expect("full id match");
        assert_eq!(found.cwd, "/repo/one");
        assert!(lookup_removed_beside(&reg, "nosuch").is_none());
    }

    #[test]
    fn expired_rows_and_missing_files_answer_none() {
        let (_dir, reg) = staged(json!([
            {"harness": "codex", "session_id": "0198ffff-0000-0000-0000-000000000002",
             "short": "aaaa1111", "name": "t", "cwd": "/repo", "removed_at": 1}
        ]));
        assert!(
            lookup_removed_beside(&reg, "aaaa1111").is_none(),
            "an expired record never resolves"
        );
        let missing = std::env::temp_dir()
            .join("nonexistent-rm-tombstones")
            .join("registry.json");
        assert!(lookup_removed_beside(&missing, "aaaa1111").is_none());
    }
}
