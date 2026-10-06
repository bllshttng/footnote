//! The wake-name tombstone: one JSON file beside the registry mapping a
//! stopped worker's harness session uuid to the registry name it last held.
//! `fno agents stop` keeps the row (Exited + the stop record), but the
//! retirement sweep's later drop deletes it, and a wake that arrives after
//! the drop used to fall back to `wake-<handle>` - a name the board and mail
//! never knew. The stop stamps the name here first, so every wake after a
//! stop revives under the old name. One producer (the stop seams, plus the
//! rm tombstone stamp), one consumer (`reentry::wake_spawn_name_beside`).
//! No expiry: a name is the worker-to-node join for the uuid's lifetime;
//! the per-uuid dedupe plus the record cap bound the file.

use crate::daemon::now_epoch_secs;
use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Records kept after a write trims. One per distinct stopped worker uuid;
/// a fleet week is a few hundred, so the cap never trims live history.
const CAP: usize = 512;

const FILENAME: &str = "wake_names.json";

fn path(home: &AgentsHome) -> PathBuf {
    home.root().join(FILENAME)
}

/// Stamp `entry`'s uuid -> name pair. Best-effort by contract: a failed
/// write costs a later wake its name, never the stop itself (the stop arms
/// ignore the result, the same stance the rm tombstone takes).
pub(crate) fn record(home: &AgentsHome, session_id: &str, name: &str) {
    let _ = record_at(&path(home), session_id, name, now_epoch_secs());
}

pub(crate) fn record_at(file: &Path, session_id: &str, name: &str, now: i64) -> Result<(), String> {
    if session_id.trim().is_empty() || name.trim().is_empty() {
        return Ok(());
    }
    crate::rm_tombstone::update_json_array(file, |mut entries| {
        entries.retain(|row| row.get("session_id").and_then(Value::as_str) != Some(session_id));
        entries.push(json!({ "session_id": session_id, "name": name, "at": now }));
        while entries.len() > CAP {
            entries.remove(0);
        }
        entries
    })
}

/// The tombstoned name `session_id` held when its row stopped, read from the
/// file beside `registry_path` (the daemon resolves the pair together, the
/// same way the rm tombstone lookup does). A missing or unreadable file
/// answers None: the caller's alias fallback applies.
pub(crate) fn lookup_beside(registry_path: &Path, session_id: &str) -> Option<String> {
    let file = registry_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILENAME);
    let raw = std::fs::read_to_string(file).ok()?;
    let entries: Vec<Value> = serde_json::from_str(&raw).ok()?;
    entries.into_iter().rev().find_map(|row| {
        let id = row.get("session_id").and_then(Value::as_str)?;
        if id != session_id {
            return None;
        }
        let name = row.get("name").and_then(Value::as_str)?;
        if name.is_empty() {
            return None;
        }
        Some(name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let reg = dir.path().join("registry.json");
        (dir, reg)
    }

    /// AC-HP: stop then wake keeps the name - the node's repro at the store
    /// level, with a re-stop rewriting the record.
    #[test]
    fn record_then_lookup_roundtrips_and_dedupes() {
        let (_dir, reg) = staged();
        let file = reg.parent().unwrap().join(FILENAME);
        record_at(&file, "aaaabbbb-cccc-dddd-eeee-ffff00001111", "t-old", 1).unwrap();
        record_at(&file, "aaaabbbb-cccc-dddd-eeee-ffff00001111", "t-new", 2).unwrap();
        let name = lookup_beside(&reg, "aaaabbbb-cccc-dddd-eeee-ffff00001111").unwrap();
        assert_eq!(name, "t-new", "the last stop's name wins");
        let raw = std::fs::read_to_string(&file).unwrap();
        let rows: Vec<Value> = serde_json::from_str(&raw).unwrap();
        assert_eq!(rows.len(), 1, "the uuid holds one record");
    }

    /// AC-ERR: an unnamed uuid, an empty name and a missing file all answer
    /// None, so the caller's alias fallback applies.
    #[test]
    fn empty_inputs_and_missing_file_answer_none() {
        let (_dir, reg) = staged();
        assert!(lookup_beside(&reg, "aaaabbbb-cccc-dddd-eeee-ffff00001111").is_none());
        let file = reg.parent().unwrap().join(FILENAME);
        record_at(&file, "", "t", 1).unwrap();
        record_at(&file, "aaaabbbb-cccc-dddd-eeee-ffff00001111", "  ", 1).unwrap();
        assert!(
            lookup_beside(&reg, "aaaabbbb-cccc-dddd-eeee-ffff00001111").is_none(),
            "nothing was stamped"
        );
    }

    /// The cap trims oldest-first so the file stays bounded.
    #[test]
    fn the_cap_trims_oldest_first() {
        let (_dir, reg) = staged();
        let file = reg.parent().unwrap().join(FILENAME);
        for i in 0..CAP + 10 {
            let uuid = format!("aaaabbbb-cccc-dddd-eeee-ffff{i:012}");
            record_at(&file, &uuid, &format!("t-{i}"), i as i64).unwrap();
        }
        let raw = std::fs::read_to_string(&file).unwrap();
        let rows: Vec<Value> = serde_json::from_str(&raw).unwrap();
        assert_eq!(rows.len(), CAP);
        assert!(
            lookup_beside(&reg, "aaaabbbb-cccc-dddd-eeee-ffff000000000000").is_none(),
            "the oldest record trimmed"
        );
        assert!(
            lookup_beside(&reg, "aaaabbbb-cccc-dddd-eeee-ffff000000000019").is_some(),
            "a recent record survives"
        );
    }
}
