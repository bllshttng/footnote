//! The cancel sentinel: is this run asked to stop?
//!
//! Two drivers, two sentinel families: a `/target` run reads the tombstone
//! then the sentinel under the project's `.fno/`; the king reads only the
//! `cancelled` twin of its own state file, so cancelling one lane never
//! answers for the other.

use chrono::{DateTime, Utc};
use std::path::Path;

pub(crate) fn check_cancel_sentinel(
    cwd: &Path,
    state_path: &Path,
    created_at: &Option<String>,
    driver: &str,
) -> bool {
    let target_sentinel = cwd.join(".fno/.target-cancelled");
    let target_tombstone = cwd.join(".fno/.target-cancelled-final");
    let king_sentinel = state_path.with_extension("cancelled");
    let paths: Vec<&Path> = if driver == "king" {
        vec![king_sentinel.as_path()]
    } else {
        vec![target_tombstone.as_path(), target_sentinel.as_path()]
    };

    for path in paths {
        if !path.exists() {
            continue;
        }
        // Check mtime >= created_at
        if let Some(ca) = created_at {
            if let Ok(parsed_ca) = ca.parse::<DateTime<Utc>>() {
                if let Ok(meta) = std::fs::metadata(path) {
                    if let Ok(modified) = meta.modified() {
                        let sentinel_time: DateTime<Utc> = modified.into();
                        if sentinel_time >= parsed_ca {
                            return true;
                        }
                        // Stale sentinel (older than created_at) -> ignore
                        continue;
                    }
                }
            }
            // Can't read mtime -> treat as present (fail-closed)
            return true;
        }
        return true;
    }
    false
}
