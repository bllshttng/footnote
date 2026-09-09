//! The claude sessions store, read as indexes: the socket index the
//! liveness ladder's socket rung pays one walk for, and the single-scan
//! liveness reads built on it.

use crate::claude_ask::ClaudeHome;
use crate::client_verbs::{row_liveness_with_indexed, RowLiveness};
use crate::truth_probe::family1_truth_state;
use serde_json::Value;

/// Silence on every rung is `Unknown`. The ladder NEVER returns `Dead`: only
/// a positive death proof may, and absence never is one. A missing or
/// unreadable transcript falls through the third rung and lands `Unknown`.
///
/// Resume keeps its own inline copy of rungs 1 and 3 because it must also
/// read the truth VALUE (`done`/`stalled` route the relaunch arm, which the
/// verdict type has no room for); its behavior is pinned identical by tests.
pub fn row_liveness(entry: &crate::state::RegistryEntry, claude_home: &ClaudeHome) -> RowLiveness {
    let sockets = sessions_socket_index(claude_home);
    row_liveness_indexed(entry, &sockets)
}

/// One scan of claude's sessions dir: `jobId -> messagingSocketPath` for
/// every live-shaped bg session file, first-sorted-wins (the same pick
/// `locate_session` makes). The socket rung's cost is this walk, so a sweep
/// probing N rows pays it once, not N times.
pub fn sessions_socket_index(
    claude_home: &ClaudeHome,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let dir = claude_home.sessions_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut paths: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        // The dir holds the bg records this index wants plus the session
        // transcript markdown (measured: 11k files, 99.6% of the walk's
        // bytes in .md). The records are `.json`; skip everything else
        // before reading.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) != Some("bg") {
            continue;
        }
        let (Some(job), Some(sock)) = (
            v.get("jobId").and_then(Value::as_str),
            v.get("messagingSocketPath").and_then(Value::as_str),
        ) else {
            continue;
        };
        if job.is_empty() || sock.is_empty() || out.contains_key(job) {
            continue;
        }
        out.insert(job.to_string(), sock.to_string());
    }
    out
}

/// [`row_liveness`] against a prebuilt [`sessions_socket_index`] - the form
/// a sweep uses, so N probed rows cost one dir scan. The truth read is the
/// production one.
pub(crate) fn row_liveness_indexed(
    entry: &crate::state::RegistryEntry,
    sockets: &std::collections::HashMap<String, String>,
) -> RowLiveness {
    row_liveness_with_indexed(entry, sockets, None, family1_truth_state)
}
