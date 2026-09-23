//! `fno-agents hook` - the per-turn hooks as native entries.
//!
//! The two shell hooks (Stop, PreToolUse) become 20-line exec wrappers of
//! these entries: the policy they carried in shell + Python + jq moves here,
//! so a fire costs one process instead of five CLI startups plus interpreter
//! spins (measured 9.3s of a 9.9s court trace at `hooks/king-delegation-guard.sh`
//! before the port). Transport, not client verbs: dispatched in `main()`
//! before the tokio runtime builds, never in `run`, so the verb-surface
//! ratchet never sees them (shrink law d-fe66560a).

pub mod king_guard;
pub mod pipe_guard;
pub mod prompt;
pub mod stop;
pub mod test_run_guard;

use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Dispatch one hook entry by name. Transport, not a client verb: `main()`
/// calls this before the runtime builds, so a fire costs one process.
pub fn dispatch(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("king-guard") => king_guard::run(&args[1..]),
        Some("pipe-guard") => pipe_guard::run(&args[1..]),
        Some("prompt") => prompt::run(&args[1..]),
        Some("test-run-guard") => test_run_guard::run(&args[1..]),
        Some("stop") => stop::run(&args[1..]),
        other => {
            eprintln!(
                "fno-agents hook: unknown entry {other:?}; expected king-guard, pipe-guard, prompt, test-run-guard or stop"
            );
            2
        }
    }
}

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

/// One `guard_decision` row into the space events file, the bounded appender
/// `emit_to_both` uses, shared by every native guard so the rows stay
/// byte-identical (as `hooks/lib/guard-mark.sh` did).
pub(crate) fn emit_guard_decision(cwd: &Path, guard: &str, tool: &str, denied: bool) {
    let path = crate::paths::events_path(cwd);
    let event = json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "type": "guard_decision",
        "data": {"guard": guard, "decision": if denied { "block" } else { "allow" }, "tool": tool},
        "source": "hook"
    });
    let _ = crate::claims::append_event_line(&path, &event, std::time::Duration::from_secs(2));
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
