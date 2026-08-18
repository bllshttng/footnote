//! King escalation: the one thing the in-session king arm shells out for.
//!
//! Module name starts with "loop" so its lines count toward the control-plane
//! LOC-ratchet glob `crates/fno-agents/src/loop*`, the same reason
//! `loop_runtime.rs` and `loop_target.rs` are named that way.
//!
//! ## Why there is no walk arm here
//!
//! A cross-session `KingQueue` lived here and was cut before it shipped. It had
//! no working path: `run_loop`'s resume guard closed its unit without
//! dispatching, because the unit's `session_key` and the king's own termination
//! event shared the manifest `fno_id`. On the rare path where it did dispatch,
//! `CONTINUE_PROMPT` is hardcoded to `/target --resume` for every driver and
//! `Unit.extra_env` is read by nothing, so the spawned session was a target
//! resume that had no idea it was a king.
//!
//! Under that sat a larger hole. Nothing crowns a king: the `king-for-a-day`
//! skill never calls `fno king init`, nothing deletes the manifest when a king
//! dies, and a spawn can transfer a crown with no verb to return it. Respawning
//! needs crown, respawn and expire to exist together. That is a lifecycle, and
//! it belongs to one node rather than to a queue bolted onto this one.

use std::path::Path;
use std::process::{Command, Stdio};

/// Tell the operator the king stopped with work still pending.
///
/// Called from every `NoProgress` terminal in `king_decide`, via that
/// function's shared `terminate` closure, so a terminal added later is covered
/// without anyone remembering to wire it. Returns the one-line outcome to
/// record, never an error: a failed escalation is named and moves on, since
/// blocking the terminal on it leaves the king stopped with nobody told either
/// way.
pub(crate) fn escalate_stalled(fno_bin: &str, cwd: &Path, ids: &[String], reason: &str) -> String {
    let output = Command::new(fno_bin)
        .args([
            "king",
            "escalate",
            "--stalled",
            &ids.join(","),
            "--reason",
            reason,
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let qid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            format!("escalated to the operator as {qid}")
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim().chars().take(300).collect::<String>();
            eprintln!("king: escalation failed: {detail}");
            format!("escalation FAILED: {detail}")
        }
        Err(e) => {
            eprintln!("king: cannot run {fno_bin} king escalate: {e}");
            format!("escalation FAILED: cannot run {fno_bin} king escalate: {e}")
        }
    }
}
