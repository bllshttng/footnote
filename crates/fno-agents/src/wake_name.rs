//! The wake-name tombstone: one JSON file beside the registry mapping a
//! stopped worker's harness session uuid to the registry name it last held.
//! `fno agents stop` keeps the row (Exited + the stop record), but the
//! retirement sweep's later drop deletes it, and a wake that arrived after
//! such a drop used to fall back to `wake-<handle>` - a name the board and
//! mail never knew. Every registry-row drop stamps the name here (the stop
//! seams, the rm tombstone stamp, and the retirement sweep's
//! `commit_retirements`), and a wake that finds neither row nor tombstone
//! reads the transcript's own naming records before the alias
//! (`lookup_transcript`). One consumer (`reentry::wake_spawn_name`).
//! No expiry: a name is the worker-to-node join for the uuid's lifetime;
//! the per-uuid dedupe plus the record cap bound the file.

use crate::claude_ask::ClaudeHome;
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

/// The last spawn-safe name this uuid's transcript carries: the newest
/// `custom-title` (customTitle) or `agent-name` (agentName) record whose
/// value is non-empty, not this session's own `wake-<handle>` alias, and
/// spawn-name shaped. The rung between the tombstone and the alias: a row
/// dropped before the tombstone existed still revives under the name its
/// transcript shows. An unreadable store answers None - best-effort by the
/// module contract.
pub(crate) fn lookup_transcript(claude_home: &ClaudeHome, session_id: &str) -> Option<String> {
    let alias = format!("wake-{}", crate::identity::canonical_handle(session_id));
    for projects in claude_home.project_dirs() {
        let Some(path) = crate::claude_transcript_paths::resolve_transcript(&projects, session_id)
        else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut name = None;
        for line in std::io::BufRead::lines(std::io::BufReader::new(file)).map_while(Result::ok) {
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let value = match record.get("type").and_then(Value::as_str) {
                Some("custom-title") => record.get("customTitle").and_then(Value::as_str),
                Some("agent-name") => record.get("agentName").and_then(Value::as_str),
                _ => None,
            };
            let Some(value) = value
                .map(str::trim)
                .filter(|v| !v.is_empty() && *v != alias)
            else {
                continue;
            };
            // A spawn name only: the fallback's answer must be spawnable.
            if !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                continue;
            }
            name = Some(value.to_string());
        }
        if name.is_some() {
            return name;
        }
    }
    None
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
        // An empty uuid or name is refused at the write, so the alias
        // fallback answers instead of a blank record.
        record_at(&file, "", "t", 3).unwrap();
        record_at(&file, "aaaabbbb-cccc-dddd-eeee-ffff00001111", "  ", 3).unwrap();
        let raw = std::fs::read_to_string(&file).unwrap();
        let rows: Vec<Value> = serde_json::from_str(&raw).unwrap();
        assert_eq!(rows.len(), 1, "the uuid holds one record");
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

    /// The transcript rung answers the last non-wake customTitle or
    /// agentName: the store that lost both the row and the tombstone still
    /// revives under the name the transcript shows.
    #[test]
    fn transcript_rung_answers_the_last_non_wake_name() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude").join("projects").join("work");
        std::fs::create_dir_all(&projects).unwrap();
        let uuid = "7ada6c8a-1111-2222-3333-444444444444";
        std::fs::write(
            projects.join(format!("{uuid}.jsonl")),
            concat!(
                "{\"type\":\"custom-title\",\"customTitle\":\"t-old-name\"}\n",
                "{\"type\":\"user\",\"message\":{}}\n",
                "{\"type\":\"agent-name\",\"agentName\":\"wake-7ada6c8a\"}\n",
                "{\"type\":\"agent-name\",\"agentName\":\"t-rename-later\"}\n",
            ),
        )
        .unwrap();
        let claude_home = ClaudeHome::at(home.path());
        assert_eq!(
            lookup_transcript(&claude_home, uuid).as_deref(),
            Some("t-rename-later"),
            "the last non-wake record wins; the wake alias is skipped",
        );
    }

    /// A title that cannot spawn (spaces, blank) never reaches the answer,
    /// and a store with no transcript answers None for the alias fallback.
    #[test]
    fn transcript_rung_skips_unspawnable_titles_and_missing_stores() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude").join("projects").join("work");
        std::fs::create_dir_all(&projects).unwrap();
        let uuid = "7ada6c8a-1111-2222-3333-444444444444";
        std::fs::write(
            projects.join(format!("{uuid}.jsonl")),
            concat!(
                "{\"type\":\"custom-title\",\"customTitle\":\"not a spawn name\"}\n",
                "{\"type\":\"custom-title\",\"customTitle\":\"  \"}\n",
                "{\"type\":\"summary\",\"summary\":\"noise\"}\n",
            ),
        )
        .unwrap();
        let claude_home = ClaudeHome::at(home.path());
        assert_eq!(lookup_transcript(&claude_home, uuid), None);
        assert_eq!(
            lookup_transcript(&ClaudeHome::at(home.path().join("none")), uuid),
            None,
            "no transcript anywhere: None, the alias fallback applies"
        );
    }
}
