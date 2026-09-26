//! `fno-agents hook` - the per-turn hooks as native entries.
//!
//! The two shell hooks (Stop, PreToolUse) become 20-line exec wrappers of
//! these entries: the policy they carried in shell + Python + jq moves here,
//! so a fire costs one process instead of five CLI startups plus interpreter
//! spins (measured 9.3s of a 9.9s court trace at `hooks/king-delegation-guard.sh`
//! before the port). Transport, not client verbs: dispatched in `main()`
//! before the tokio runtime builds, never in `run`, so the verb-surface
//! ratchet never sees them (shrink law d-fe66560a).

pub mod edit_integrity;
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
        Some("edit-integrity") => edit_integrity::run(&args[1..]),
        Some("king-guard") => king_guard::run(&args[1..]),
        Some("pipe-guard") => pipe_guard::run(&args[1..]),
        Some("prompt") => prompt::run(&args[1..]),
        Some("test-run-guard") => test_run_guard::run(&args[1..]),
        Some("stop") => stop::run(&args[1..]),
        other => {
            eprintln!(
                "fno-agents hook: unknown entry {other:?}; expected edit-integrity, king-guard, pipe-guard, prompt, test-run-guard or stop"
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

/// One control-plane arm row for this fire. The row carries the fire's
/// session id: `emit_tick` writes one row per SPACE with no session key of
/// its own, so a reader of `fno agents status` sees only the space's newest
/// fire and cannot tell a king's own row from a neighbor's - the misread
/// that once sent a drain-reserve fix chasing a driver=target
/// misclassification for days.
pub(crate) fn global_events_path(fallback: &Path) -> PathBuf {
    std::env::var_os("GLOBAL_EVENTS_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            crate::paths::AgentsHome::from_env_opt()
                .map(|home| crate::daemon::global_events_path(&home))
        })
        .unwrap_or_else(|| fallback.to_path_buf())
}

pub(crate) fn emit_tick(cwd: &Path, decision: &str, reason: &str, driver: &str, session: &str) {
    let project_events = crate::paths::events_path(cwd);
    let global_events = global_events_path(&project_events);
    // A missing space root must drop nothing: this row is the one record
    // that a fire ran, and the append below is best-effort, so the target
    // dirs are created here rather than trusted to exist.
    for events in [&project_events, &global_events] {
        if let Some(parent) = events.parent() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let mut detail = format!(
        "driver={driver} decision={decision} reason={}",
        if reason.is_empty() { "live" } else { reason }
    );
    if !session.is_empty() {
        let short: String = session.chars().take(8).collect();
        detail.push_str(&format!(" session={short}"));
    }
    let data = serde_json::json!({
        "arm": "stop_hook",
        "scheduler": "hook:target-stop-hook",
        "acted": 1,
        "skip_reason": serde_json::Value::Null,
        "detail": detail,
        "interval_s": 0,
    });
    crate::loopcheck::emit_to_both(&project_events, &global_events, "control_plane_tick", data);
}
