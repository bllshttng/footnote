//! The per-id sidecar file reader, the Rust twin of `fno.tracker.sidecar.load`
//! external mode.
//!
//! The writer is the Python pydantic `Sidecar` with `extra="forbid"`, so the
//! partition is enforced where the file is written; this reader passes every
//! key through except `id` and keeps no second field list. `SIDECAR_KEYS`
//! exists for the graph backend, which stores sidecar fields in the row and
//! must project only the sidecar-side names. Both lists answer for readers;
//! the inventory (dual-implementation-inventory.md) carries the dual row.

use crate::claims::encode_key;
use crate::tracker::TrackerError;
use serde_json::Value;
use std::path::Path;

/// The Python `Sidecar.model_fields` minus `id`: the sidecar-side names, used
/// by the graph backend's `sidecar()` projection. A new Sidecar field must be
/// added here for graph mode to serve it.
pub(crate) const SIDECAR_KEYS: &[&str] = &[
    "cwd",
    "plan_path",
    "pr_number",
    "pr_url",
    "additional_prs",
    "cost_usd",
    "cost_sessions",
    "claimed_at",
    "batch",
    "contained_in",
    "sessions",
    "source_session_id",
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "source_inbox_msg",
    "spawned_by_session",
    "spawned_by_harness",
    "spawned_by_cwd",
];

/// `~/.fno/sidecar` (or the configured state root's `sidecar` dir).
pub(crate) fn root(cwd: &Path) -> std::path::PathBuf {
    crate::agents_config::state_dir(cwd)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("sidecar")
}

/// Read one per-id sidecar file. A missing file is an empty map; a parse or
/// read failure is `Backend` naming the id. Every key passes through except
/// `id` - no second field list here.
pub fn load(root: &Path, id: &str) -> Result<serde_json::Map<String, Value>, TrackerError> {
    use super::TrackerError;
    let path = root.join(format!("{}.json", encode_key(id)));
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| TrackerError::Backend(format!("sidecar unreadable for {id}: {e}")))?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| TrackerError::Backend(format!("sidecar unreadable for {id}: {e}")))?;
    let mut out = serde_json::Map::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k != "id" {
                out.insert(k.clone(), val.clone());
            }
        }
    }
    Ok(out)
}
