//! `fno-agents hook` - the per-turn hooks as native entries (x-09d2).
//!
//! The two shell hooks (Stop, PreToolUse) become 20-line exec wrappers of
//! these entries: the policy they carried in shell + Python + jq moves here,
//! so a fire costs one process instead of five CLI startups plus interpreter
//! spins (measured 9.3s of a 9.9s court trace at `hooks/king-delegation-guard.sh`
//! before the port). Transport, not client verbs: dispatched in `main()`
//! before the tokio runtime builds, never in `run`, so the verb-surface
//! ratchet never sees them (shrink law d-fe66560a).

pub mod king_guard;
pub mod stop;

use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The resolved events path's parent: the space the hooks key their counters,
/// king manifests and journals off. `FNO_EVENTS_PATH` wins exactly as it does
/// for `fno-agents state path events`, so a pinned test sees a pinned space.
pub(crate) fn events_space(cwd: &std::path::Path) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_EVENTS_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(v)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::paths::space_dir(cwd));
    }
    crate::paths::space_dir(cwd)
}

/// Slurp stdin. A read failure answers the empty string; both hooks treat
/// empty as allow (an unreadable payload is not a refusal).
pub(crate) fn read_stdin() -> String {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    input
}

/// PreToolUse allow: hook result via stdout JSON at exit 0.
pub(crate) fn emit_allow() -> i32 {
    println!("{{}}");
    0
}

/// PreToolUse deny: the exact shape the shell's `_block` printed through jq.
pub(crate) fn emit_block(reason: &str) -> i32 {
    let out = json!({
        "decision": "block",
        "reason": reason,
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    });
    println!("{out}");
    0
}
